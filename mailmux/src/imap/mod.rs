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

        // Report monitored mailboxes
        crate::metrics::set_mailboxes_monitored(account_id, self.account.mailboxes.len() as f64);

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
                if e.is::<connection::AuthenticationFailure>() {
                    error!("mailbox watcher stopped after IMAP authentication failure");
                    return;
                }
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
