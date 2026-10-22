pub mod connection;
pub mod sync;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::AccountConfig;
use crate::store::MessageStore;

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

    pub async fn run_supervised(&self) {
        crate::supervision::supervise(
            "account manager",
            &self.token,
            crate::supervision::RestartPolicy::default(),
            || self.run(),
        )
        .await;
    }

    async fn run(&self) -> Result<()> {
        let account_id = &self.account.id;
        info!(account = account_id, "starting account manager");

        let mut join_set = JoinSet::new();

        // Report monitored mailboxes
        crate::metrics::set_mailboxes_monitored(account_id, self.account.mailboxes.len() as f64);

        // Spawn initial watchers
        for mailbox in &self.account.mailboxes {
            self.spawn_watcher(&mut join_set, mailbox.clone());
        }

        // Watcher supervisors run until cancellation or an unexpected exit.
        tokio::select! {
                _ = self.token.cancelled() => {
                    info!(account = account_id, "account manager shutting down");
                    join_set.abort_all();
                    while join_set.join_next().await.is_some() {}
                    Ok(())
                }

                result = join_set.join_next() => {
                    // Watchers have their own restart loops. An exit here is
                    // a failure of supervision itself: restart this manager,
                    // dropping its JoinSet so no old watchers survive.
                    if self.token.is_cancelled() { return Ok(()); }
                    anyhow::bail!("supervised watcher terminated unexpectedly: {result:?}");
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
        let token = self.token.clone();
        join_set.spawn(async move {
            crate::supervision::supervise(
                "mailbox watcher",
                &token,
                crate::supervision::RestartPolicy::default(),
                || watcher.run(),
            )
            .await;
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
