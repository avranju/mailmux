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
    pub fn new(
        pool: PgPool,
        token: CancellationToken,
        event_tx: mpsc::Sender<Vec<Event>>,
    ) -> Self {
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
                if let Err(e) = self.event_tx.send(events).await {
                    error!(error = %e, "failed to send events to scheduler");
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to fetch unprocessed events");
            }
        }
    }
}
