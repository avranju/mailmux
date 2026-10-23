use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use governor::DefaultDirectRateLimiter;
use mail_parser::MessageParser;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::AccountConfig;
use crate::db::emails::NewEmail;
use crate::db::events::NewEvent;
use crate::imap::connection::{AuthenticationFailure, FetchedMessage, IdleOutcome, ImapConnection};
use crate::store::MessageStore;

const FETCH_BATCH_SIZE: usize = 500;
const MAX_SUBJECT_BYTES: usize = 4096;
const MAX_SENDER_BYTES: usize = 1024;

fn bounded_subject(subject: &str) -> String {
    let mut end = subject.len().min(MAX_SUBJECT_BYTES);
    if end < subject.len() {
        while !subject.is_char_boundary(end) {
            end -= 1;
        }
        crate::metrics::inc_oversized_header("subject");
        warn!(
            limit = MAX_SUBJECT_BYTES,
            "oversized email subject truncated"
        );
    }
    subject[..end].to_owned()
}

fn bounded_sender(sender: &str) -> Option<String> {
    let sender = sender.trim();
    if sender.is_empty() {
        return None;
    }
    if sender.len() > MAX_SENDER_BYTES {
        crate::metrics::inc_oversized_header("sender");
        warn!(limit = MAX_SENDER_BYTES, "oversized email sender omitted");
        return None;
    }
    let normalized = sender.to_lowercase();
    if normalized.len() > MAX_SENDER_BYTES {
        crate::metrics::inc_oversized_header("sender");
        warn!(limit = MAX_SENDER_BYTES, "oversized email sender omitted");
        return None;
    }
    Some(normalized)
}

/// Watches a single mailbox, performing initial + incremental sync.
/// Supports IDLE when available, falls back to polling.
pub struct MailboxWatcher {
    account: Arc<AccountConfig>,
    mailbox: String,
    pool: PgPool,
    store: Arc<MessageStore>,
    token: CancellationToken,
    rate_limiter: Arc<DefaultDirectRateLimiter>,
}

impl MailboxWatcher {
    pub fn new(
        account: Arc<AccountConfig>,
        mailbox: String,
        pool: PgPool,
        store: Arc<MessageStore>,
        token: CancellationToken,
        rate_limiter: Arc<DefaultDirectRateLimiter>,
    ) -> Self {
        Self {
            account,
            mailbox,
            pool,
            store,
            token,
            rate_limiter,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let rate_limiter = self.rate_limiter.clone();

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
                    if e.is::<AuthenticationFailure>() {
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
                            "IMAP authentication failed; watcher stopped, check configured credentials"
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
        rate_limiter: &Arc<DefaultDirectRateLimiter>,
        poll_interval: Duration,
    ) -> Result<()> {
        debug!(
            account = self.account.id,
            mailbox = self.mailbox,
            "connecting for sync cycle"
        );

        let mut conn = ImapConnection::connect(&self.account).await?;
        let (uid_validity, exists) = conn.select(&self.mailbox).await?;
        debug!(
            exists,
            uid_validity, "selected mailbox; scanning UIDs to cover concurrent arrivals"
        );

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
        rate_limiter: &Arc<DefaultDirectRateLimiter>,
        poll_interval: Duration,
    ) -> Result<()> {
        let mut conn = ImapConnection::connect(&self.account).await?;
        let (uid_validity, exists) = conn.select(&self.mailbox).await?;
        debug!(
            exists,
            uid_validity, "selected mailbox; scanning UIDs to cover concurrent arrivals"
        );

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
        rate_limiter: &Arc<DefaultDirectRateLimiter>,
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
                    u32::try_from(state.last_seen_uid).context("invalid stored UID checkpoint")?
                }
            }
            None => 0,
        };

        // Step 1: UID-only scan to discover new message UIDs without
        // downloading bodies. This is cheap even for large mailboxes.
        let Some(next_uid) = last_seen_uid.checked_add(1) else {
            // UIDs cannot advance within this UIDVALIDITY generation.
            return Ok(0);
        };
        rate_limiter.until_ready().await;
        let all_uids = conn.uid_fetch_uid_list(next_uid).await?;

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
                .map(|max| {
                    uids.len()
                        .saturating_sub(usize::try_from(max).unwrap_or(usize::MAX))
                })
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

