use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::ProcessorConfig;
use crate::db::emails::get_email_by_id;
use crate::db::events::{Event, get_event_by_id};
use crate::db::jobs;
use crate::processor::registry::ProcessorRegistry;

const RETRY_SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Receives events from the EventLoop and dispatches them to processors.
/// Also periodically sweeps for failed jobs that are ready to retry.
pub struct JobScheduler {
    pool: PgPool,
    registry: Arc<ProcessorRegistry>,
    event_rx: mpsc::Receiver<Vec<Event>>,
    token: CancellationToken,
    processor_configs: HashMap<String, ProcessorConfig>,
}

impl JobScheduler {
    pub fn new(
        pool: PgPool,
        registry: Arc<ProcessorRegistry>,
        event_rx: mpsc::Receiver<Vec<Event>>,
        token: CancellationToken,
        processor_configs: Vec<ProcessorConfig>,
    ) -> Self {
        let configs = processor_configs
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();
        Self {
            pool,
            registry,
            event_rx,
            token,
            processor_configs: configs,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        info!("job scheduler starting");

        let mut retry_interval = tokio::time::interval(RETRY_SWEEP_INTERVAL);

        loop {
            tokio::select! {
                _ = self.token.cancelled() => {
                    info!("job scheduler shutting down");
                    return Ok(());
                }

                Some(events) = self.event_rx.recv() => {
                    self.process_events(events).await;
                }

                _ = retry_interval.tick() => {
                    self.retry_sweep().await;
                }
            }
        }
    }

    async fn process_events(&self, events: Vec<Event>) {
        for event in events {
            let processors = self.registry.processors_for_event(&event.event_type);
            if processors.is_empty() {
                debug!(
                    event_id = event.id,
                    event_type = event.event_type,
                    "no processors for event type"
                );
                continue;
            }

            let email = if let Some(email_id) = event.email_id {
                match get_email_by_id(&self.pool, email_id).await {
                    Ok(e) => e,
                    Err(err) => {
                        warn!(
                            event_id = event.id,
                            email_id,
                            error = %err,
                            "failed to load email for event"
                        );
                        None
                    }
                }
            } else {
                None
            };

            for processor in processors {
                let processor_name = processor.name().to_string();
                let timeout_secs = self
                    .processor_configs
                    .get(&processor_name)
                    .map(|c| c.timeout_secs)
                    .unwrap_or(30);

                let job_id = match jobs::create_job(&self.pool, event.id, &processor_name).await {
                    Ok(id) => id,
                    Err(e) => {
                        debug!(
                            event_id = event.id,
                            processor = processor_name,
                            error = %e,
                            "job already exists or create failed"
                        );
                        continue;
                    }
                };

                self.execute_job(job_id, &processor_name, &event, email.as_ref(), timeout_secs)
                    .await;
            }
        }
    }

    async fn execute_job(
        &self,
        job_id: i64,
        processor_name: &str,
        event: &Event,
        email: Option<&crate::db::emails::EmailRecord>,
        timeout_secs: u64,
    ) {
        if let Err(e) =
            jobs::update_job_status(&self.pool, job_id, "in_progress", None, None).await
        {
            error!(job_id, error = %e, "failed to update job status to in_progress");
            return;
        }

        let processor = match self.registry.processors_for_event(&event.event_type)
            .into_iter()
            .find(|p| p.name() == processor_name)
        {
            Some(p) => p,
            None => return,
        };

        let timeout = Duration::from_secs(timeout_secs);
        let result = tokio::time::timeout(timeout, processor.process(event, email)).await;

        match result {
            Ok(Ok(output)) if output.success => {
                debug!(job_id, processor = processor_name, event_id = event.id, "processor completed");
                let _ = jobs::update_job_status(&self.pool, job_id, "completed", None, None).await;
                crate::metrics::inc_processor_runs(processor_name, "success");
            }
            Ok(Ok(output)) => {
                let msg = output.message.unwrap_or_default();
                self.handle_failure(job_id, processor_name, &msg).await;
                crate::metrics::inc_processor_runs(processor_name, "failure");
            }
            Ok(Err(e)) => {
                self.handle_failure(job_id, processor_name, &e.to_string()).await;
                crate::metrics::inc_processor_runs(processor_name, "error");
            }
            Err(_) => {
                self.handle_failure(job_id, processor_name, "execution timed out").await;
                crate::metrics::inc_processor_runs(processor_name, "timeout");
            }
        }
    }

    async fn handle_failure(&self, job_id: i64, processor_name: &str, error_msg: &str) {
        let config = self.processor_configs.get(processor_name);
        let max_retries = config.map(|c| c.max_retries).unwrap_or(0);
        let backoff_secs = config.map(|c| &c.retry_backoff_secs[..]).unwrap_or(&[]);

        // Get current attempt count
        let current_job = jobs::get_pending_jobs(&self.pool, processor_name, 1).await;
        let attempts = match current_job {
            Ok(jobs_list) => jobs_list.first().map(|j| j.attempts).unwrap_or(0),
            Err(_) => 0,
        };

        if max_retries == 0 || attempts as u32 >= max_retries {
            warn!(
                job_id,
                processor = processor_name,
                error = error_msg,
                "processor failed, marking as abandoned (max retries exceeded)"
            );
            let _ = jobs::update_job_status(&self.pool, job_id, "abandoned", Some(error_msg), None)
                .await;
        } else {
            // Calculate next retry time from backoff schedule
            let backoff_idx = (attempts as usize).min(backoff_secs.len().saturating_sub(1));
            let delay_secs = backoff_secs.get(backoff_idx).copied().unwrap_or(60);
            let next_retry = chrono::Utc::now() + chrono::Duration::seconds(delay_secs as i64);

            warn!(
                job_id,
                processor = processor_name,
                error = error_msg,
                attempts,
                next_retry_secs = delay_secs,
                "processor failed, scheduling retry"
            );
            let _ = jobs::update_job_status(
                &self.pool,
                job_id,
                "failed",
                Some(error_msg),
                Some(next_retry),
            )
            .await;
        }
    }

    /// Periodically sweep for failed jobs that are ready to retry.
    async fn retry_sweep(&self) {
        let retryable = match jobs::get_retryable_jobs(&self.pool, 50).await {
            Ok(jobs) => jobs,
            Err(e) => {
                debug!(error = %e, "failed to fetch retryable jobs");
                return;
            }
        };

        if retryable.is_empty() {
            return;
        }

        debug!(count = retryable.len(), "found retryable jobs");

        for job in retryable {
            let event = match get_event_by_id(&self.pool, job.event_id).await {
                Ok(Some(e)) => e,
                Ok(None) => {
                    warn!(job_id = job.id, event_id = job.event_id, "event not found for retry");
                    continue;
                }
                Err(e) => {
                    warn!(job_id = job.id, error = %e, "failed to load event for retry");
                    continue;
                }
            };

            let email = if let Some(email_id) = event.email_id {
                get_email_by_id(&self.pool, email_id).await.ok().flatten()
            } else {
                None
            };

            let timeout_secs = self
                .processor_configs
                .get(&job.processor_name)
                .map(|c| c.timeout_secs)
                .unwrap_or(30);

            self.execute_job(job.id, &job.processor_name, &event, email.as_ref(), timeout_secs)
                .await;
        }
    }
}
