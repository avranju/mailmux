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
use crate::imap::connection::ImapConnection;
use crate::store::MessageStore;

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
        let rate_limiter = Arc::new(RateLimiter::direct(
            Quota::per_second(
                std::num::NonZeroU32::new(rate_limit).unwrap_or(nonzero!(1u32)),
            ),
        ));

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
                    let delay = super::retry_delay(retry_attempt, 1000, 300_000);
                    warn!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        error = %e,
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
        rate_limiter: &Arc<RateLimiter<governor::state::NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>>,
        poll_interval: Duration,
    ) -> Result<()> {
        debug!(
            account = self.account.id,
            mailbox = self.mailbox,
            "connecting for sync cycle"
        );

        let mut conn = ImapConnection::connect(&self.account).await?;
        let (uid_validity, _exists) = conn.select(&self.mailbox).await?;

        // Initial sync
        self.do_incremental_sync(&mut conn, uid_validity, rate_limiter).await?;

        // Try IDLE mode — if it fails, fall back to polling
        loop {
            if self.token.is_cancelled() {
                let _ = conn.logout().await;
                return Ok(());
            }

            match conn.idle(&self.token).await {
                Ok(true) => {
                    // Got an update, do incremental sync
                    debug!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        "IDLE update received, syncing"
                    );
                    self.do_incremental_sync(&mut conn, uid_validity, rate_limiter).await?;
                }
                Ok(false) => {
                    // Cancelled
                    let _ = conn.logout().await;
                    return Ok(());
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
        rate_limiter: &Arc<RateLimiter<governor::state::NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>>,
        poll_interval: Duration,
    ) -> Result<()> {
        let mut conn = ImapConnection::connect(&self.account).await?;
        let (uid_validity, _exists) = conn.select(&self.mailbox).await?;

        self.do_incremental_sync(&mut conn, uid_validity, rate_limiter).await?;
        let _ = conn.logout().await;

        // Wait for next poll
        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = self.token.cancelled() => {}
        }

        Ok(())
    }

    /// Fetch new messages since last_seen_uid and persist them.
    async fn do_incremental_sync(
        &self,
        conn: &mut ImapConnection,
        uid_validity: u32,
        rate_limiter: &Arc<RateLimiter<governor::state::NotKeyed, governor::state::InMemoryState, governor::clock::DefaultClock>>,
    ) -> Result<()> {
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

        let fetch_from = last_seen_uid + 1;
        debug!(
            account = self.account.id,
            mailbox = self.mailbox,
            fetch_from,
            "fetching messages"
        );

        rate_limiter.until_ready().await;
        let messages = conn.uid_fetch_range(fetch_from, None).await?;

        if messages.is_empty() {
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
            return Ok(());
        }

        let mut max_uid = last_seen_uid;
        let mut ingested = 0u64;
        let parser = MessageParser::default();

        // Apply initial sync bounds
        let max_messages = self.account.initial_sync_max_messages.unwrap_or(u64::MAX);
        let messages_to_process: Vec<_> = if last_seen_uid == 0 {
            let skip = messages.len().saturating_sub(max_messages as usize);
            messages.into_iter().skip(skip).collect()
        } else {
            messages
        };

        for msg in messages_to_process {
            if self.token.is_cancelled() {
                break;
            }

            rate_limiter.until_ready().await;

            let uid = msg.uid;
            if uid > max_uid {
                max_uid = uid;
            }

            let parsed = parser.parse(&msg.raw_bytes);

            let (message_id, subject, sender, recipients, date) = match parsed {
                Some(ref parsed) => {
                    let message_id = parsed.message_id().map(|s| s.to_string());
                    let subject = parsed.subject().map(|s| s.to_string());
                    let sender = parsed.from().and_then(|addrs| {
                        addrs.first().map(|a| {
                            match (&a.name, &a.address) {
                                (Some(name), Some(addr)) => format!("{name} <{addr}>"),
                                (None, Some(addr)) => addr.to_string(),
                                (Some(name), None) => name.to_string(),
                                (None, None) => String::new(),
                            }
                        })
                    });
                    let recipients = parsed.to().map(|addrs| {
                        let list: Vec<String> = addrs.iter().map(|a| {
                            a.address.as_deref().unwrap_or("").to_string()
                        }).collect();
                        serde_json::json!(list)
                    });
                    let date = parsed.date().and_then(|d| {
                        chrono::DateTime::from_timestamp(d.to_timestamp(), 0)
                    });

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

            match crate::db::events::insert_email_with_event(&self.pool, &new_email, &new_event)
                .await
            {
                Ok((email_id, event_id)) => {
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
                    ingested += 1;
                }
                Err(e) => {
                    warn!(
                        account = self.account.id,
                        mailbox = self.mailbox,
                        uid,
                        error = %e,
                        "failed to persist message, will retry next cycle"
                    );
                }
            }
        }

        crate::db::emails::upsert_mailbox_state(
            &self.pool,
            &self.account.id,
            &self.mailbox,
            uid_validity as i64,
            max_uid as i64,
        )
        .await?;

        info!(
            account = self.account.id,
            mailbox = self.mailbox,
            ingested,
            last_seen_uid = max_uid,
            "sync complete"
        );

        Ok(())
    }
}