                batch_ingested += self.ingest_message(msg, uid_validity, &parser).await? as u64;
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
    async fn ingest_message(
        &self,
        msg: &FetchedMessage,
        uid_validity: u32,
        parser: &MessageParser,
    ) -> Result<u8> {
        let uid = msg.uid;
        let parsed = parser.parse(&msg.raw_bytes);

        let (message_id, subject, sender, recipients, date) = match parsed {
            Some(ref parsed) => {
                let message_id = parsed.message_id().map(|s| s.to_string());
                let subject = parsed.subject().map(bounded_subject);
                let sender = parsed.from().and_then(|addrs| {
                    addrs
                        .first()
                        .and_then(|a| a.address.as_deref())
                        .and_then(bounded_sender)
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
            .save(
                &self.account.id,
                &self.mailbox,
                uid_validity,
                uid,
                &msg.raw_bytes,
            )
            .await
            .with_context(|| format!("saving raw message uid={uid}"))?;

        let new_email = NewEmail {
            account_id: self.account.id.clone(),
            mailbox_name: self.mailbox.clone(),
            uid: uid as i64,
            uid_validity: uid_validity as i64,
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
mod account_limit_tests {
    use super::*;
    #[tokio::test]
    async fn peer_mailboxes_share_one_account_request_budget() {
        let account: AccountConfig = toml::from_str(
            r#"
            id='test'
            imap_host='localhost'
            username='test'
            password='test'
            rate_limit_per_second=1
            mailboxes=['INBOX','Archive']
        "#,
        )
        .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@localhost/unused")
            .unwrap();
        let manager = crate::imap::AccountManager::new(
            account,
            pool,
            Arc::new(MessageStore::new("/unused")),
            CancellationToken::new(),
        );
        let first = manager.watcher("INBOX".into());
        let second = manager.watcher("Archive".into());
        assert!(Arc::ptr_eq(&first.rate_limiter, &second.rate_limiter));
        assert!(first.rate_limiter.check().is_ok());
        assert!(second.rate_limiter.check().is_err());
    }
}

#[cfg(test)]
mod checkpoint_boundary_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn maximum_uid_checkpoint_does_not_wrap_or_issue_another_fetch(
        pool: PgPool,
    ) -> Result<()> {
        crate::db::emails::upsert_mailbox_state(&pool, "test", "INBOX", 123, i64::from(u32::MAX))
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let account: AccountConfig = toml::from_str(&format!(
            r#"
            id='test'
            imap_host='127.0.0.1'
            imap_port={}
            tls=false
            username='test'
            password='test'
            mailboxes=['INBOX']
        "#,
            listener.local_addr()?.port()
        ))?;
        let server = tokio::spawn(async move {
            let (peer, _) = listener.accept().await.unwrap();
            let mut peer = BufReader::new(peer);
            peer.write_all(b"* OK ready\r\n").await.unwrap();
            let mut login = String::new();
            peer.read_line(&mut login).await.unwrap();
            let tag = login.split_whitespace().next().unwrap();
            peer.write_all(format!("{tag} OK done\r\n").as_bytes())
                .await
                .unwrap();
            let mut command = String::new();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), peer.read_line(&mut command))
                    .await
                    .is_err(),
                "UID overflow generated a new command"
            );
        });
        let mut connection = ImapConnection::connect(&account).await?;
        let manager = crate::imap::AccountManager::new(
            account,
            pool.clone(),
            Arc::new(MessageStore::new("/unused")),
            CancellationToken::new(),
        );
        let watcher = manager.watcher("INBOX".into());
        assert_eq!(
            watcher
                .do_incremental_sync(&mut connection, 123, &manager.rate_limiter)
                .await?,
            0
        );
        assert_eq!(
            crate::db::emails::get_mailbox_state(&pool, "test", "INBOX")
                .await?
                .unwrap()
                .last_seen_uid,
            i64::from(u32::MAX)
        );
        server.await?;
        Ok(())
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

#[cfg(test)]
mod header_limit_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn header_bounds_preserve_utf8_and_never_turn_a_sender_prefix_into_an_address() {
        assert_eq!(bounded_subject("ordinary subject"), "ordinary subject");
        let oversized = format!("{}é", "x".repeat(MAX_SUBJECT_BYTES - 1));
        assert_eq!(bounded_subject(&oversized).len(), MAX_SUBJECT_BYTES - 1);
        assert_eq!(
            bounded_subject(&"x".repeat(MAX_SUBJECT_BYTES)).len(),
            MAX_SUBJECT_BYTES
        );
        assert_eq!(
            bounded_sender("  TEST@example.com "),
            Some("test@example.com".into())
        );
        assert_eq!(bounded_sender("  "), None);
        assert!(bounded_sender(&format!("{}@example.com", "a".repeat(MAX_SENDER_BYTES))).is_none());
    }

    #[test]
    fn generated_payload_has_a_bound_even_with_json_control_escaping() {
        let payload = serde_json::json!({
            "uid": u32::MAX,
            "subject": bounded_subject(&"\u{1}".repeat(MAX_SUBJECT_BYTES)),
            "sender": bounded_sender(&"\u{1}".repeat(MAX_SENDER_BYTES)),
        });
        assert!(serde_json::to_vec(&payload).unwrap().len() < 31 * 1024);
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn oversized_headers_are_bounded_in_email_and_event_but_raw_is_preserved(
        pool: PgPool,
    ) -> Result<()> {
        let account: AccountConfig = toml::from_str(
            r#"
            id = "test"
            imap_host = "localhost"
            username = "test"
            password = "test"
            mailboxes = ["INBOX"]
        "#,
        )?;
        let dir = TempDir::new()?;
        let store = Arc::new(MessageStore::new(dir.path().to_str().unwrap()));
        let watcher = MailboxWatcher::new(
            Arc::new(account),
            "INBOX".into(),
            pool.clone(),
            store,
            CancellationToken::new(),
        );
        let raw = format!(
            "From: {}@example.com\r\nSubject: {}\r\n\r\nBody",
            "a".repeat(MAX_SENDER_BYTES),
            "s".repeat(MAX_SUBJECT_BYTES + 1)
        )
        .into_bytes();
        let msg = FetchedMessage {
            uid: 1,
            flags: vec![],
            raw_bytes: raw.clone(),
            size: None,
        };
        assert_eq!(
            watcher
                .ingest_message(&msg, &MessageParser::default())
                .await?,
            1
        );
        let row: (String, Option<String>, String) =
            sqlx::query_as("SELECT subject, sender, raw_message_path FROM emails")
                .fetch_one(&pool)
                .await?;
        assert_eq!(row.0.len(), MAX_SUBJECT_BYTES);
        assert_eq!(row.1, None);
        assert_eq!(tokio::fs::read(row.2).await?, raw);
        let payload: serde_json::Value = sqlx::query_scalar("SELECT payload FROM events")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            payload["subject"].as_str().unwrap().len(),
            MAX_SUBJECT_BYTES
        );
        assert!(payload["sender"].is_null());
        assert!(serde_json::to_vec(&payload)?.len() < MAX_SUBJECT_BYTES + 128);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    async fn mock_mailbox(generation: u32) -> (AccountConfig, tokio::task::JoinHandle<()>) {
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
                        let raw = format!(
                            "From: test@example.com\r\nSubject: Generation {generation} Message {uid}\r\n\r\nBody"
                        );
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
    async fn uidvalidity_change_preserves_archives_and_creates_new_events(
        pool: PgPool,
    ) -> Result<()> {
        let dir = TempDir::new()?;
        let store = Arc::new(MessageStore::new(dir.path().to_str().unwrap()));
        let limiter = Arc::new(RateLimiter::direct(Quota::per_second(nonzero!(1000u32))));
        let mut original = Vec::new();
        for (attempt, generation) in [123, 124, 124].into_iter().enumerate() {
            if attempt == 2 {
                // Simulate a crash after durable messages but before checkpoint.
                crate::db::emails::upsert_mailbox_state(&pool, "test", "INBOX", 124, 0).await?;
            }
            let (account, server) = mock_mailbox(generation).await;
            let mut conn = ImapConnection::connect(&account).await?;
            let watcher = MailboxWatcher::new(
                Arc::new(account),
                "INBOX".into(),
                pool.clone(),
                store.clone(),
                CancellationToken::new(),
            );
            let count = watcher
                .do_incremental_sync(&mut conn, generation, &limiter)
                .await?;
            server.await?;
            assert_eq!(count, if attempt == 2 { 0 } else { 3 });
            let state = crate::db::emails::get_mailbox_state(&pool, "test", "INBOX")
                .await?
                .unwrap();
            assert_eq!(
                (state.uid_validity, state.last_seen_uid),
                (generation as i64, 3)
            );
            if attempt == 0 {
                let paths: Vec<String> =
                    sqlx::query_scalar("SELECT raw_message_path FROM emails ORDER BY id")
                        .fetch_all(&pool)
                        .await?;
                for path in paths {
                    let bytes = tokio::fs::read(&path).await?;
                    original.push((path, bytes));
                }
            }
        }
        for (path, bytes) in original {
            assert!(path.contains("uv_123"));
            assert_eq!(tokio::fs::read(&path).await?, bytes);
        }
        let emails: i64 = sqlx::query_scalar("SELECT count(*) FROM emails")
            .fetch_one(&pool)
            .await?;
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await?;
        let paths: i64 = sqlx::query_scalar("SELECT count(DISTINCT raw_message_path) FROM emails")
            .fetch_one(&pool)
            .await?;
        assert_eq!((emails, events, paths), (6, 6, 6));
        Ok(())
    }
}
