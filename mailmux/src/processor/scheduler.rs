use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use sqlx::PgPool;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::ProcessorConfig;
use crate::db::emails::get_email_by_id;
use crate::db::events::{Event, get_event_by_id};
use crate::db::jobs;
use crate::processor::registry::ProcessorRegistry;

const MAX_INFLIGHT_BATCHES: usize = 16;

const RETRY_SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Receives events from the EventLoop and dispatches them to processors.
/// Also periodically sweeps for failed jobs that are ready to retry.
pub struct JobScheduler {
    pool: PgPool,
    registry: Arc<ProcessorRegistry>,
    event_rx: mpsc::Receiver<Vec<Event>>,
    token: CancellationToken,
    processor_configs: HashMap<String, ProcessorConfig>,
    processor_limits: HashMap<String, Arc<Semaphore>>,
    #[cfg(test)]
    before_claim: Option<(
        tokio::sync::mpsc::UnboundedSender<()>,
        Arc<tokio::sync::Barrier>,
    )>,
}

impl JobScheduler {
    pub fn new(
        pool: PgPool,
        registry: Arc<ProcessorRegistry>,
        event_rx: mpsc::Receiver<Vec<Event>>,
        token: CancellationToken,
        processor_configs: Vec<ProcessorConfig>,
    ) -> Self {
        let limits = processor_configs
            .iter()
            .map(|config| {
                (
                    config.name.clone(),
                    Arc::new(Semaphore::new(config.concurrency.max(1) as usize)),
                )
            })
            .collect();
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
            processor_limits: limits,
            #[cfg(test)]
            before_claim: None,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        info!("job scheduler starting");
        let mut event_rx = std::mem::replace(&mut self.event_rx, mpsc::channel(1).1);
        let scheduler = Arc::new(self);
        let mut running = JoinSet::new();
        let sweep_limit = Arc::new(Semaphore::new(1));
        let mut retry_interval = tokio::time::interval(RETRY_SWEEP_INTERVAL);
        loop {
            tokio::select! {
                _ = scheduler.token.cancelled() => {
                    info!("job scheduler draining active work");
                    while let Some(result) = running.join_next().await {
                        if let Err(error) = result { error!(%error, "scheduler worker failed during shutdown"); }
                    }
                    return Ok(());
                }
                Some(events) = event_rx.recv(), if running.len() < MAX_INFLIGHT_BATCHES => {
                    let scheduler = scheduler.clone();
                    running.spawn(async move { scheduler.process_events(events).await });
                }
                _ = retry_interval.tick(), if running.len() < MAX_INFLIGHT_BATCHES => {
                    if let Ok(permit) = sweep_limit.clone().try_acquire_owned() {
                        let scheduler = scheduler.clone();
                        running.spawn(async move {
                            let _permit = permit;
                            scheduler.retry_sweep().await;
                        });
                    }
                }
                Some(result) = running.join_next(), if !running.is_empty() => {
                    if let Err(error) = result { error!(%error, "scheduler worker failed"); }
                }
            }
        }
    }

    async fn process_events(&self, events: Vec<Event>) {
        let mut executions = Vec::new();
        for event in events {
            let processors = self.registry.processors_for_event(&event);
            if processors.is_empty() {
                debug!(
                    event_id = event.id,
                    event_type = event.event_type,
                    "no processors eligible for event"
                );
            }

            // Register the entire dispatch atomically, including zero-match
            // events, so they cannot block later events in polling batches.
            // old event from cleanup between processor executions.
            let processor_names: Vec<_> = processors.iter().map(|p| p.name()).collect();
            let mut new_jobs: HashMap<_, _> = match jobs::register_event_dispatch(
                &self.pool,
                event.id,
                &processor_names,
            )
            .await
            {
                Ok(jobs) => jobs.into_iter().map(|(id, name)| (name, id)).collect(),
                Err(e) => {
                    error!(event_id = event.id, error = %e, "failed to register processor jobs");
                    continue;
                }
            };

            if new_jobs.is_empty() {
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
                let Some(job_id) = new_jobs.remove(&processor_name) else {
                    // Duplicate dispatch from the NOTIFY + poll overlap: only
                    // execute jobs newly registered by this dispatch.
                    continue;
                };
                let timeout_secs = self
                    .processor_configs
                    .get(&processor_name)
                    .map(|c| c.timeout_secs)
                    .unwrap_or(30);

                executions.push((
                    job_id,
                    processor_name,
                    event.clone(),
                    email.clone(),
                    timeout_secs,
                ));
            }
        }
        futures::stream::iter(executions)
            .for_each_concurrent(
                None,
                |(job_id, name, event, email, timeout_secs)| async move {
                    self.execute_job(job_id, &name, &event, email.as_ref(), timeout_secs)
                        .await;
                },
            )
            .await;
    }

