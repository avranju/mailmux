use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::db::events::{Event, get_unprocessed_events};

const CHANNEL_NAME: &str = "mailmux_events";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const BATCH_SIZE: i64 = 100;

/// Listens for new events via PostgreSQL LISTEN/NOTIFY and periodic polling.
/// Sends events to the job scheduler via an mpsc channel.
pub struct EventLoop {
    pool: PgPool,
    token: CancellationToken,
    event_tx: mpsc::Sender<Vec<Event>>,
}

impl EventLoop {
    pub fn new(pool: PgPool, token: CancellationToken, event_tx: mpsc::Sender<Vec<Event>>) -> Self {
        Self {
            pool,
            token,
            event_tx,
        }
    }

    pub async fn run(&self) -> Result<()> {
        info!("event loop starting");

        // Try to set up LISTEN, fall back to poll-only if it fails
        let mut listener = match PgListener::connect_with(&self.pool).await {
            Ok(mut l) => {
                if let Err(e) = l.listen(CHANNEL_NAME).await {
                    warn!(error = %e, "failed to LISTEN on channel, falling back to polling");
                    None
                } else {
                    info!(channel = CHANNEL_NAME, "listening for NOTIFY events");
                    Some(l)
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to create PG listener, falling back to polling");
                None
            }
        };

        let poll_interval = tokio::time::interval(POLL_INTERVAL);
        tokio::pin!(poll_interval);

        loop {
            tokio::select! {
                _ = self.token.cancelled() => {
                    info!("event loop shutting down");
                    return Ok(());
                }

                // LISTEN/NOTIFY path
                notification = async {
                    if let Some(ref mut l) = listener {
                        l.recv().await
                    } else {
                        // No listener — just wait forever (poll interval handles it)
                        std::future::pending().await
                    }
                } => {
                    match notification {
                        Ok(n) => {
                            debug!(payload = n.payload(), "received NOTIFY");
                            self.fetch_and_dispatch().await;
                        }
                        Err(e) => {
                            warn!(error = %e, "NOTIFY listener error, will retry on next poll");
                        }
                    }
                }

                // Periodic poll fallback
                _ = poll_interval.tick() => {
                    self.fetch_and_dispatch().await;
                }
            }
        }
    }

    async fn fetch_and_dispatch(&self) {
        match get_unprocessed_events(&self.pool, BATCH_SIZE).await {
            Ok(events) if events.is_empty() => {
                // Nothing to do
            }
            Ok(events) => {
                let count = events.len();
                debug!(count, "dispatching unprocessed events");
                tokio::select! {
                    _ = self.token.cancelled() => {},
                    result = self.event_tx.send(events) => if let Err(error) = result {
                        error!(%error, "failed to send events to scheduler");
                    },
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to fetch unprocessed events");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn event(pool: &PgPool) -> Result<i64> {
        Ok(sqlx::query_scalar("INSERT INTO events(event_type,account_id,mailbox_name,payload) VALUES ('email_arrived','test','INBOX','{}') RETURNING id").fetch_one(pool).await?)
    }
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn notify_and_initial_poll_overlap_register_only_one_durable_job(
        pool: PgPool,
    ) -> Result<()> {
        let id = event(&pool).await?;
        let token = CancellationToken::new();
        let (sender, mut receiver) = mpsc::channel(1);
        let listener = std::sync::Arc::new(EventLoop::new(pool.clone(), token.clone(), sender));
        let running = listener.clone();
        let task = tokio::spawn(async move { running.run().await });
        let first = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await?
            .unwrap();
        assert_eq!(first[0].id, id);
        sqlx::query("SELECT pg_notify('mailmux_events','overlap')")
            .execute(&pool)
            .await?;
        let second = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await?
            .unwrap();
        assert_eq!(second[0].id, id);
        let (left, right) = tokio::join!(
            crate::db::jobs::register_event_dispatch(&pool, first[0].id, &["logger"]),
            crate::db::jobs::register_event_dispatch(&pool, second[0].id, &["logger"])
        );
        assert_eq!(left?.len() + right?.len(), 1);
        assert!(get_unprocessed_events(&pool, 100).await?.is_empty());
        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), task).await???;
        Ok(())
    }
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn cancellation_interrupts_a_full_dispatch_channel_without_losing_event(
        pool: PgPool,
    ) -> Result<()> {
        let id = event(&pool).await?;
        let token = CancellationToken::new();
        let (sender, _receiver) = mpsc::channel(1);
        sender.try_send(vec![]).unwrap();
        let listener = EventLoop::new(pool.clone(), token.clone(), sender);
        let dispatch = listener.fetch_and_dispatch();
        tokio::pin!(dispatch);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut dispatch)
                .await
                .is_err()
        );
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), dispatch).await?;
        assert_eq!(get_unprocessed_events(&pool, 100).await?[0].id, id);
        Ok(())
    }
}
