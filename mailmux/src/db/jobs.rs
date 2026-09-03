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
    /// The complete serialized ProcessorOutput from the last execution,
    /// or NULL when no output was produced (e.g. timeout, anyhow error).
    pub output: Option<serde_json::Value>,
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

/// Update a job's status and optionally persist or clear output.
///
/// Use `AttemptsUpdate::Increment` when transitioning to `in_progress` so that
/// each dispatch cycle counts as exactly one attempt.
/// Use `AttemptsUpdate::None` to leave it unchanged.
///
/// Pass `Some(&serialized_output)` to persist a ProcessorOutput, or `None` to
/// clear output (e.g. when entering `in_progress` before a retry/replay).
pub async fn update_job_status(
    pool: &PgPool,
    job_id: i64,
    status: &str,
    error: Option<&str>,
    next_retry_at: Option<DateTime<Utc>>,
    output: Option<&serde_json::Value>,
    attempts_update: AttemptsUpdate,
) -> Result<()> {
    match attempts_update {
        AttemptsUpdate::None => {
            sqlx::query(
                r#"
                UPDATE processor_jobs
                SET status = $2, last_error = $3, next_retry_at = $4,
                    attempts = attempts, output = $5,
                    updated_at = now()
                WHERE id = $1
                "#,
            )
            .bind(job_id)
            .bind(status)
            .bind(error)
            .bind(next_retry_at)
            .bind(output)
            .execute(pool)
            .await
            .context("updating job status")?;
        }
        AttemptsUpdate::Increment => {
            sqlx::query(
                r#"
                UPDATE processor_jobs
                SET status = $2, last_error = $3, next_retry_at = $4,
                    attempts = attempts + 1, output = $5,
                    updated_at = now()
                WHERE id = $1
                "#,
            )
            .bind(job_id)
            .bind(status)
            .bind(error)
            .bind(next_retry_at)
            .bind(output)
            .execute(pool)
            .await
            .context("updating job status")?;
        }
    }

    Ok(())
}

/// Atomically reset an existing job for replay: set status to pending,
/// zero out attempts, clear last_error / next_retry_at / output.
pub async fn reset_job_for_replay(pool: &PgPool, job_id: i64) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE processor_jobs
        SET status = 'pending', attempts = 0, last_error = NULL,
            next_retry_at = NULL, output = NULL,
            updated_at = now()
        WHERE id = $1
        "#,
    )
    .bind(job_id)
    .execute(pool)
    .await
    .context("resetting job for replay")?;

    Ok(())
}

/// Get a single job by its ID.
pub async fn get_job_by_id(pool: &PgPool, job_id: i64) -> Result<Option<ProcessorJob>> {
    let row = sqlx::query(
        r#"
        SELECT id, event_id, processor_name, status, attempts, last_error,
               next_retry_at, created_at, updated_at, output
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
               next_retry_at, created_at, updated_at, output
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
               next_retry_at, created_at, updated_at, output
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
        output: r.get("output"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_event(pool: &PgPool) -> Result<i64> {
        let event_id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO events (event_type, account_id, mailbox_name, payload)
            VALUES ('email_arrived', 'test', 'INBOX', '{}'::jsonb)
            RETURNING id
            "#,
        )
        .fetch_one(pool)
        .await
        .context("creating test event")?;
        Ok(event_id)
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_reset_job_for_replay_cleans_output(pool: PgPool) -> Result<()> {
        // Create an event first to satisfy the foreign key.
        let event_id = create_test_event(&pool).await?;

        // Seed a job with some output and attempts.
        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO processor_jobs (event_id, processor_name, status, attempts, output)
            VALUES ($1, 'test_proc', 'failed', 3, '{"success":false,"message":"old error"}'::jsonb)
            RETURNING id
            "#,
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await?;

        // Reset for replay.
        reset_job_for_replay(&pool, id).await?;

        let job = get_job_by_id(&pool, id).await?.expect("job should exist");
        assert_eq!(job.status, "pending");
        assert_eq!(job.attempts, 0);
        assert!(job.last_error.is_none());
        assert!(job.next_retry_at.is_none());
        assert!(job.output.is_none());

        // Now complete the replay with a new output.
        let new_output = serde_json::json!({
            "success": true,
            "message": "replayed successfully",
            "metadata": { "outcome": "posted" }
        });
        update_job_status(
            &pool,
            id,
            "completed",
            None,
            None,
            Some(&new_output),
            AttemptsUpdate::Increment,
        )
        .await?;

        let job = get_job_by_id(&pool, id).await?.expect("job should exist");
        assert_eq!(job.status, "completed");
        assert_eq!(job.attempts, 1);
        assert_eq!(job.output, Some(new_output));

        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_update_job_status_clears_output(pool: PgPool) -> Result<()> {
        let event_id = create_test_event(&pool).await?;

        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO processor_jobs (event_id, processor_name, status, output)
            VALUES ($1, 'test_proc', 'pending', '{"success":true}'::jsonb)
            RETURNING id
            "#,
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await?;

        // Transition to in_progress, clearing output.
        update_job_status(
            &pool,
            id,
            "in_progress",
            None,
            None,
            None,
            AttemptsUpdate::Increment,
        )
        .await?;

        let job = get_job_by_id(&pool, id).await?.expect("job should exist");
        assert_eq!(job.status, "in_progress");
        assert!(job.output.is_none());

        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_update_job_status_persists_failure_output(pool: PgPool) -> Result<()> {
        let event_id = create_test_event(&pool).await?;

        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO processor_jobs (event_id, processor_name, status)
            VALUES ($1, 'test_proc', 'in_progress')
            RETURNING id
            "#,
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await?;

        let failure_output = serde_json::json!({
            "success": false,
            "message": "processing failed",
            "metadata": { "outcome": "error" }
        });
        update_job_status(
            &pool,
            id,
            "failed",
            Some("processing failed"),
            Some(Utc::now()),
            Some(&failure_output),
            AttemptsUpdate::None,
        )
        .await?;

        let job = get_job_by_id(&pool, id).await?.expect("job should exist");
        assert_eq!(job.status, "failed");
        assert_eq!(job.output, Some(failure_output));
        assert_eq!(job.last_error, Some("processing failed".to_string()));

        Ok(())
    }

    /// Verify that a successful replay completion stores output.message as
    /// last_error (preserving the previous behavior) while also persisting
    /// the serialized output.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_replay_success_preserves_last_error(pool: PgPool) -> Result<()> {
        let event_id = create_test_event(&pool).await?;

        let id = sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO processor_jobs (event_id, processor_name, status)
            VALUES ($1, 'test_proc', 'pending')
            RETURNING id
            "#,
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await?;

        let replay_output = serde_json::json!({
            "success": true,
            "message": "replayed successfully",
            "metadata": { "outcome": "posted" }
        });

        // Simulate the replay success path: pass output.message as the error
        // argument (last_error) while also persisting the serialized output.
        update_job_status(
            &pool,
            id,
            "completed",
            Some("replayed successfully"),
            None,
            Some(&replay_output),
            AttemptsUpdate::None,
        )
        .await?;

        let job = get_job_by_id(&pool, id).await?.expect("job should exist");
        assert_eq!(job.status, "completed");
        assert_eq!(job.output, Some(replay_output));
        // last_error should contain the output message, matching the previous
        // replay behavior.
        assert_eq!(job.last_error, Some("replayed successfully".to_string()));

        Ok(())
    }
}