    async fn execute_job(
        &self,
        job_id: i64,
        processor_name: &str,
        event: &Event,
        email: Option<&crate::db::emails::EmailRecord>,
        timeout_secs: u64,
    ) {
        let processor = match self
            .registry
            .require_processor_for_event(processor_name, event)
        {
            Ok(processor) => processor,
            Err(error) => {
                if let Err(db_error) =
                    jobs::abandon_queued_job(&self.pool, job_id, &error.to_string()).await
                {
                    error!(job_id, error = %db_error, "failed to abandon ineligible queued job");
                }
                return;
            }
        };
        #[cfg(test)]
        if let Some((arrived, barrier)) = &self.before_claim {
            let _ = arrived.send(());
            barrier.wait().await;
        }
        let Some(limit) = self.processor_limits.get(processor_name) else {
            error!(
                processor = processor_name,
                "missing processor concurrency configuration"
            );
            return;
        };
        // Hold the same permit across dispatch and retry execution. Claim only
        // after acquiring capacity so queued jobs remain recoverable in the DB.
        let _permit = tokio::select! {
            permit = limit.acquire() => match permit { Ok(permit) => permit, Err(_) => return },
            _ = self.token.cancelled() => return,
        };
        let claim = match jobs::claim_job_with_lease(&self.pool, job_id, timeout_secs, false).await
        {
            Ok(Some(claim)) => claim,
            Ok(None) => return,
            Err(error) => {
                error!(job_id, error = %error, "failed to claim queued job");
                return;
            }
        };

        let timeout = Duration::from_secs(timeout_secs);
        let result = tokio::time::timeout(timeout, processor.process(event, email)).await;

        match result {
            Ok(Ok(output)) if output.success => {
                debug!(
                    job_id,
                    processor = processor_name,
                    event_id = event.id,
                    "processor completed"
                );

                // Serialize output before consuming any borrowed fields.
                let serialized = match serde_json::to_value(&output) {
                    Ok(v) => v,
                    Err(e) => {
                        error!(job_id, error = %e, "failed to serialize ProcessorOutput");
                        let _ = jobs::finish_job(&self.pool, &claim, "completed", None, None, None)
                            .await;
                        crate::metrics::inc_processor_runs(processor_name, "success");
                        return;
                    }
                };

                if let Err(e) = jobs::finish_job(
                    &self.pool,
                    &claim,
                    "completed",
                    None,
                    None,
                    Some(&serialized),
                )
                .await
                {
                    error!(job_id, error = %e, "failed to persist completed job output");
                }

                crate::metrics::inc_processor_runs(processor_name, "success");
                if !output.metrics.is_empty() {
                    crate::metrics::record_processor_metrics(processor_name, &output.metrics);
                }
            }
            Ok(Ok(output)) => {
                // success == false — persist the output alongside the failure state.
                let serialized = match serde_json::to_value(&output) {
                    Ok(v) => v,
                    Err(e) => {
                        error!(job_id, error = %e, "failed to serialize ProcessorOutput");
                        self.handle_failure(
                            &claim,
                            processor_name,
                            &output.message.unwrap_or_default(),
                            None,
                        )
                        .await;
                        crate::metrics::inc_processor_runs(processor_name, "failure");
                        return;
                    }
                };
                let msg = output.message.unwrap_or_default();
                self.handle_failure(&claim, processor_name, &msg, Some(&serialized))
                    .await;
                crate::metrics::inc_processor_runs(processor_name, "failure");
            }
            Ok(Err(e)) => {
                self.handle_failure(&claim, processor_name, &e.to_string(), None)
                    .await;
                crate::metrics::inc_processor_runs(processor_name, "error");
            }
            Err(_) => {
                self.handle_failure(&claim, processor_name, "execution timed out", None)
                    .await;
                crate::metrics::inc_processor_runs(processor_name, "timeout");
            }
        }
    }

    async fn handle_failure(
        &self,
        claim: &jobs::JobClaim,
        processor_name: &str,
        error_msg: &str,
        output: Option<&serde_json::Value>,
    ) {
        let job_id = claim.id;
        let config = self.processor_configs.get(processor_name);
        let max_retries = config.map(|c| c.max_retries).unwrap_or(0);
        let backoff_secs = config.map(|c| &c.retry_backoff_secs[..]).unwrap_or(&[]);

        // Get current attempt count for this specific job.
        let attempts = match jobs::get_job_by_id(&self.pool, job_id).await {
            Ok(Some(job)) => job.attempts,
            Ok(None) => {
                error!(job_id, "job not found when handling failure");
                return;
            }
            Err(e) => {
                error!(job_id, error = %e, "failed to fetch job when handling failure");
                return;
            }
        };

        if max_retries == 0 || attempts as u32 >= max_retries {
            warn!(
                job_id,
                processor = processor_name,
                error = error_msg,
                "processor failed, marking as abandoned (max retries exceeded)"
            );
            if let Err(e) = jobs::finish_job(
                &self.pool,
                claim,
                "abandoned",
                Some(error_msg),
                None,
                output,
            )
            .await
            {
                error!(job_id, error = %e, "failed to persist abandoned job state");
            }
        } else {
            // Map attempts → backoff schedule index. `attempts` is already 1 on the
            // first failure (incremented when transitioning to in_progress), so subtract
            // 1 to align the first failure with index 0. Clamp to the last entry so
            // that retries beyond the schedule length keep using the longest delay.
            let backoff_idx = (attempts as usize)
                .saturating_sub(1)
                .min(backoff_secs.len().saturating_sub(1));
            // Fall back to 60 s if retry_backoff_secs was left empty in config.
            let delay_secs = backoff_secs.get(backoff_idx).copied().unwrap_or(60);
            // Compute an absolute timestamp; the retry sweep compares next_retry_at
            // against now() to decide when to re-queue the job.
            let next_retry = chrono::Utc::now() + chrono::Duration::seconds(delay_secs as i64);

            warn!(
                job_id,
                processor = processor_name,
                error = error_msg,
                attempts,
                next_retry_secs = delay_secs,
                "processor failed, scheduling retry"
            );
            if let Err(e) = jobs::finish_job(
                &self.pool,
                claim,
                "failed",
                Some(error_msg),
                Some(next_retry),
                output,
            )
            .await
            {
                error!(job_id, error = %e, "failed to persist failed job state");
            }
        }
    }

