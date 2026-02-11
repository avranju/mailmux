use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// A processor job tracking the state of processing an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorJob {
    pub id: i64,
    pub event_id: i64,
    pub processor_name: String,
    pub status: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Create a new processor job (pending).
pub async fn create_job(
    pool: &PgPool,
    event_id: i64,
    processor_name: &str,
) -> Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO processor_jobs (event_id, processor_name, status)
        VALUES ($1, $2, 'pending')
        ON CONFLICT (event_id, processor_name) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(event_id)
    .bind(processor_name)
    .fetch_one(pool)
    .await
    .context("creating processor job")?;

    Ok(id)
}

/// Update a job's status.
pub async fn update_job_status(
    pool: &PgPool,
    job_id: i64,
    status: &str,
    error: Option<&str>,
    next_retry_at: Option<DateTime<Utc>>,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE processor_jobs
        SET status = $2, last_error = $3, next_retry_at = $4,
            attempts = attempts + 1, updated_at = now()
        WHERE id = $1
        "#,
    )
    .bind(job_id)
    .bind(status)
    .bind(error)
    .bind(next_retry_at)
    .execute(pool)
    .await
    .context("updating job status")?;

    Ok(())
}

/// Get pending jobs for a specific processor.
pub async fn get_pending_jobs(
    pool: &PgPool,
    processor_name: &str,
    limit: i64,
) -> Result<Vec<ProcessorJob>> {
    let rows = sqlx::query(
        r#"
        SELECT id, event_id, processor_name, status, attempts, last_error,
               next_retry_at, created_at, updated_at
        FROM processor_jobs
        WHERE processor_name = $1 AND status = 'pending'
        ORDER BY id ASC
        LIMIT $2
        "#,
    )
    .bind(processor_name)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("fetching pending jobs")?;

    Ok(rows.into_iter().map(row_to_job).collect())
}

/// Get failed jobs that are ready to retry.
pub async fn get_retryable_jobs(pool: &PgPool, limit: i64) -> Result<Vec<ProcessorJob>> {
    let rows = sqlx::query(
        r#"
        SELECT id, event_id, processor_name, status, attempts, last_error,
               next_retry_at, created_at, updated_at
        FROM processor_jobs
        WHERE status = 'failed' AND next_retry_at IS NOT NULL AND next_retry_at <= now()
        ORDER BY next_retry_at ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("fetching retryable jobs")?;

    Ok(rows.into_iter().map(row_to_job).collect())
}

fn row_to_job(r: sqlx::postgres::PgRow) -> ProcessorJob {
    ProcessorJob {
        id: r.get("id"),
        event_id: r.get("event_id"),
        processor_name: r.get("processor_name"),
        status: r.get("status"),
        attempts: r.get("attempts"),
        last_error: r.get("last_error"),
        next_retry_at: r.get("next_retry_at"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}
