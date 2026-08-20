use crate::{
    index::{SearchIndex, document_key, tantivy_doc},
    storage::{Repository, models::PendingDocument},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time};
use tokio_util::sync::CancellationToken;

/// Synchronous batch seams make version races and cancellation boundaries
/// deterministic in tests without changing the production worker flow.
pub type BeforeBatchHook = Arc<dyn Fn(&[PendingDocument]) + Send + Sync>;

pub type AfterBatchHook = Arc<dyn Fn(usize) + Send + Sync>;

#[derive(Clone, Default)]
pub struct WorkerHooks {
    pub before_batch: Option<BeforeBatchHook>,
    pub after_batch: Option<AfterBatchHook>,
}

pub struct IndexWorker {
    repo: Arc<Repository>,
    index: Arc<SearchIndex>,
    writer: Arc<Mutex<tantivy::IndexWriter>>,
    notify: Arc<Notify>,
    batch: usize,
    interval: Duration,
    cancel: CancellationToken,
    hooks: WorkerHooks,
}

impl IndexWorker {
    pub fn new(
        repo: Arc<Repository>,
        index: Arc<SearchIndex>,
        writer: tantivy::IndexWriter,
        notify: Arc<Notify>,
        batch: usize,
        interval_ms: u64,
        cancel: CancellationToken,
    ) -> Self {
        Self::new_with_hooks(
            repo,
            index,
            writer,
            notify,
            batch,
            interval_ms,
            cancel,
            WorkerHooks::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_hooks(
        repo: Arc<Repository>,
        index: Arc<SearchIndex>,
        writer: tantivy::IndexWriter,
        notify: Arc<Notify>,
        batch: usize,
        interval_ms: u64,
        cancel: CancellationToken,
        hooks: WorkerHooks,
    ) -> Self {
        Self {
            repo,
            index,
            writer: Arc::new(Mutex::new(writer)),
            notify,
            batch: batch.max(1),
            interval: Duration::from_millis(interval_ms.max(1)),
            cancel,
            hooks,
        }
    }

    pub fn writer(&self) -> Arc<Mutex<tantivy::IndexWriter>> {
        self.writer.clone()
    }

    pub async fn run(self) {
        // Reconcile rows left pending by a process crash before waiting for new work.
        self.drain().await;
        let mut tick = time::interval(self.interval);
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = self.notify.notified() => self.drain().await,
                _ = tick.tick() => self.drain().await,
            }
        }
    }

    async fn drain(&self) {
        let mut batch_number = 0;
        loop {
            // Finish an in-progress batch, but do not start another one after
            // shutdown has been requested.
            if self.cancel.is_cancelled() {
                break;
            }
            let rows = match self.repo.pending(self.batch).await {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(%error, "pending index scan failed");
                    break;
                }
            };
            if rows.is_empty() {
                break;
            }
            if self.cancel.is_cancelled() {
                break;
            }
            if let Some(hook) = &self.hooks.before_batch {
                hook(&rows);
            }
            let repo = self.repo.clone();
            let index = self.index.clone();
            let writer = self.writer.clone();
            let indexed_rows = rows.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut w = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("writer lock poisoned"))?;
                let outcome = (|| {
                    for d in &indexed_rows {
                        w.delete_term(tantivy::Term::from_field_text(
                            index.fields.document_key,
                            &document_key(&d.source, &d.source_id),
                        ));
                        w.add_document(tantivy_doc(d, &index.fields))?;
                    }
                    w.commit()?;
                    index.reload()?;
                    Ok::<(), anyhow::Error>(())
                })();
                if let Err(error) = outcome {
                    // Delete/add operations remain queued in IndexWriter after an
                    // add or commit error. Never let them leak into a later batch.
                    let _ = w.rollback();
                    Err(error)
                } else {
                    Ok(())
                }
            })
            .await;
            match result {
                Ok(Ok(())) => {
                    for d in &rows {
                        if let Err(error) = repo.mark_indexed(d.id, &d.raw_sha256).await {
                            tracing::warn!(document_id=d.id, %error, "mark indexed failed");
                        }
                    }
                }
                Ok(Err(error)) => {
                    let message = concise_error(&error.to_string());
                    tracing::error!(%message, "index batch failed");
                    for d in &rows {
                        if let Err(error) = repo.mark_error(d.id, &d.raw_sha256, &message).await {
                            tracing::warn!(document_id=d.id, %error, "mark index error failed");
                        }
                    }
                }
                Err(error) => tracing::error!(%error, "index task failed"),
            }
            batch_number += 1;
            if let Some(hook) = &self.hooks.after_batch {
                hook(batch_number);
            }
        }
    }
}

fn concise_error(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}
