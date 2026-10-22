use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use governor::{Quota, RateLimiter};
use mail_parser::MessageParser;
use nonzero_ext::nonzero;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::AccountConfig;
use crate::db::emails::NewEmail;
use crate::db::events::NewEvent;
use crate::imap::connection::{FetchedMessage, IdleOutcome, ImapConnection};
use crate::store::MessageStore;

const FETCH_BATCH_SIZE: usize = 500;

/// Watches a single mailbox, performing initial + incremental sync.
/// Supports IDLE when available, falls back to polling.
pub struct MailboxWatcher {
    account: Arc<AccountConfig>,
    mailbox: String,
    pool: PgPool,
    store: Arc<MessageStore>,
    token: CancellationToken,
}

impl MailboxWatcher {
    pub fn new(
        account: Arc<AccountConfig>,
        mailbox: String,
        pool: PgPool,
        store: Arc<MessageStore>,
        token: CancellationToken,
    ) -> Self {
        Self {
            account,
            mailbox,
            pool,
            store,
            token,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let rate_limit = self.account.rate_limit_per_second.max(1);
        let rate_limiter = Arc::new(RateLimiter::direct(Quota::per_second(
            std::num::NonZeroU32::new(rate_limit).unwrap_or(nonzero!(1u32)),
        )));

        let poll_interval = Duration::from_secs(self.account.poll_interval_secs);
        let mut retry_attempt: u32 = 0;

        loop {
            if self.token.is_cancelled() {
                info!(
                    account = self.account.id,
                    mailbox = self.mailbox,
                    "shutdown requested, stopping mailbox watcher"
                );
                return Ok(());
            }

            match self.sync_and_idle_cycle(&rate_limiter, poll_interval).await {
                Ok(()) => {
                    retry_attempt = 0;
                }
                Err(e) => {
                    let err_str = format!("{e:#}");
                    if err_str.contains("LOGIN rejected") || err_str.contains("authentication") {
                        error!(
                            account = self.account.id,
                            mailbox = self.mailbox,
                            error = %e,
                            "authentication failure, stopping watcher"
                        );
                        return Err(e);
                    }

                    retry_attempt += 1;
                    let error_type = crate::metrics::classify_error(&err_str);
                    crate::metrics::inc_sync_failures(&self.account.id, &self.mailbox, error_type);
                    let delay = super::retry_delay(retry_attempt, 1000, 300_000);
                    warn!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        error = %e,
                        error_type,
                        retry_attempt,
                        delay_ms = delay.as_millis() as u64,
                        "sync cycle failed, will retry"
                    );

                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = self.token.cancelled() => return Ok(()),
                    }
                }
            }
        }
    }

    /// Connect, sync, then enter IDLE (or poll) loop. Returns when
    /// the connection is lost or shutdown is requested.
    async fn sync_and_idle_cycle(
        &self,
        rate_limiter: &Arc<
            RateLimiter<
                governor::state::NotKeyed,
                governor::state::InMemoryState,
                governor::clock::DefaultClock,
            >,
        >,
        poll_interval: Duration,
    ) -> Result<()> {
        debug!(
            account = self.account.id,
            mailbox = self.mailbox,
            "connecting for sync cycle"
        );

        let mut conn = ImapConnection::connect(&self.account).await?;
        let (uid_validity, _exists) = conn.select(&self.mailbox).await?;

        let heartbeat_interval = self
            .account
            .idle_heartbeat_interval_secs
            .map(Duration::from_secs);

        // Initial sync
        self.do_incremental_sync(&mut conn, uid_validity, rate_limiter)
            .await?;

        // Try IDLE mode — if it fails, fall back to polling
        loop {
            if self.token.is_cancelled() {
                let _ = conn.logout().await;
                return Ok(());
            }

            match conn.idle(&self.token, heartbeat_interval).await {
                Ok(IdleOutcome::Update) => {
                    debug!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        "IDLE update received, syncing"
                    );
                    self.do_incremental_sync(&mut conn, uid_validity, rate_limiter)
                        .await?;
                }
                Ok(IdleOutcome::Heartbeat) => {
                    debug!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        "IDLE heartbeat fired, syncing"
                    );
                    let ingested = self
                        .do_incremental_sync(&mut conn, uid_validity, rate_limiter)
                        .await?;
                    if ingested > 0 {
                        warn!(
                            account = self.account.id,
                            mailbox = self.mailbox,
                            ingested,
                            "heartbeat sync found messages that IDLE missed"
                        );
                        crate::metrics::add_idle_heartbeat_catches(
                            &self.account.id,
                            &self.mailbox,
                            ingested,
                        );
                    }
                }
                Ok(IdleOutcome::Shutdown) => {
                    let _ = conn.logout().await;
                    return Ok(());
                }
                Err(e) if e.to_string().contains("timed out") => {
                    return Err(e);
                }
                Err(e) => {
                    debug!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        error = %e,
                        "IDLE not supported or failed, falling back to polling"
                    );
                    // Fall back to polling for the rest of this connection
                    let _ = conn.logout().await;
                    return self.poll_cycle(rate_limiter, poll_interval).await;
                }
            }
        }
    }

    /// Polling fallback when IDLE is not available.
    async fn poll_cycle(
        &self,
        rate_limiter: &Arc<
            RateLimiter<
                governor::state::NotKeyed,
                governor::state::InMemoryState,
                governor::clock::DefaultClock,
            >,
        >,
        poll_interval: Duration,
    ) -> Result<()> {
        let mut conn = ImapConnection::connect(&self.account).await?;
        let (uid_validity, _exists) = conn.select(&self.mailbox).await?;

        self.do_incremental_sync(&mut conn, uid_validity, rate_limiter)
            .await?;
        let _ = conn.logout().await;

        // Wait for next poll
        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = self.token.cancelled() => {}
        }

        Ok(())
    }

    /// Fetch new messages since last_seen_uid and persist them.
    /// Uses a UID-only scan first to avoid downloading large mailboxes in one
    /// shot, then fetches bodies in batches of FETCH_BATCH_SIZE, checkpointing
    /// after each batch so progress is not lost on interruption.
    /// Returns the number of newly ingested messages.
    async fn do_incremental_sync(
        &self,
        conn: &mut ImapConnection,
        uid_validity: u32,
        rate_limiter: &Arc<
            RateLimiter<
                governor::state::NotKeyed,
                governor::state::InMemoryState,
                governor::clock::DefaultClock,
            >,
        >,
    ) -> Result<u64> {
        let stored_state =
            crate::db::emails::get_mailbox_state(&self.pool, &self.account.id, &self.mailbox)
                .await?;

        let last_seen_uid = match &stored_state {
            Some(state) => {
                if state.uid_validity != uid_validity as i64 {
                    warn!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        old_validity = state.uid_validity,
                        new_validity = uid_validity,
                        "UIDVALIDITY changed, re-syncing from scratch"
                    );
                    0
                } else {
                    state.last_seen_uid as u32
                }
            }
            None => 0,
        };

        // Step 1: UID-only scan to discover new message UIDs without
        // downloading bodies. This is cheap even for large mailboxes.
        rate_limiter.until_ready().await;
        let all_uids = conn.uid_fetch_uid_list(last_seen_uid + 1).await?;

        if all_uids.is_empty() {
            debug!(
                account = self.account.id,
                mailbox = self.mailbox,
                "no new messages"
            );
            crate::db::emails::upsert_mailbox_state(
                &self.pool,
                &self.account.id,
                &self.mailbox,
                uid_validity as i64,
                last_seen_uid as i64,
            )
            .await?;
            return Ok(0);
        }

        // Step 2: Apply initial sync limits (only on first sync).
        let uids_to_fetch_owned: Vec<u32> = if last_seen_uid == 0 {
            let mut uids = all_uids;

            // Filter by start date if configured.
            if let Some(start_date) = self.account.initial_sync_start_date {
                info!(
                    account = self.account.id,
                    mailbox = self.mailbox,
                    start_date = %start_date,
                    "applying initial_sync_start_date filter via UID SEARCH SINCE"
                );
                rate_limiter.until_ready().await;
                let since_set: std::collections::HashSet<u32> = conn
                    .uid_search_since(start_date)
                    .await?
                    .into_iter()
                    .collect();
                uids.retain(|uid| since_set.contains(uid));
            }

            // Apply max-messages cap (keep the most recent N).
            let skip = self
                .account
                .initial_sync_max_messages
                .map(|max| uids.len().saturating_sub(max as usize))
                .unwrap_or(0);
            uids[skip..].to_vec()
        } else {
            all_uids
        };
        let uids_to_fetch = &uids_to_fetch_owned[..];

        debug!(
            account = self.account.id,
            mailbox = self.mailbox,
            count = uids_to_fetch.len(),
            "fetching messages in batches"
        );

        let mut max_uid = last_seen_uid;
        let mut total_ingested = 0u64;
        let parser = MessageParser::default();

        // Step 3: Fetch and ingest in batches, checkpointing after each.
        'batch_loop: for chunk in uids_to_fetch.chunks(FETCH_BATCH_SIZE) {
            if self.token.is_cancelled() {
                break;
            }

            let uid_start = chunk[0];
            let uid_end = *chunk.last().expect("chunks are non-empty");

            rate_limiter.until_ready().await;
            let messages = conn.uid_fetch_range(uid_start, Some(uid_end)).await?;

            let mut batch_ingested = 0u64;
            for msg in &messages {
                if self.token.is_cancelled() {
                    break 'batch_loop;
                }

                let uid = msg.uid;
                batch_ingested += self.ingest_message(msg, &parser).await? as u64;
                // Never cover a failed insert with a later checkpoint. Errors
                // abort the batch before its state write; retries are idempotent.
                max_uid = max_uid.max(uid);
            }

            crate::db::emails::upsert_mailbox_state(
                &self.pool,
                &self.account.id,
                &self.mailbox,
                uid_validity as i64,
                max_uid as i64,
            )
            .await?;

            total_ingested += batch_ingested;
        }

        info!(
            account = self.account.id,
            mailbox = self.mailbox,
            ingested = total_ingested,
            last_seen_uid = max_uid,
            "sync complete"
        );

        Ok(total_ingested)
    }

    /// Parse and persist a single fetched message. Returns 1 for a new email,
    /// 0 for an existing email, and an error if persistence failed. The caller
    /// must not checkpoint a batch containing a failed persistence operation.
    async fn ingest_message(&self, msg: &FetchedMessage, parser: &MessageParser) -> Result<u8> {
        let uid = msg.uid;
        let parsed = parser.parse(&msg.raw_bytes);

        let (message_id, subject, sender, recipients, date) = match parsed {
            Some(ref parsed) => {
                let message_id = parsed.message_id().map(|s| s.to_string());
                let subject = parsed.subject().map(|s| s.to_string());
                let sender = parsed.from().and_then(|addrs| {
                    addrs
                        .first()
                        .and_then(|a| a.address.as_deref())
                        .map(|addr| addr.trim().to_lowercase())
                        .filter(|addr| !addr.is_empty())
                });
                let recipients = parsed.to().map(|addrs| {
                    let list: Vec<String> = addrs
                        .iter()
                        .map(|a| a.address.as_deref().unwrap_or("").to_string())
                        .collect();
                    serde_json::json!(list)
                });
                let date = parsed
                    .date()
                    .and_then(|d| chrono::DateTime::from_timestamp(d.to_timestamp(), 0));

                (message_id, subject, sender, recipients, date)
            }
            None => {
                warn!(
                    account = self.account.id,
                    mailbox = self.mailbox,
                    uid,
                    "failed to parse message, storing raw only"
                );
                (None, None, None, None, None)
            }
        };

        let raw_path = self
            .store
            .save(&self.account.id, &self.mailbox, uid, &msg.raw_bytes)
            .await
            .with_context(|| format!("saving raw message uid={uid}"))?;

        let new_email = NewEmail {
            account_id: self.account.id.clone(),
            mailbox_name: self.mailbox.clone(),
            uid: uid as i64,
            message_id,
            subject: subject.clone(),
            sender: sender.clone(),
            recipients,
            date,
            flags: msg.flags.clone(),
            raw_message_path: raw_path.to_string_lossy().to_string(),
            size_bytes: msg.size.map(|s| s as i64),
        };

        let new_event = NewEvent {
            event_type: "email_arrived".to_string(),
            account_id: self.account.id.clone(),
            mailbox_name: self.mailbox.clone(),
            email_id: None,
            payload: serde_json::json!({
                "uid": uid,
                "subject": subject,
                "sender": sender,
            }),
        };

        match crate::db::events::insert_email_with_event(&self.pool, &new_email, &new_event).await {
            Ok(Some((email_id, event_id))) => {
                debug!(
                    account = self.account.id,
                    mailbox = self.mailbox,
                    uid,
                    email_id,
                    event_id,
                    "ingested message"
                );
                crate::metrics::inc_messages_ingested(&self.account.id, &self.mailbox);
                crate::metrics::inc_events_created("email_arrived");
                Ok(1)
            }
            Ok(None) => {
                debug!(
                    account = self.account.id,
                    mailbox = self.mailbox,
                    uid,
                    "skipping already-ingested message"
                );
                Ok(0)
            }
            Err(e) => {
                warn!(
                    account = self.account.id,
                    mailbox = self.mailbox,
                    uid,
                    error = %e,
                    "failed to persist message, will retry next cycle"
                );
                Err(e)
                    .with_context(|| format!("persisting message uid={uid}; checkpoint unchanged"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    async fn mock_mailbox() -> (AccountConfig, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let account: AccountConfig = toml::from_str(&format!(
            r#"
            id = "test"
            imap_host = "127.0.0.1"
            imap_port = {}
            tls = false
            username = "test"
            password = "test"
            mailboxes = ["INBOX"]
        "#,
            listener.local_addr().unwrap().port()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut peer = BufReader::new(stream);
            peer.write_all(b"* OK mock ready\r\n").await.unwrap();
            for phase in 1..=3 {
                let mut line = String::new();
                peer.read_line(&mut line).await.unwrap();
                let tag = line.split_whitespace().next().unwrap();
                if phase == 1 {
                    assert!(line.contains(" LOGIN "));
                } else if phase == 2 {
                    assert!(line.contains(" UID FETCH 1:* "));
                    peer.write_all(
                        b"* 1 FETCH (UID 1)\r\n* 2 FETCH (UID 2)\r\n* 3 FETCH (UID 3)\r\n",
                    )
                    .await
                    .unwrap();
                } else {
                    assert!(line.contains(" UID FETCH 1:3 "));
                    // Deliberately out of order: a higher UID persists before
                    // the injected failure at UID 2. It must not be checkpointed.
                    for uid in [3, 2, 1] {
                        let raw =
                            format!("From: test@example.com\r\nSubject: Message {uid}\r\n\r\nBody");
                        let response = format!(
                            "* {uid} FETCH (UID {uid} FLAGS () BODY[] {{{}}}\r\n{raw})\r\n",
                            raw.len()
                        );
                        peer.write_all(response.as_bytes()).await.unwrap();
                    }
                }
                peer.write_all(format!("{tag} OK done\r\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        (account, server)
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn failed_insert_cannot_be_checkpointed_and_is_retried(pool: PgPool) -> Result<()> {
        crate::db::emails::upsert_mailbox_state(&pool, "test", "INBOX", 123, 0).await?;
        sqlx::query("ALTER TABLE emails ADD CONSTRAINT injected_failure CHECK (uid <> 2)")
            .execute(&pool)
            .await?;
        let dir = TempDir::new()?;
        let store = Arc::new(MessageStore::new(dir.path().to_str().unwrap()));
        let limiter = Arc::new(RateLimiter::direct(Quota::per_second(nonzero!(1000u32))));
        for attempt in 0..2 {
            let (account, server) = mock_mailbox().await;
            let mut conn = ImapConnection::connect(&account).await?;
            let watcher = MailboxWatcher::new(
                Arc::new(account),
                "INBOX".into(),
                pool.clone(),
                store.clone(),
                CancellationToken::new(),
            );
            let result = watcher.do_incremental_sync(&mut conn, 123, &limiter).await;
            server.await?;
            let state = crate::db::emails::get_mailbox_state(&pool, "test", "INBOX")
                .await?
                .unwrap();
            if attempt == 0 {
                assert!(result.is_err());
                assert_eq!(state.last_seen_uid, 0);
                let uids: Vec<i64> = sqlx::query_scalar("SELECT uid FROM emails ORDER BY uid")
                    .fetch_all(&pool)
                    .await?;
                assert_eq!(uids, vec![3]);
                sqlx::query("ALTER TABLE emails DROP CONSTRAINT injected_failure")
                    .execute(&pool)
                    .await?;
            } else {
                assert_eq!(result?, 2); // UID 3 was already durable.
                assert_eq!(state.last_seen_uid, 3);
                let emails: i64 = sqlx::query_scalar("SELECT count(*) FROM emails")
                    .fetch_one(&pool)
                    .await?;
                let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
                    .fetch_one(&pool)
                    .await?;
                assert_eq!((emails, events), (3, 3));
            }
        }
        Ok(())
    }
}
