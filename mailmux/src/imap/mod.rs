pub mod connection;
pub mod sync;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use nonzero_ext::nonzero;
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
    rate_limiter: Arc<DefaultDirectRateLimiter>,
}

impl AccountManager {
    pub fn new(
        account: AccountConfig,
        pool: PgPool,
        store: Arc<MessageStore>,
        token: CancellationToken,
    ) -> Self {
        let rate =
            std::num::NonZeroU32::new(account.rate_limit_per_second).unwrap_or(nonzero!(1u32));
        let rate_limiter = Arc::new(RateLimiter::direct(Quota::per_second(rate)));
        Self {
            account: Arc::new(account),
            pool,
            store,
            token,
            rate_limiter,
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

    fn watcher(&self, mailbox: String) -> sync::MailboxWatcher {
        sync::MailboxWatcher::new(
            self.account.clone(),
            mailbox,
            self.pool.clone(),
            self.store.clone(),
            self.token.clone(),
            self.rate_limiter.clone(),
        )
    }

    fn spawn_watcher(&self, join_set: &mut JoinSet<()>, mailbox: String) {
        let watcher = self.watcher(mailbox.clone());
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

#[cfg(test)]
mod auth_privacy_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn authentication_errors_and_watcher_logs_do_not_expose_account_or_server_text() {
        let captured = Capture::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let account: AccountConfig = toml::from_str(&format!(
                        r#"
                    id = "PRIVATE-ACCOUNT"
                    username = "PRIVATE-USERNAME"
                    password = "PRIVATE-PASSWORD"
                    imap_host = "127.0.0.1"
                    imap_port = {}
                    tls = false
                    mailboxes = ["PRIVATE-MAILBOX"]
                "#,
                        listener.local_addr().unwrap().port()
                    ))
                    .unwrap();
                    let server = tokio::spawn(async move {
                        for _ in 0..2 {
                            let (stream, _) = listener.accept().await.unwrap();
                            let mut peer = BufReader::new(stream);
                            peer.write_all(b"* OK ready\r\n").await.unwrap();
                            let mut line = String::new();
                            peer.read_line(&mut line).await.unwrap();
                            let tag = line.split_whitespace().next().unwrap();
                            peer.write_all(
                                format!(
                                    "{tag} NO PRIVATE-USERNAME PRIVATE-PASSWORD server detail\r\n"
                                )
                                .as_bytes(),
                            )
                            .await
                            .unwrap();
                        }
                    });
                    let error = match connection::ImapConnection::connect(&account).await {
                        Ok(_) => panic!("authentication unexpectedly succeeded"),
                        Err(error) => error,
                    };
                    assert!(error.is::<connection::AuthenticationFailure>());
                    assert_eq!(format!("{error:#}"), "IMAP authentication failed");
                    let pool = sqlx::postgres::PgPoolOptions::new()
                        .connect_lazy("postgres://unused@localhost/unused")
                        .unwrap();
                    let manager = AccountManager::new(
                        account,
                        pool,
                        Arc::new(MessageStore::new("/tmp/unused-auth-test")),
                        CancellationToken::new(),
                    );
                    let mut watchers = JoinSet::new();
                    manager.spawn_watcher(&mut watchers, "PRIVATE-MAILBOX".into());
                    tokio::time::timeout(Duration::from_secs(2), watchers.join_next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                    server.await.unwrap();
                });
        });
        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("authentication fail"), "{logs}");
        for private in [
            "PRIVATE-ACCOUNT",
            "PRIVATE-USERNAME",
            "PRIVATE-PASSWORD",
            "PRIVATE-MAILBOX",
            "server detail",
        ] {
            assert!(!logs.contains(private), "private data leaked: {private}");
        }
    }
}
