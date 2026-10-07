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
    #[allow(dead_code)]
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

/// Atomically register eligible jobs and mark event routing complete.
pub async fn register_event_dispatch(
    pool: &PgPool,
    event_id: i64,
    processor_names: &[&str],
) -> Result<Vec<(i64, String)>> {
    let mut tx = pool.begin().await.context("beginning event dispatch")?;
    let dispatched: Option<Option<DateTime<Utc>>> =
        sqlx::query_scalar("SELECT dispatched_at FROM events WHERE id = $1 FOR UPDATE")
            .bind(event_id)
            .fetch_optional(&mut *tx)
            .await
            .context("locking event for dispatch")?;
    let Some(dispatched) = dispatched else {
        tx.commit().await?;
        return Ok(vec![]);
    };
    if dispatched.is_some() {
        tx.commit().await?;
        return Ok(vec![]);
    }
    let inserted = sqlx::query_as("INSERT INTO processor_jobs (event_id, processor_name, status) SELECT $1, unnest($2::text[]), 'pending' ON CONFLICT (event_id, processor_name) DO NOTHING RETURNING id, processor_name")
        .bind(event_id).bind(processor_names).fetch_all(&mut *tx).await.context("registering processor jobs")?;
    sqlx::query("UPDATE events SET dispatched_at=now() WHERE id=$1")
        .bind(event_id)
        .execute(&mut *tx)
        .await
        .context("marking event dispatched")?;
    tx.commit().await.context("committing event dispatch")?;
    Ok(inserted)
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

/// Reset and claim an existing job for explicit replay in one row-locked
/// operation. A scheduler sweep that already claimed the job wins; replay
/// never overwrites an in-progress execution.
pub async fn reset_and_claim_job_for_replay(pool: &PgPool, job_id: i64) -> Result<bool> {
    let mut tx = pool.begin().await.context("beginning replay claim")?;
    let result = sqlx::query(
        r#"
        UPDATE processor_jobs
        SET status = 'in_progress', attempts = 1, last_error = NULL,
            next_retry_at = NULL, output = NULL, updated_at = now()
        WHERE id = $1 AND status <> 'in_progress'
        "#,
    )
    .bind(job_id)
    .execute(&mut *tx)
    .await
    .context("resetting and claiming job for replay")?;
    tx.commit().await.context("committing replay claim")?;
    Ok(result.rows_affected() == 1)
}

/// Legacy reset helper retained for the focused state-reset test.
#[allow(dead_code)]
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

/// Get pending jobs and failed jobs whose retry time is due.
pub async fn get_runnable_jobs(pool: &PgPool, limit: i64) -> Result<Vec<ProcessorJob>> {
    let rows = sqlx::query(
        r#"
        SELECT id, event_id, processor_name, status, attempts, last_error,
               next_retry_at, created_at, updated_at, output
        FROM processor_jobs
        WHERE status = 'pending' OR (status = 'failed' AND next_retry_at IS NOT NULL AND next_retry_at <= now())
        ORDER BY COALESCE(next_retry_at, created_at) ASC, id ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("fetching retryable jobs")?;

    Ok(rows.into_iter().map(row_to_job).collect())
}

/// Atomically claim pending or due failed work.
pub async fn claim_job(pool: &PgPool, job_id: i64) -> Result<bool> {
    let result = sqlx::query("UPDATE processor_jobs SET status='in_progress', attempts=attempts+1, last_error=NULL, next_retry_at=NULL, output=NULL, updated_at=now() WHERE id=$1 AND (status='pending' OR (status='failed' AND next_retry_at IS NOT NULL AND next_retry_at <= now()))").bind(job_id).execute(pool).await.context("claiming processor job")?;
    Ok(result.rows_affected() == 1)
}

/// Abandon queued work that is no longer eligible without incrementing attempts.
pub async fn abandon_queued_job(pool: &PgPool, job_id: i64, reason: &str) -> Result<bool> {
    let result = sqlx::query("UPDATE processor_jobs SET status='abandoned', last_error=$2, next_retry_at=NULL, updated_at=now() WHERE id=$1 AND (status='pending' OR (status='failed' AND next_retry_at IS NOT NULL AND next_retry_at <= now()))").bind(job_id).bind(reason).execute(pool).await.context("abandoning queued processor job")?;
    Ok(result.rows_affected() == 1)
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
    async fn dispatch_registration_is_atomic_and_concurrent_safe(pool: PgPool) -> Result<()> {
        let event_id = create_test_event(&pool).await?;
        let (left, right) = tokio::join!(
            register_event_dispatch(&pool, event_id, &["one", "two"]),
            register_event_dispatch(&pool, event_id, &["one", "two"]),
        );
        let left = left?;
        let right = right?;
        assert_eq!(left.len() + right.len(), 2);
        assert_eq!(
            left.iter()
                .chain(&right)
                .map(|(_, name)| name.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
        assert!(
            register_event_dispatch(&pool, event_id, &["later"])
                .await?
                .is_empty()
        );

        let rollback_event = create_test_event(&pool).await?;
        sqlx::query("CREATE FUNCTION reject_dispatch_marker() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.dispatched_at IS NOT NULL THEN RAISE EXCEPTION 'test dispatch failure'; END IF; RETURN NEW; END $$")
            .execute(&pool).await?;
        sqlx::query("CREATE TRIGGER reject_dispatch_marker BEFORE UPDATE ON events FOR EACH ROW EXECUTE FUNCTION reject_dispatch_marker()")
            .execute(&pool).await?;
        assert!(
            register_event_dispatch(&pool, rollback_event, &["rolled_back"])
                .await
                .is_err()
        );
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM processor_jobs WHERE event_id=$1")
            .bind(rollback_event)
            .fetch_one(&pool)
            .await?;
        let dispatched: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT dispatched_at FROM events WHERE id=$1")
                .bind(rollback_event)
                .fetch_one(&pool)
                .await?;
        assert_eq!(jobs, 0);
        assert!(dispatched.is_none());
        sqlx::query("DROP TRIGGER reject_dispatch_marker ON events")
            .execute(&pool)
            .await?;
        sqlx::query("DROP FUNCTION reject_dispatch_marker()")
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn concurrent_claim_and_abandonment_preserve_state(pool: PgPool) -> Result<()> {
        let event_id = create_test_event(&pool).await?;
        let job_id = create_job(&pool, event_id, "test_proc").await?.unwrap();
        let runnable = get_runnable_jobs(&pool, 10).await?;
        assert!(
            runnable.iter().any(|job| job.id == job_id),
            "pending job should be recovered by the sweep"
        );
        let (a, b) = tokio::join!(claim_job(&pool, job_id), claim_job(&pool, job_id));
        assert_eq!(usize::from(a?) + usize::from(b?), 1);
        assert_eq!(get_job_by_id(&pool, job_id).await?.unwrap().attempts, 1);

        let queued = create_job(&pool, event_id, "queued").await?.unwrap();
        let previous_output = serde_json::json!({"kept": true});
        sqlx::query("UPDATE processor_jobs SET status='failed', attempts=4, last_error='old', next_retry_at=now()-interval '1 second', output=$2 WHERE id=$1")
            .bind(queued).bind(&previous_output).execute(&pool).await?;
        assert!(abandon_queued_job(&pool, queued, "not eligible").await?);
        let job = get_job_by_id(&pool, queued).await?.unwrap();
        assert_eq!(job.status, "abandoned");
        assert_eq!(job.attempts, 4);
        assert_eq!(job.output, Some(previous_output));
        assert!(job.next_retry_at.is_none());
        assert_eq!(job.last_error.as_deref(), Some("not eligible"));
        Ok(())
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
    async fn replay_and_sweep_claims_are_mutually_exclusive(pool: PgPool) -> Result<()> {
        let event_id = create_test_event(&pool).await?;
        let job_id = create_job(&pool, event_id, "test_proc").await?.unwrap();

        // Sweep wins first: replay must not reset or steal its in-progress job.
        assert!(claim_job(&pool, job_id).await?);
        assert!(!reset_and_claim_job_for_replay(&pool, job_id).await?);
        let job = get_job_by_id(&pool, job_id).await?.unwrap();
        assert_eq!(job.status, "in_progress");
        assert_eq!(job.attempts, 1);

        // After a completed prior run, replay atomically acquires ownership;
        // a subsequent sweep cannot claim the job a second time.
        update_job_status(
            &pool,
            job_id,
            "completed",
            None,
            None,
            None,
            AttemptsUpdate::None,
        )
        .await?;
        assert!(reset_and_claim_job_for_replay(&pool, job_id).await?);
        assert!(!claim_job(&pool, job_id).await?);
        let job = get_job_by_id(&pool, job_id).await?.unwrap();
        assert_eq!(job.status, "in_progress");
        assert_eq!(job.attempts, 1);
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
