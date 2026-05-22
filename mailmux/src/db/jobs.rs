use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// Controls how the `attempts` column is updated when changing a job's status.
#[derive(Debug, Clone, Copy)]
pub enum AttemptsUpdate {
    /// Leave the attempts count unchanged.
    None,
    /// Increment the current attempts count by one.
    Increment,
    /// Set the attempts count to the given value.
    Set(i32),
}

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
/// Returns `Some(id)` on success, or `None` if the job already exists
/// (duplicate dispatch — `ON CONFLICT DO NOTHING`).
pub async fn create_job(pool: &PgPool, event_id: i64, processor_name: &str) -> Result<Option<i64>> {
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
    .fetch_optional(pool)
    .await
    .context("creating processor job")?;

    Ok(id)
}

/// Update a job's status. Use `AttemptsUpdate::Increment` when transitioning
/// to `in_progress` so that each dispatch cycle counts as exactly one attempt.
/// Use `AttemptsUpdate::Set(n)` to overwrite the attempt count (e.g. resetting
/// to zero before a replay), and `AttemptsUpdate::None` to leave it unchanged.
pub async fn update_job_status(
    pool: &PgPool,
    job_id: i64,
    status: &str,
    error: Option<&str>,
    next_retry_at: Option<DateTime<Utc>>,
    attempts_update: AttemptsUpdate,
) -> Result<()> {
    match attempts_update {
        AttemptsUpdate::None => {
            let sql = r#"
                UPDATE processor_jobs
                SET status = $2, last_error = $3, next_retry_at = $4,
                    attempts = attempts,
                    updated_at = now()
                WHERE id = $1
                "#.to_string();
            sqlx::query(&sql)
                .bind(job_id)
                .bind(status)
                .bind(error)
                .bind(next_retry_at)
                .execute(pool)
                .await
                .context("updating job status")?;
        }
        AttemptsUpdate::Increment => {
            let sql = r#"
                UPDATE processor_jobs
                SET status = $2, last_error = $3, next_retry_at = $4,
                    attempts = attempts + 1,
                    updated_at = now()
                WHERE id = $1
                "#.to_string();
            sqlx::query(&sql)
                .bind(job_id)
                .bind(status)
                .bind(error)
                .bind(next_retry_at)
                .execute(pool)
                .await
                .context("updating job status")?;
        }
        AttemptsUpdate::Set(n) => {
            sqlx::query(
                r#"
                UPDATE processor_jobs
                SET status = $2, last_error = $3, next_retry_at = $4,
                    attempts = $5,
                    updated_at = now()
                WHERE id = $1
                "#,
            )
            .bind(job_id)
            .bind(status)
            .bind(error)
            .bind(next_retry_at)
            .bind(n)
            .execute(pool)
            .await
            .context("updating job status")?;
        }
    }

    Ok(())
}

/// Get a single job by its ID.
pub async fn get_job_by_id(pool: &PgPool, job_id: i64) -> Result<Option<ProcessorJob>> {
    let row = sqlx::query(
        r#"
        SELECT id, event_id, processor_name, status, attempts, last_error,
               next_retry_at, created_at, updated_at
        FROM processor_jobs
        WHERE id = $1
        "#,
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await
    .context("fetching job by id")?;

    Ok(row.map(row_to_job))
}

/// Get a job by event_id and processor_name.
pub async fn get_job_by_event_and_processor(
    pool: &PgPool,
    event_id: i64,
    processor_name: &str,
) -> Result<Option<ProcessorJob>> {
    let row = sqlx::query(
        r#"
        SELECT id, event_id, processor_name, status, attempts, last_error,
               next_retry_at, created_at, updated_at
        FROM processor_jobs
        WHERE event_id = $1 AND processor_name = $2
        "#,
    )
    .bind(event_id)
    .bind(processor_name)
    .fetch_optional(pool)
    .await
    .context("fetching job by event and processor")?;

    Ok(row.map(row_to_job))
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
