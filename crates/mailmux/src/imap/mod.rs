pub mod connection;
pub mod sync;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::AccountConfig;
use crate::store::MessageStore;

const CIRCUIT_BREAKER_FAILURES: usize = 10;
const CIRCUIT_BREAKER_WINDOW: Duration = Duration::from_secs(300); // 5 minutes
const CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(900); // 15 minutes
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Manages all IMAP tasks for a single account.
/// Supervises watchers and restarts them on failure.
pub struct AccountManager {
    pub account: Arc<AccountConfig>,
    pool: PgPool,
    store: Arc<MessageStore>,
    token: CancellationToken,
}

impl AccountManager {
    pub fn new(
        account: AccountConfig,
        pool: PgPool,
        store: Arc<MessageStore>,
        token: CancellationToken,
    ) -> Self {
        Self {
            account: Arc::new(account),
            pool,
            store,
            token,
        }
    }

    pub async fn run(self) -> Result<()> {
        let account_id = &self.account.id;
        info!(account = account_id, "starting account manager");

        let mut join_set = JoinSet::new();
        let mut failure_times: VecDeque<Instant> = VecDeque::new();

        // Spawn initial watchers
        for mailbox in &self.account.mailboxes {
            self.spawn_watcher(&mut join_set, mailbox.clone());
        }

        // Monitor and restart failed watchers
        loop {
            tokio::select! {
                _ = self.token.cancelled() => {
                    info!(account = account_id, "account manager shutting down");
                    join_set.abort_all();
                    while join_set.join_next().await.is_some() {}
                    return Ok(());
                }

                result = join_set.join_next() => {
                    match result {
                        Some(Ok(())) => {
                            // A watcher exited cleanly
                        }
                        Some(Err(e)) => {
                            warn!(account = account_id, error = %e, "watcher task panicked");

                            // Track failure for circuit breaker
                            let now = Instant::now();
                            failure_times.push_back(now);

                            // Remove failures outside the window
                            while failure_times.front().is_some_and(|t| now.duration_since(*t) > CIRCUIT_BREAKER_WINDOW) {
                                failure_times.pop_front();
                            }

                            // Check circuit breaker
                            if failure_times.len() >= CIRCUIT_BREAKER_FAILURES {
                                warn!(
                                    account = account_id,
                                    failures = failure_times.len(),
                                    cooldown_secs = CIRCUIT_BREAKER_COOLDOWN.as_secs(),
                                    "circuit breaker triggered, entering cooldown"
                                );
                                failure_times.clear();

                                tokio::select! {
                                    _ = tokio::time::sleep(CIRCUIT_BREAKER_COOLDOWN) => {}
                                    _ = self.token.cancelled() => return Ok(()),
                                }
                            }
                        }
                        None => {
                            // All watchers have exited — restart them
                            if self.token.is_cancelled() {
                                return Ok(());
                            }

                            warn!(account = account_id, "all watchers exited, restarting after delay");
                            tokio::select! {
                                _ = tokio::time::sleep(RESTART_DELAY) => {}
                                _ = self.token.cancelled() => return Ok(()),
                            }

                            for mailbox in &self.account.mailboxes {
                                self.spawn_watcher(&mut join_set, mailbox.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    fn spawn_watcher(&self, join_set: &mut JoinSet<()>, mailbox: String) {
        let watcher = sync::MailboxWatcher::new(
            self.account.clone(),
            mailbox.clone(),
            self.pool.clone(),
            self.store.clone(),
            self.token.clone(),
        );
        let acct_id = self.account.id.clone();

        join_set.spawn(async move {
            if let Err(e) = watcher.run().await {
                error!(
                    account = acct_id,
                    mailbox = mailbox,
                    error = %e,
                    "mailbox watcher failed"
                );
            }
        });
    }
}

/// Exponential backoff with jitter for connection retries.
pub fn retry_delay(attempt: u32, base_ms: u64, max_ms: u64) -> Duration {
    let delay = base_ms.saturating_mul(1u64 << attempt.min(10));
    let delay = delay.min(max_ms);
    let jitter = delay / 2 + rand_simple(delay / 2);
    Duration::from_millis(jitter)
}

fn rand_simple(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    nanos % max
}
