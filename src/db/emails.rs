use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// Metadata for a stored email.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailRecord {
    pub id: i64,
    pub account_id: String,
    pub mailbox_name: String,
    pub uid: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub recipients: Option<serde_json::Value>,
    pub date: Option<DateTime<Utc>>,
    pub flags: Vec<String>,
    pub raw_message_path: String,
    pub size_bytes: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Data needed to insert a new email.
#[derive(Debug)]
pub struct NewEmail {
    pub account_id: String,
    pub mailbox_name: String,
    pub uid: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub recipients: Option<serde_json::Value>,
    pub date: Option<DateTime<Utc>>,
    pub flags: Vec<String>,
    pub raw_message_path: String,
    pub size_bytes: Option<i64>,
}


/// Sync state for a mailbox.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MailboxState {
    pub id: i64,
    pub account_id: String,
    pub mailbox_name: String,
    pub uid_validity: i64,
    pub last_seen_uid: i64,
    pub last_sync_at: Option<DateTime<Utc>>,
}

/// Get the current mailbox state.
pub async fn get_mailbox_state(
    pool: &PgPool,
    account_id: &str,
    mailbox: &str,
) -> Result<Option<MailboxState>> {
    let row = sqlx::query(
        r#"
        SELECT id, account_id, mailbox_name, uid_validity, last_seen_uid, last_sync_at
        FROM mailbox_states
        WHERE account_id = $1 AND mailbox_name = $2
        "#,
    )
    .bind(account_id)
    .bind(mailbox)
    .fetch_optional(pool)
    .await
    .context("fetching mailbox state")?;

    Ok(row.map(|r| MailboxState {
        id: r.get("id"),
        account_id: r.get("account_id"),
        mailbox_name: r.get("mailbox_name"),
        uid_validity: r.get("uid_validity"),
        last_seen_uid: r.get("last_seen_uid"),
        last_sync_at: r.get("last_sync_at"),
    }))
}

/// Upsert mailbox state (insert or update).
pub async fn upsert_mailbox_state(
    pool: &PgPool,
    account_id: &str,
    mailbox_name: &str,
    uid_validity: i64,
    last_seen_uid: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO mailbox_states (account_id, mailbox_name, uid_validity, last_seen_uid, last_sync_at)
        VALUES ($1, $2, $3, $4, now())
        ON CONFLICT (account_id, mailbox_name)
        DO UPDATE SET uid_validity = $3, last_seen_uid = $4, last_sync_at = now(), updated_at = now()
        "#,
    )
    .bind(account_id)
    .bind(mailbox_name)
    .bind(uid_validity)
    .bind(last_seen_uid)
    .execute(pool)
    .await
    .context("upserting mailbox state")?;

    Ok(())
}

/// Get an email record by ID.
pub async fn get_email_by_id(pool: &PgPool, id: i64) -> Result<Option<EmailRecord>> {
    let row = sqlx::query(
        r#"
        SELECT id, account_id, mailbox_name, uid, message_id, subject, sender, recipients,
               date, flags, raw_message_path, size_bytes, created_at, updated_at
        FROM emails
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("fetching email by id")?;

    Ok(row.map(|r| EmailRecord {
        id: r.get("id"),
        account_id: r.get("account_id"),
        mailbox_name: r.get("mailbox_name"),
        uid: r.get("uid"),
        message_id: r.get("message_id"),
        subject: r.get("subject"),
        sender: r.get("sender"),
        recipients: r.get("recipients"),
        date: r.get("date"),
        flags: r.get("flags"),
        raw_message_path: r.get("raw_message_path"),
        size_bytes: r.get("size_bytes"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }))
}
