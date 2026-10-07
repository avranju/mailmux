use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// An event in the append-only event log.
///
/// Persisted events always have a positive `id`. The identifier `id == 0`
/// is reserved for transient, non-persisted events constructed by the
/// historical backfill command (see `crate::backfill`); it never refers to a
/// row in the `events` table.
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
#[allow(dead_code)]
pub struct NewEvent {
    pub event_type: String,
    pub account_id: String,
    pub mailbox_name: String,
    pub email_id: Option<i64>,
    pub payload: serde_json::Value,
}

/// Insert an email and its corresponding event atomically in a single transaction.
/// Also sends a NOTIFY to the mailmux_events channel.
///
/// Returns `Some((email_id, event_id))` when the email is newly inserted.
/// Returns `None` when the email already exists (duplicate UID — no event is
/// created, preventing the processor pipeline from re-firing for an email that
/// has already been handled).
pub async fn insert_email_with_event(
    pool: &PgPool,
    email: &super::emails::NewEmail,
    event: &NewEvent,
) -> Result<Option<(i64, i64)>> {
    let mut tx = pool.begin().await.context("beginning transaction")?;

    // ON CONFLICT DO NOTHING so that a duplicate UID returns no row, letting
    // us distinguish a fresh insert from a re-fetch of an existing message.
    let email_id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO emails (account_id, mailbox_name, uid, message_id, subject, sender, recipients, date, flags, raw_message_path, size_bytes)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (account_id, mailbox_name, uid) DO NOTHING
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
    .fetch_optional(&mut *tx)
    .await
    .context("inserting email in transaction")?;

    let Some(email_id) = email_id else {
        // Email already exists — silently skip event creation so the processor
        // pipeline does not re-fire for an already-ingested message.
        tx.rollback()
            .await
            .context("rolling back duplicate email transaction")?;
        return Ok(None);
    };

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

    Ok(Some((email_id, event_id)))
}

/// Fetch events that have not completed a routing pass.
pub async fn get_unprocessed_events(pool: &PgPool, limit: i64) -> Result<Vec<Event>> {
    let rows = sqlx::query(
        r#"
        SELECT e.id, e.event_type, e.account_id, e.mailbox_name, e.email_id,
               e.payload, e.created_at
        FROM events e
        WHERE e.dispatched_at IS NULL
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

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = false)]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn dispatch_migration_preserves_existing_jobs_and_leaves_jobless_events_unrouted(
        pool: PgPool,
    ) -> Result<()> {
        sqlx::raw_sql(include_str!(
            "../../migrations/20240101000000_initial_schema.sql"
        ))
        .execute(&pool)
        .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/20260804000000_add_processor_jobs_output.sql"
        ))
        .execute(&pool)
        .await?;
        let with_job: i64 = sqlx::query_scalar("INSERT INTO events (event_type, account_id, mailbox_name, payload) VALUES ('email_arrived','test','INBOX','{}') RETURNING id")
            .fetch_one(&pool).await?;
        let without_job: i64 = sqlx::query_scalar("INSERT INTO events (event_type, account_id, mailbox_name, payload) VALUES ('email_arrived','test','Archive','{}') RETURNING id")
            .fetch_one(&pool).await?;
        let job_id: i64 = sqlx::query_scalar("INSERT INTO processor_jobs (event_id, processor_name, status, attempts, last_error, output) VALUES ($1,'existing','failed',3,'saved','{\"kept\":true}') RETURNING id")
            .bind(with_job).fetch_one(&pool).await?;
        let before: serde_json::Value =
            sqlx::query_scalar("SELECT to_jsonb(p) FROM processor_jobs p WHERE id=$1")
                .bind(job_id)
                .fetch_one(&pool)
                .await?;
        sqlx::raw_sql(include_str!(
            "../../migrations/20261008000000_add_event_dispatch_marker.sql"
        ))
        .execute(&pool)
        .await?;
        let after: serde_json::Value =
            sqlx::query_scalar("SELECT to_jsonb(p) FROM processor_jobs p WHERE id=$1")
                .bind(job_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(before, after);
        let dispatched: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT dispatched_at FROM events WHERE id=$1")
                .bind(with_job)
                .fetch_one(&pool)
                .await?;
        let undispatched: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT dispatched_at FROM events WHERE id=$1")
                .bind(without_job)
                .fetch_one(&pool)
                .await?;
        assert!(dispatched.is_some());
        assert!(undispatched.is_none());
        assert_eq!(
            get_unprocessed_events(&pool, 10)
                .await?
                .iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            vec![without_job]
        );
        Ok(())
    }
}