    /// Periodically sweep for failed jobs that are ready to retry.
    pub(crate) async fn retry_sweep(&self) {
        match jobs::recover_expired_jobs(&self.pool, 50).await {
            Ok(count) if count > 0 => warn!(count, "recovered interrupted processor jobs"),
            Ok(_) => {}
            Err(error) => {
                error!(error = %error, "failed to recover interrupted processor jobs");
                return;
            }
        }
        let retryable = match jobs::get_runnable_jobs(&self.pool, 50).await {
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

        let mut executions = Vec::new();
        for job in retryable {
            let event = match get_event_by_id(&self.pool, job.event_id).await {
                Ok(Some(e)) => e,
                Ok(None) => {
                    warn!(
                        job_id = job.id,
                        event_id = job.event_id,
                        "event not found for retry"
                    );
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

            executions.push((job.id, job.processor_name, event, email, timeout_secs));
        }
        futures::stream::iter(executions)
            .for_each_concurrent(
                None,
                |(job_id, name, event, email, timeout_secs)| async move {
                    self.execute_job(job_id, &name, &event, email.as_ref(), timeout_secs)
                        .await;
                },
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::emails::EmailRecord;
    use crate::db::events::Event;
    use crate::processor::Processor;
    use crate::processor::ProcessorOutput;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingProcessor {
        name: String,
        events: Vec<String>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Processor for CountingProcessor {
        fn name(&self) -> &str {
            &self.name
        }
        fn subscribed_events(&self) -> &[String] {
            &self.events
        }
        async fn process(
            &self,
            _event: &Event,
            _email: Option<&EmailRecord>,
        ) -> Result<ProcessorOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ProcessorOutput {
                success: true,
                message: None,
                metadata: None,
                metrics: vec![],
            })
        }
    }

    /// A minimal test processor that returns a fixed ProcessorOutput.
    struct TestProcessor {
        name: String,
        events: Vec<String>,
        output: ProcessorOutput,
    }

    #[async_trait]
    impl Processor for TestProcessor {
        fn name(&self) -> &str {
            &self.name
        }

        fn subscribed_events(&self) -> &[String] {
            &self.events
        }

        async fn process(
            &self,
            _event: &Event,
            _email: Option<&EmailRecord>,
        ) -> Result<ProcessorOutput> {
            Ok(self.output.clone())
        }
    }

    struct CleanupBetweenProcessors {
        pool: PgPool,
        events: Vec<String>,
        terminal_status: &'static str,
        second_release: Arc<Semaphore>,
    }

    #[async_trait]
    impl Processor for CleanupBetweenProcessors {
        fn name(&self) -> &str {
            "first"
        }

        fn subscribed_events(&self) -> &[String] {
            &self.events
        }

        async fn process(
            &self,
            event: &Event,
            _email: Option<&EmailRecord>,
        ) -> Result<ProcessorOutput> {
            // Force the state cleanup would observe immediately after the
            // scheduler finishes this job, before the next processor runs.
            sqlx::query(
                "UPDATE processor_jobs SET status = $2 WHERE event_id = $1 AND processor_name = 'first'",
            )
            .bind(event.id)
            .bind(self.terminal_status)
            .execute(&self.pool)
            .await?;
            crate::housekeeping::cleanup_old_events(&self.pool, 30).await?;

            assert!(get_event_by_id(&self.pool, event.id).await?.is_some());
            let next_job = jobs::get_job_by_event_and_processor(&self.pool, event.id, "second")
                .await?
                .expect("the next job must already be registered");
            assert!(matches!(
                next_job.status.as_str(),
                "pending" | "in_progress"
            ));
            self.second_release.add_permits(1);

            Ok(ProcessorOutput {
                success: self.terminal_status == "completed",
                message: None,
                metadata: None,
                metrics: vec![],
            })
        }
    }

    async fn create_scheduler_test_event(pool: &PgPool) -> Result<i64> {
        Ok(sqlx::query_scalar("INSERT INTO events (event_type, account_id, mailbox_name, payload) VALUES ('email_arrived', 'test', 'INBOX', '{}'::jsonb) RETURNING id")
            .fetch_one(pool).await?)
    }

    fn test_processor_config(name: &str, max_retries: u32, backoff: Vec<u64>) -> ProcessorConfig {
        ProcessorConfig {
            name: name.into(),
            enabled: true,
            events: vec!["email_arrived".into()],
            sources: None,
            max_retries,
            retry_backoff_secs: backoff,
            timeout_secs: 30,
            concurrency: 1,
            config: HashMap::new(),
        }
    }

    struct GatedProcessor {
        name: String,
        events: Vec<String>,
        release: Option<Arc<Semaphore>>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        entered: tokio::sync::mpsc::UnboundedSender<String>,
    }

    #[async_trait]
    impl Processor for GatedProcessor {
        fn name(&self) -> &str {
            &self.name
        }
        fn subscribed_events(&self) -> &[String] {
            &self.events
        }
        async fn process(&self, _: &Event, _: Option<&EmailRecord>) -> Result<ProcessorOutput> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let _ = self.entered.send(self.name.clone());
            if let Some(release) = &self.release {
                release.acquire().await?.forget();
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ProcessorOutput {
                success: true,
                message: None,
                metadata: None,
                metrics: vec![],
            })
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn run_honors_shared_limits_and_fast_work_bypasses_slow_dispatch_and_retries(
        pool: PgPool,
    ) -> Result<()> {
        let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut configs = Vec::new();
        let mut processors: Vec<(Box<dyn Processor>, ProcessorConfig)> = Vec::new();
        for name in ["slow", "fast"] {
            let config = ProcessorConfig {
                concurrency: if name == "slow" { 2 } else { 1 },
                sources: Some(vec![crate::config::ProcessorSource {
                    account: name.into(),
                    mailboxes: None,
                }]),
                ..test_processor_config(name, 3, vec![0])
            };
            processors.push((
                Box::new(GatedProcessor {
                    name: name.into(),
                    events: vec!["email_arrived".into()],
                    release: if name == "slow" {
                        Some(release.clone())
                    } else {
                        None
                    },
                    active: if name == "slow" {
                        active.clone()
                    } else {
                        Arc::new(AtomicUsize::new(0))
                    },
                    peak: if name == "slow" {
                        peak.clone()
                    } else {
                        Arc::new(AtomicUsize::new(0))
                    },
                    entered: entered.clone(),
                }),
                config.clone(),
            ));
            configs.push(config);
        }
        let mut slow_events = Vec::new();
        for index in 0..3 {
            let id: i64 = sqlx::query_scalar("INSERT INTO events(event_type,account_id,mailbox_name) VALUES ('email_arrived','slow','INBOX') RETURNING id").fetch_one(&pool).await?;
            if index == 0 {
                let job = jobs::create_job(&pool, id, "slow").await?.unwrap();
                sqlx::query("UPDATE processor_jobs SET status='failed',attempts=1,next_retry_at=now() WHERE id=$1").bind(job).execute(&pool).await?;
            } else {
                slow_events.push(get_event_by_id(&pool, id).await?.unwrap());
            }
        }
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let token = CancellationToken::new();
        let scheduler = JobScheduler::new(
            pool.clone(),
            Arc::new(ProcessorRegistry::for_tests_with_configs(processors)),
            rx,
            token.clone(),
            configs,
        );
        tx.send(slow_events).await?;
        let worker = tokio::spawn(scheduler.run());
        tokio::time::timeout(Duration::from_secs(5), async {
            assert_eq!(entries.recv().await.unwrap(), "slow");
            assert_eq!(entries.recv().await.unwrap(), "slow");
            assert_eq!(peak.load(Ordering::SeqCst), 2);
            // Both slow permits are held. A new batch for a different
            // processor must still reach execution without releasing them.
            let fast_id: i64 = sqlx::query_scalar("INSERT INTO events(event_type,account_id,mailbox_name) VALUES ('email_arrived','fast','INBOX') RETURNING id").fetch_one(&pool).await?;
            tx.send(vec![get_event_by_id(&pool, fast_id).await?.unwrap()]).await?;
            assert_eq!(entries.recv().await.unwrap(), "fast");
            release.add_permits(2);
            assert_eq!(entries.recv().await.unwrap(), "slow");
            release.add_permits(1);
            loop {
                let count: i64 = sqlx::query_scalar("SELECT count(*) FROM processor_jobs WHERE status='completed'").fetch_one(&pool).await?;
                if count == 4 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        }).await??;
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        token.cancel();
        worker.await??;
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn scheduler_restart_recovers_interrupted_and_pending_jobs_once(
        pool: PgPool,
    ) -> Result<()> {
        let event = create_scheduler_test_event(&pool).await?;
        let interrupted = jobs::create_job(&pool, event, "counting").await?.unwrap();
        jobs::claim_job_with_lease(&pool, interrupted, 30, false)
            .await?
            .unwrap();
        sqlx::query(
            "UPDATE processor_jobs SET lease_expires_at=now()-interval '1 second' WHERE id=$1",
        )
        .bind(interrupted)
        .execute(&pool)
        .await?;
        let second_event = create_scheduler_test_event(&pool).await?;
        let pending = jobs::create_job(&pool, second_event, "counting")
            .await?
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let config = test_processor_config("counting", 3, vec![0]);
        let registry = Arc::new(ProcessorRegistry::for_tests_with_configs(vec![(
            Box::new(CountingProcessor {
                name: "counting".into(),
                events: vec!["email_arrived".into()],
                calls: calls.clone(),
            }),
            config.clone(),
        )]));
        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![config],
        );
        scheduler.retry_sweep().await;
        scheduler.retry_sweep().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        for (id, attempts) in [(interrupted, 2), (pending, 1)] {
            let job = jobs::get_job_by_id(&pool, id).await?.unwrap();
            assert_eq!(job.status, "completed");
            assert_eq!(job.attempts, attempts);
        }
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn unmatched_poll_batch_does_not_starve_later_matching_event(pool: PgPool) -> Result<()> {
        for _ in 0..100 {
            sqlx::query("INSERT INTO events (event_type, account_id, mailbox_name, payload) VALUES ('email_arrived', 'excluded', 'INBOX', '{}'::jsonb)")
                .execute(&pool).await?;
        }
        let matching_id: i64 = sqlx::query_scalar("INSERT INTO events (event_type, account_id, mailbox_name, payload) VALUES ('email_arrived', 'included', 'INBOX', '{}'::jsonb) RETURNING id")
            .fetch_one(&pool).await?;
        let config = ProcessorConfig {
            sources: Some(vec![crate::config::ProcessorSource {
                account: "included".into(),
                mailboxes: None,
            }]),
            ..test_processor_config("matching", 0, vec![])
        };
        let registry = Arc::new(ProcessorRegistry::for_tests_with_configs(vec![(
            Box::new(TestProcessor {
                name: "matching".into(),
                events: vec!["email_arrived".into()],
                output: ProcessorOutput {
                    success: true,
                    message: None,
                    metadata: None,
                    metrics: vec![],
                },
            }),
            config.clone(),
        )]));
        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![config],
        );
        let first_batch = crate::db::events::get_unprocessed_events(&pool, 100).await?;
        assert_eq!(first_batch.len(), 100);
        scheduler.process_events(first_batch).await;
        let next_batch = crate::db::events::get_unprocessed_events(&pool, 100).await?;
        assert!(next_batch.iter().any(|event| event.id == matching_id));
        scheduler.process_events(next_batch).await;
        let marker: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT dispatched_at FROM events WHERE id = $1")
                .bind(matching_id)
                .fetch_one(&pool)
                .await?;
        assert!(marker.is_some());
        let unmatched_jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM processor_jobs j JOIN events e ON e.id=j.event_id WHERE e.account_id='excluded'").fetch_one(&pool).await?;
        assert_eq!(unmatched_jobs, 0);
        let matching_job = jobs::get_job_by_event_and_processor(&pool, matching_id, "matching")
            .await?
            .unwrap();
        assert_eq!(matching_job.status, "completed");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn scheduler_recovers_pending_once_and_abandons_ineligible_queued_jobs(
        pool: PgPool,
    ) -> Result<()> {
        let calls = Arc::new(AtomicUsize::new(0));
        let config = ProcessorConfig {
            sources: Some(vec![
                crate::config::ProcessorSource {
                    account: "test".into(),
                    mailboxes: None,
                },
                crate::config::ProcessorSource {
                    account: "test".into(),
                    mailboxes: Some(vec!["INBOX".into()]),
                },
            ]),
            ..test_processor_config("counting", 0, vec![])
        };
        let registry = Arc::new(ProcessorRegistry::for_tests_with_configs(vec![(
            Box::new(CountingProcessor {
                name: "counting".into(),
                events: vec!["email_arrived".into()],
                calls: calls.clone(),
            }),
            config.clone(),
        )]));
        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![config],
        );

        let event_id = create_scheduler_test_event(&pool).await?;
        let event = get_event_by_id(&pool, event_id).await?.unwrap();
        // Both dispatchers race from an event with no pre-existing job. The
        // overlapping selectors must still register and invoke it exactly once.
        let (left, right) = tokio::join!(
            scheduler.process_events(vec![event.clone()]),
            scheduler.process_events(vec![event])
        );
        let _ = (left, right);
        let completed = jobs::get_job_by_event_and_processor(&pool, event_id, "counting")
            .await?
            .unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.attempts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A separately seeded eligible pending job is recovered by the sweep.
        let pending_event_id = create_scheduler_test_event(&pool).await?;
        let pending_id = jobs::create_job(&pool, pending_event_id, "counting")
            .await?
            .unwrap();
        scheduler.retry_sweep().await;
        let recovered = jobs::get_job_by_id(&pool, pending_id).await?.unwrap();
        assert_eq!(recovered.status, "completed");
        assert_eq!(recovered.attempts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Pause dispatch immediately before its claim. Its transaction has
        // committed and returned the new job ID, so the sweep can observe the
        // same pending row and contend through the identical execution path.
        let contested_event_id = create_scheduler_test_event(&pool).await?;
        let contested_event = get_event_by_id(&pool, contested_event_id).await?.unwrap();
        let mut contested_scheduler = JobScheduler::new(
            pool.clone(),
            scheduler.registry.clone(),
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![test_processor_config("counting", 0, vec![])],
        );
        let (arrived_tx, mut arrived_rx) = tokio::sync::mpsc::unbounded_channel();
        contested_scheduler.before_claim =
            Some((arrived_tx, Arc::new(tokio::sync::Barrier::new(2))));
        let contested_scheduler = Arc::new(contested_scheduler);
        let dispatch_scheduler = contested_scheduler.clone();
        let dispatch = tokio::spawn(async move {
            dispatch_scheduler
                .process_events(vec![contested_event])
                .await;
        });
        // This notification is emitted only after registration commits and
        // dispatch has reached the claim gate. Start the sweep afterward, so
        // its runnable-job query cannot race ahead of job registration.
        tokio::time::timeout(Duration::from_secs(5), arrived_rx.recv())
            .await
            .expect("dispatch did not reach the pre-claim gate")
            .expect("pre-claim notification channel closed");
        let sweep_scheduler = contested_scheduler.clone();
        let sweep = tokio::spawn(async move { sweep_scheduler.retry_sweep().await });
        tokio::time::timeout(Duration::from_secs(10), async {
            let (dispatch, sweep) = tokio::join!(dispatch, sweep);
            dispatch.expect("dispatch task panicked");
            sweep.expect("sweep task panicked");
        })
        .await
        .expect("dispatch-versus-sweep claim contention timed out");
        let contested_job_id: i64 = sqlx::query_scalar(
            "SELECT id FROM processor_jobs WHERE event_id=$1 AND processor_name='counting'",
        )
        .bind(contested_event_id)
        .fetch_one(&pool)
        .await?;
        let contested = jobs::get_job_by_id(&pool, contested_job_id).await?.unwrap();
        assert_eq!(contested.status, "completed");
        assert_eq!(contested.attempts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Current source exclusions are enforced by the same sweep execution
        // path, for pending and due failed work, without invoking or mutating
        // prior output/attempt history.
        for (status, attempts) in [("pending", 3), ("failed", 4)] {
            for reason in ["source", "subscription", "disabled", "removed"] {
                let excluded_event = create_scheduler_test_event(&pool).await?;
                let job_id = jobs::create_job(&pool, excluded_event, "counting")
                    .await?
                    .unwrap();
                let previous_output = serde_json::json!({"kept": true});
                sqlx::query("UPDATE processor_jobs SET status=$2, attempts=$3, last_error='old error', next_retry_at=CASE WHEN $2='failed' THEN now()-interval '1 second' ELSE NULL END, output=$4 WHERE id=$1")
                    .bind(job_id).bind(status).bind(attempts).bind(&previous_output).execute(&pool).await?;
                let mut excluded_config = test_processor_config("counting", 0, vec![]);
                match reason {
                    "source" => {
                        excluded_config.sources = Some(vec![crate::config::ProcessorSource {
                            account: "other-account".into(),
                            mailboxes: None,
                        }])
                    }
                    "subscription" => excluded_config.events = vec!["email_removed".into()],
                    "disabled" => excluded_config.enabled = false,
                    _ => {}
                }
                let excluded_registry = Arc::new(ProcessorRegistry::for_tests_with_configs(
                    if reason == "removed" {
                        vec![]
                    } else {
                        vec![(
                            Box::new(CountingProcessor {
                                name: "counting".into(),
                                events: vec!["email_arrived".into()],
                                calls: calls.clone(),
                            }),
                            excluded_config.clone(),
                        )]
                    },
                ));
                let excluded_scheduler = JobScheduler::new(
                    pool.clone(),
                    excluded_registry,
                    tokio::sync::mpsc::channel(16).1,
                    CancellationToken::new(),
                    vec![excluded_config],
                );
                excluded_scheduler.retry_sweep().await;
                let abandoned = jobs::get_job_by_id(&pool, job_id).await?.unwrap();
                assert_eq!(abandoned.status, "abandoned");
                assert_eq!(abandoned.attempts, attempts);
                assert_eq!(abandoned.output, Some(previous_output));
                assert!(abandoned.next_retry_at.is_none());
                let error = abandoned.last_error.as_deref().unwrap();
                assert!(
                    match reason {
                        "source" => error.contains("excludes source"),
                        "subscription" => error.contains("not subscribed"),
                        _ => error.contains("unavailable or disabled"),
                    },
                    "unexpected {reason} error: {error}"
                );
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_cleanup_preserves_partially_dispatched_event(pool: PgPool) -> Result<()> {
        for terminal_status in ["completed", "abandoned"] {
            let event_id = sqlx::query_scalar(
                r#"
                INSERT INTO events (event_type, account_id, mailbox_name, created_at)
                VALUES ('email_arrived', 'test', 'INBOX', now() - interval '60 days')
                RETURNING id
                "#,
            )
            .fetch_one(&pool)
            .await?;
            let event = get_event_by_id(&pool, event_id).await?.unwrap();
            let second_release = Arc::new(Semaphore::new(0));
            let registry = Arc::new(ProcessorRegistry::for_tests(vec![
                Box::new(CleanupBetweenProcessors {
                    pool: pool.clone(),
                    events: vec!["email_arrived".into()],
                    terminal_status,
                    second_release: second_release.clone(),
                }),
                Box::new(GatedProcessor {
                    name: "second".into(),
                    events: vec!["email_arrived".into()],
                    release: Some(second_release),
                    active: Arc::new(AtomicUsize::new(0)),
                    peak: Arc::new(AtomicUsize::new(0)),
                    entered: tokio::sync::mpsc::unbounded_channel().0,
                }),
            ]));
            let scheduler = JobScheduler::new(
                pool.clone(),
                registry,
                tokio::sync::mpsc::channel(16).1,
                CancellationToken::new(),
                vec![
                    test_processor_config("first", 0, vec![]),
                    test_processor_config("second", 0, vec![]),
                ],
            );

            scheduler.process_events(vec![event.clone()]).await;
            // Repeated dispatch must not execute existing jobs again.
            scheduler.process_events(vec![event]).await;
            for (name, status) in [("first", terminal_status), ("second", "completed")] {
                let job = jobs::get_job_by_event_and_processor(&pool, event_id, name)
                    .await?
                    .expect("both processors should have run");
                assert_eq!(job.status, status);
                assert_eq!(job.attempts, 1);
            }

            // Once the full dispatch is terminal, retention cleanup can delete it.
            crate::housekeeping::cleanup_old_events(&pool, 30).await?;
            assert!(get_event_by_id(&pool, event_id).await?.is_none());
        }
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_scheduler_persists_posted_output(pool: PgPool) {
        let output = ProcessorOutput {
            success: true,
            message: Some("posted".into()),
            metadata: Some(serde_json::json!({
                "outcome": "posted",
                "firefly_transaction_id": "tx-123"
            })),
            metrics: vec![],
        };

        let proc = Box::new(TestProcessor {
            name: "test_mailtx".into(),
            events: vec!["email_arrived".into()],
            output: output.clone(),
        });

        let registry = Arc::new(ProcessorRegistry::for_tests(vec![proc]));

        // Seed event + email so the processor can run.
        let event_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO events (event_type, account_id, mailbox_name, payload)
            VALUES ('email_arrived', 'test', 'INBOX', '{}'::jsonb)
            RETURNING id
            "#,
        )
        .fetch_one(&pool)
        .await
        .expect("create event");

        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![test_processor_config("test_mailtx", 0, vec![])],
        );

        let event = Event {
            id: event_id,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        scheduler.process_events(vec![event]).await;

        let job = jobs::get_job_by_event_and_processor(&pool, event_id, "test_mailtx")
            .await
            .expect("fetch job")
            .expect("job should exist");

        assert_eq!(job.status, "completed");
        assert_eq!(job.output, Some(serde_json::to_value(&output).unwrap()));
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_scheduler_persists_no_transaction_output(pool: PgPool) {
        let output = ProcessorOutput {
            success: true,
            message: Some("no transaction found".into()),
            metadata: Some(serde_json::json!({
                "outcome": "no_transaction"
            })),
            metrics: vec![],
        };

        let proc = Box::new(TestProcessor {
            name: "test_mailtx".into(),
            events: vec!["email_arrived".into()],
            output: output.clone(),
        });

        let registry = Arc::new(ProcessorRegistry::for_tests(vec![proc]));

        let event_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO events (event_type, account_id, mailbox_name, payload)
            VALUES ('email_arrived', 'test', 'INBOX', '{}'::jsonb)
            RETURNING id
            "#,
        )
        .fetch_one(&pool)
        .await
        .expect("create event");

        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![test_processor_config("test_mailtx", 0, vec![])],
        );

        let event = Event {
            id: event_id,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        scheduler.process_events(vec![event]).await;

        let job = jobs::get_job_by_event_and_processor(&pool, event_id, "test_mailtx")
            .await
            .expect("fetch job")
            .expect("job should exist");

        assert_eq!(job.status, "completed");
        // Assert the complete serialized output matches exactly.
        assert_eq!(job.output, Some(serde_json::to_value(&output).unwrap()));
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_scheduler_persists_failure_output(pool: PgPool) {
        let output = ProcessorOutput {
            success: false,
            message: Some("firefly returned 500".into()),
            metadata: Some(serde_json::json!({
                "outcome": "error"
            })),
            metrics: vec![],
        };

        let proc = Box::new(TestProcessor {
            name: "test_mailtx".into(),
            events: vec!["email_arrived".into()],
            output: output.clone(),
        });

        let registry = Arc::new(ProcessorRegistry::for_tests(vec![proc]));

        let event_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO events (event_type, account_id, mailbox_name, payload)
            VALUES ('email_arrived', 'test', 'INBOX', '{}'::jsonb)
            RETURNING id
            "#,
        )
        .fetch_one(&pool)
        .await
        .expect("create event");

        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![test_processor_config("test_mailtx", 0, vec![])],
        );

        let event = Event {
            id: event_id,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        scheduler.process_events(vec![event]).await;

        let job = jobs::get_job_by_event_and_processor(&pool, event_id, "test_mailtx")
            .await
            .expect("fetch job")
            .expect("job should exist");

        assert_eq!(job.status, "abandoned");
        assert!(job.last_error.is_some());
        // Assert the complete serialized output matches exactly.
        assert_eq!(job.output, Some(serde_json::to_value(&output).unwrap()));
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_scheduler_retry_clears_old_output(pool: PgPool) {
        // First run: produce an output and mark as failed (with retry configured).
        let mut output = ProcessorOutput {
            success: false,
            message: Some("first attempt failed".into()),
            metadata: Some(serde_json::json!({
                "outcome": "error",
                "attempt": 1
            })),
            metrics: vec![],
        };

        let proc = Box::new(TestProcessor {
            name: "test_mailtx".into(),
            events: vec!["email_arrived".into()],
            output: output.clone(),
        });

        let registry = Arc::new(ProcessorRegistry::for_tests(vec![proc]));

        let event_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO events (event_type, account_id, mailbox_name, payload)
            VALUES ('email_arrived', 'test', 'INBOX', '{}'::jsonb)
            RETURNING id
            "#,
        )
        .fetch_one(&pool)
        .await
        .expect("create event");

        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![test_processor_config("test_mailtx", 2, vec![0])],
        );

        let event = Event {
            id: event_id,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        scheduler.process_events(vec![event]).await;

        let job = jobs::get_job_by_event_and_processor(&pool, event_id, "test_mailtx")
            .await
            .expect("fetch job")
            .expect("job should exist");

        assert_eq!(job.status, "failed");
        assert!(job.output.is_some());

        // Now simulate a retry sweep — the second attempt should clear the old output
        // before running. We'll manually trigger a retry by setting next_retry_at in the past.
        sqlx::query(
            "UPDATE processor_jobs SET next_retry_at = now() - interval '1 second' WHERE id = $1",
        )
        .bind(job.id)
        .execute(&pool)
        .await
        .expect("set retry time");

        // Second attempt: succeed
        output.success = true;
        output.message = Some("second attempt succeeded".into());
        output.metadata = Some(serde_json::json!({
            "outcome": "posted",
            "attempt": 2
        }));

        // Re-register with the new output
        let proc2 = Box::new(TestProcessor {
            name: "test_mailtx".into(),
            events: vec!["email_arrived".into()],
            output: output.clone(),
        });
        let registry2 = Arc::new(ProcessorRegistry::for_tests(vec![proc2]));

        let scheduler2 = JobScheduler::new(
            pool.clone(),
            registry2,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![test_processor_config("test_mailtx", 2, vec![0])],
        );

        scheduler2.retry_sweep().await;

        let job = jobs::get_job_by_event_and_processor(&pool, event_id, "test_mailtx")
            .await
            .expect("fetch job")
            .expect("job should exist");

        assert_eq!(job.status, "completed");
        let persisted = job.output.expect("output should be persisted");
        // Should be the second attempt's output, not the first.
        assert_eq!(
            persisted
                .get("metadata")
                .and_then(|m| m.get("attempt"))
                .and_then(|a| a.as_i64()),
            Some(2)
        );
    }

    /// End-to-end regression: a CommandProcessor whose script emits structured
    /// ProcessorOutput JSON on stdout and exits non-zero should have the full
    /// output (metadata + metrics) persisted by the scheduler, not a synthesized
    /// fallback.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_scheduler_command_processor_structured_failure_output(pool: PgPool) {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Script emits structured ProcessorOutput and exits 1.
        let script_content = r#"#!/bin/sh
printf '{"success":false,"message":"firefly 500","metadata":{"outcome":"error","detail":"upstream unavailable"},"metrics":[{"name":"firefly_requests_total","kind":"counter","value":1.0,"labels":{"operation":"post_transaction","result":"error"}}]}'
exit 1
"#;
        let mut script_file = NamedTempFile::new().expect("create temp script");
        write!(script_file, "{script_content}").expect("write script");
        let script_path = script_file.path().to_string_lossy().to_string();

        // Build a command processor config pointing at the script.
        let mut config_map: HashMap<String, toml::Value> = HashMap::new();
        config_map.insert("command".to_string(), toml::Value::String("sh".to_string()));
        config_map.insert(
            "args".to_string(),
            toml::Value::Array(vec![toml::Value::String(script_path.clone())]),
        );
        let cmd_config = ProcessorConfig {
            name: "test_cmd".into(),
            enabled: true,
            events: vec!["email_arrived".into()],
            sources: None,
            max_retries: 0,
            retry_backoff_secs: vec![],
            timeout_secs: 10,
            concurrency: 1,
            config: config_map,
        };

        let cmd_proc = Box::new(crate::processor::builtin::command::CommandProcessor::new(
            &cmd_config,
        ));
        let registry = Arc::new(ProcessorRegistry::for_tests(vec![cmd_proc]));

        let event_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO events (event_type, account_id, mailbox_name, payload)
            VALUES ('email_arrived', 'test', 'INBOX', '{}'::jsonb)
            RETURNING id
            "#,
        )
        .fetch_one(&pool)
        .await
        .expect("create event");

        let scheduler = JobScheduler::new(
            pool.clone(),
            registry,
            tokio::sync::mpsc::channel(16).1,
            CancellationToken::new(),
            vec![cmd_config],
        );

        let event = Event {
            id: event_id,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };
        scheduler.process_events(vec![event]).await;

        let job = jobs::get_job_by_event_and_processor(&pool, event_id, "test_cmd")
            .await
            .expect("fetch job")
            .expect("job should exist");

        // Job should be abandoned (max_retries=0) with the structured output
        // persisted in full.
        assert_eq!(job.status, "abandoned");
        let output = job.output.expect("output should be persisted");

        // Verify the full structured output survived unchanged.
        assert_eq!(output.get("success").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            output.get("message").and_then(|v| v.as_str()),
            Some("firefly 500")
        );
        assert_eq!(
            output
                .get("metadata")
                .and_then(|m| m.get("outcome"))
                .and_then(|v| v.as_str()),
            Some("error")
        );
        assert_eq!(
            output
                .get("metadata")
                .and_then(|m| m.get("detail"))
                .and_then(|v| v.as_str()),
            Some("upstream unavailable")
        );

        // Verify metrics survived unchanged.
        let metrics = output.get("metrics").and_then(|v| v.as_array());
        assert!(metrics.is_some(), "metrics should be present in output");
        let metrics = metrics.unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(
            metrics[0].get("name").and_then(|v| v.as_str()),
            Some("firefly_requests_total")
        );
        assert_eq!(
            metrics[0]
                .get("labels")
                .and_then(|l| l.get("operation"))
                .and_then(|v| v.as_str()),
            Some("post_transaction")
        );
    }
}
