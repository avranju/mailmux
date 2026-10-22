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

            let snapshot = if self.account.credentials_file.is_some() {
                match self.account.credentials() {
                    Ok(credentials) => Some(credentials),
                    Err(_) => {
                        warn!(
                            "IMAP credential file unavailable or invalid; waiting before reconnect"
                        );
                        tokio::select! {
                            _ = self.token.cancelled() => return Ok(()),
                            _ = tokio::time::sleep(Duration::from_secs(self.account.credential_reload_interval_secs)) => continue,
                        }
                    }
                }
            } else {
                None
            };
            let result = if let (Some(path), Some(previous)) =
                (&self.account.credentials_file, &snapshot)
            {
                tokio::select! {
                    result = self.sync_and_idle_cycle(&rate_limiter, poll_interval) => result,
                    changed = crate::credentials::wait_for_change(path, previous, Duration::from_secs(self.account.credential_reload_interval_secs), &self.token) => {
                        if !changed { return Ok(()); }
                        info!("IMAP credentials rotated; reconnecting mailbox session");
                        retry_attempt = 0;
                        continue;
                    }
                }
            } else {
                self.sync_and_idle_cycle(&rate_limiter, poll_interval).await
            };
            match result {
                Ok(()) => {
                    retry_attempt = 0;
                }
                Err(e) => {
                    let err_str = format!("{e:#}");
                    if err_str.contains("LOGIN rejected") || err_str.contains("authentication") {
                        if let (Some(path), Some(previous)) =
                            (&self.account.credentials_file, &snapshot)
                        {
                            warn!("IMAP authentication rejected; waiting for credential rotation");
                            if !crate::credentials::wait_for_change(
                                path,
                                previous,
                                Duration::from_secs(self.account.credential_reload_interval_secs),
                                &self.token,
                            )
                            .await
                            {
                                return Ok(());
                            }
                            retry_attempt = 0;
                            continue;
                        }
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
                if uid > max_uid {
                    max_uid = uid;
                }

                batch_ingested += self.ingest_message(msg, &parser).await? as u64;
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

    /// Parse and persist a single fetched message. Returns 1 on success, 0 on
    /// failure (failure is logged as a warning rather than propagated so that
    /// one bad message does not abort the entire batch).
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
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    async fn line(peer: &mut BufReader<TcpStream>) -> String {
        let mut value = String::new();
        match peer.read_line(&mut value).await {
            Ok(_) => value,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                String::new()
            }
            Err(error) => panic!("mock IMAP read failed: {error}"),
        }
    }
    async fn reply(peer: &mut BufReader<TcpStream>, command: &str, data: &str) {
        let tag = command.split_whitespace().next().unwrap();
        peer.write_all(format!("{data}{tag} OK done\r\n").as_bytes())
            .await
            .unwrap();
    }
    fn replace_secret(path: &std::path::Path, content: &str) {
        let next = path.with_extension("next");
        std::fs::write(&next, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::rename(next, path).unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn rotated_file_reauthenticates_idle_and_rejected_sessions_without_daemon_restart(
        pool: PgPool,
    ) -> Result<()> {
        for reject_first in [false, true] {
            let dir = tempfile::tempdir()?;
            let secret = dir.path().join("credentials.toml");
            replace_secret(&secret, "username='user1'\npassword='pass1'\n");
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let account: AccountConfig = toml::from_str(&format!(
                r#"
                id = "test"
                imap_host = "127.0.0.1"
                imap_port = {}
                tls = false
                credentials_file = {:?}
                credential_reload_interval_secs = 1
                mailboxes = ["INBOX"]
            "#,
                listener.local_addr()?.port(),
                secret.to_str().unwrap()
            ))?;
            let token = CancellationToken::new();
            let server_token = token.clone();
            let server = tokio::spawn(async move {
                for session in 0..2 {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut peer = BufReader::new(stream);
                    peer.write_all(b"* OK ready\r\n").await.unwrap();
                    let login = line(&mut peer).await;
                    let (user, password) = if session == 0 {
                        ("user1", "pass1")
                    } else {
                        ("user2", "pass2")
                    };
                    assert!(login.contains(" LOGIN "));
                    assert!(
                        login.contains(user) && login.contains(password),
                        "wrong credential generation"
                    );
                    if session == 0 && reject_first {
                        let tag = login.split_whitespace().next().unwrap();
                        peer.write_all(format!("{tag} NO LOGIN rejected\r\n").as_bytes())
                            .await
                            .unwrap();
                        replace_secret(&secret, "username='user2'\npassword='pass2'\n");
                        continue;
                    }
                    reply(&mut peer, &login, "").await;
                    let select = line(&mut peer).await;
                    assert!(select.contains(" SELECT "));
                    reply(
                        &mut peer,
                        &select,
                        "* OK [UIDVALIDITY 123] valid\r\n* 0 EXISTS\r\n",
                    )
                    .await;
                    let fetch = line(&mut peer).await;
                    assert!(fetch.contains(" UID FETCH "));
                    reply(&mut peer, &fetch, "").await;
                    let idle = line(&mut peer).await;
                    assert!(idle.contains(" IDLE"));
                    peer.write_all(b"+ idling\r\n").await.unwrap();
                    if session == 0 {
                        replace_secret(&secret, "invalid temporary update");
                        tokio::time::sleep(Duration::from_millis(1100)).await;
                        assert!(
                            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                                .await
                                .is_err(),
                            "invalid update restarted a healthy session"
                        );
                        replace_secret(&secret, "username='user2'\npassword='pass2'\n");
                        assert!(
                            line(&mut peer).await.is_empty(),
                            "rotation must close the old session"
                        );
                    } else {
                        server_token.cancel();
                        loop {
                            let command = line(&mut peer).await;
                            if command.is_empty() {
                                break;
                            }
                            if command == "DONE\r\n" {
                                let tag = idle.split_whitespace().next().unwrap();
                                let _ = peer
                                    .write_all(format!("{tag} OK done\r\n").as_bytes())
                                    .await;
                            } else if command.contains(" LOGOUT") {
                                let tag = command.split_whitespace().next().unwrap();
                                let _ = peer
                                    .write_all(
                                        format!("* BYE closing\r\n{tag} OK done\r\n").as_bytes(),
                                    )
                                    .await;
                                break;
                            } else {
                                panic!("unexpected command during shutdown");
                            }
                        }
                    }
                }
            });
            let watcher = MailboxWatcher::new(
                Arc::new(account),
                "INBOX".into(),
                pool.clone(),
                Arc::new(MessageStore::new(dir.path().to_str().unwrap())),
                token,
            );
            tokio::time::timeout(Duration::from_secs(10), watcher.run()).await??;
            server.await?;
        }
        Ok(())
    }
}
