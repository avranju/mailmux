use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// An event in the append-only event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub event_type: String,
    pub account_id: String,
    pub mailbox_name: String,
    pub email_id: Option<i64>,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Data needed to create a new event.
#[derive(Debug)]
pub struct NewEvent {
    pub event_type: String,
    pub account_id: String,
    pub mailbox_name: String,
    pub email_id: Option<i64>,
    pub payload: serde_json::Value,
}

/// Insert an event. Returns the event ID.
pub async fn insert_event(pool: &PgPool, event: &NewEvent) -> Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO events (event_type, account_id, mailbox_name, email_id, payload)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
    )
    .bind(&event.event_type)
    .bind(&event.account_id)
    .bind(&event.mailbox_name)
    .bind(event.email_id)
    .bind(&event.payload)
    .fetch_one(pool)
    .await
    .context("inserting event")?;

    Ok(id)
}

/// Insert an email and its corresponding event atomically in a single transaction.
/// Also sends a NOTIFY to the mailmux_events channel.
/// Returns (email_id, event_id).
pub async fn insert_email_with_event(
    pool: &PgPool,
    email: &super::emails::NewEmail,
    event: &NewEvent,
) -> Result<(i64, i64)> {
    let mut tx = pool.begin().await.context("beginning transaction")?;

    let email_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO emails (account_id, mailbox_name, uid, message_id, subject, sender, recipients, date, flags, raw_message_path, size_bytes)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (account_id, mailbox_name, uid) DO UPDATE SET updated_at = now()
        RETURNING id
        "#,
    )
    .bind(&email.account_id)
    .bind(&email.mailbox_name)
    .bind(email.uid)
    .bind(&email.message_id)
    .bind(&email.subject)
    .bind(&email.sender)
    .bind(&email.recipients)
    .bind(email.date)
    .bind(&email.flags)
    .bind(&email.raw_message_path)
    .bind(email.size_bytes)
    .fetch_one(&mut *tx)
    .await
    .context("inserting email in transaction")?;

    let event_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO events (event_type, account_id, mailbox_name, email_id, payload)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id
        "#,
    )
    .bind(&event.event_type)
    .bind(&event.account_id)
    .bind(&event.mailbox_name)
    .bind(Some(email_id))
    .bind(&event.payload)
    .fetch_one(&mut *tx)
    .await
    .context("inserting event in transaction")?;

    // Notify listeners
    sqlx::query("SELECT pg_notify('mailmux_events', $1)")
        .bind(event_id.to_string())
        .execute(&mut *tx)
        .await
        .context("sending NOTIFY")?;

    tx.commit().await.context("committing transaction")?;

    Ok((email_id, event_id))
}

/// Fetch unprocessed events (events that have no corresponding processor_jobs).
pub async fn get_unprocessed_events(pool: &PgPool, limit: i64) -> Result<Vec<Event>> {
    let rows = sqlx::query(
        r#"
        SELECT e.id, e.event_type, e.account_id, e.mailbox_name, e.email_id,
               e.payload, e.created_at
        FROM events e
        WHERE NOT EXISTS (
            SELECT 1 FROM processor_jobs pj WHERE pj.event_id = e.id
        )
        ORDER BY e.id ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("fetching unprocessed events")?;

    Ok(rows
        .into_iter()
        .map(|r| Event {
            id: r.get("id"),
            event_type: r.get("event_type"),
            account_id: r.get("account_id"),
            mailbox_name: r.get("mailbox_name"),
            email_id: r.get("email_id"),
            payload: r.get("payload"),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// Fetch an event by ID.
pub async fn get_event_by_id(pool: &PgPool, id: i64) -> Result<Option<Event>> {
    let row = sqlx::query(
        r#"
        SELECT id, event_type, account_id, mailbox_name, email_id, payload, created_at
        FROM events
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("fetching event by id")?;

    Ok(row.map(|r| Event {
        id: r.get("id"),
        event_type: r.get("event_type"),
        account_id: r.get("account_id"),
        mailbox_name: r.get("mailbox_name"),
        email_id: r.get("email_id"),
        payload: r.get("payload"),
        created_at: r.get("created_at"),
    }))
}
