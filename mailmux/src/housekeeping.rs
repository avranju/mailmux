use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

const CLEANUP_INTERVAL: Duration = Duration::from_secs(3600); // 1 hour

/// Background task that cleans up old processed events.
pub async fn run_event_cleanup(
    pool: PgPool,
    retention_days: u64,
    token: CancellationToken,
) -> Result<()> {
    info!(retention_days, "event cleanup task starting");

    let mut interval = tokio::time::interval(CLEANUP_INTERVAL);

    loop {
        tokio::select! {
            _ = token.cancelled() => {
                info!("event cleanup task shutting down");
                return Ok(());
            }
            _ = interval.tick() => {
                if let Err(e) = cleanup_old_events(&pool, retention_days).await {
                    error!(error = %e, "event cleanup failed");
                }
            }
        }
    }
}

pub(crate) async fn cleanup_old_events(pool: &PgPool, retention_days: u64) -> Result<()> {
    let mut tx = pool.begin().await?;

    // Lock eligible events so a concurrent job insert (which takes a key-share
    // lock on its referenced event) cannot race the child/job deletion.
    let event_ids = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT e.id
        FROM events e
        WHERE e.created_at < now() - ($1 || ' days')::interval
        AND NOT EXISTS (
            SELECT 1 FROM processor_jobs pj
            WHERE pj.event_id = e.id
            AND pj.status NOT IN ('completed', 'abandoned')
        )
        FOR UPDATE OF e
        "#,
    )
    .bind(retention_days as i64)
    .fetch_all(&mut *tx)
    .await?;

    if event_ids.is_empty() {
        tx.commit().await?;
        debug!("no old events to clean up");
        return Ok(());
    }

    // processor_jobs.event_id references events(id) without ON DELETE CASCADE,
    // so remove terminal jobs first. Recheck the child rows when deleting events
    // in case a non-terminal job appeared while the candidates were selected.
    sqlx::query(
        r#"
        DELETE FROM processor_jobs
        WHERE event_id = ANY($1)
        AND status IN ('completed', 'abandoned')
        "#,
    )
    .bind(&event_ids)
    .execute(&mut *tx)
    .await?;

    let result = sqlx::query(
        r#"
        DELETE FROM events e
        WHERE e.id = ANY($1)
        AND NOT EXISTS (
            SELECT 1 FROM processor_jobs pj WHERE pj.event_id = e.id
        )
        "#,
    )
    .bind(&event_ids)
    .execute(&mut *tx)
    .await?;

    let deleted = result.rows_affected();
    tx.commit().await?;

    if deleted > 0 {
        info!(deleted, retention_days, "cleaned up old events");
    } else {
        debug!("no old events to clean up");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use sqlx::PgPool;

    use super::cleanup_old_events;

    async fn create_event(pool: &PgPool, age_days: i32) -> Result<i64> {
        Ok(sqlx::query_scalar(
            r#"
            INSERT INTO events (event_type, account_id, mailbox_name, created_at)
            VALUES ('email_arrived', 'test', 'INBOX', now() - make_interval(days => $1))
            RETURNING id
            "#,
        )
        .bind(age_days)
        .fetch_one(pool)
        .await?)
    }

    async fn create_job(pool: &PgPool, event_id: i64, status: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO processor_jobs (event_id, processor_name, status) VALUES ($1, 'test', $2)",
        )
        .bind(event_id)
        .bind(status)
        .execute(pool)
        .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn cleanup_deletes_terminal_jobs_before_old_events(pool: PgPool) -> Result<()> {
        let old_completed = create_event(&pool, 60).await?;
        create_job(&pool, old_completed, "completed").await?;
        let old_pending = create_event(&pool, 60).await?;
        create_job(&pool, old_pending, "pending").await?;
        let recent_completed = create_event(&pool, 2).await?;
        create_job(&pool, recent_completed, "completed").await?;
        let old_without_jobs = create_event(&pool, 60).await?;

        cleanup_old_events(&pool, 30).await?;

        for deleted_id in [old_completed, old_without_jobs] {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE id = $1)")
                    .bind(deleted_id)
                    .fetch_one(&pool)
                    .await?;
            assert!(!exists, "eligible event {deleted_id} should be deleted");
        }
        for retained_id in [old_pending, recent_completed] {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM events WHERE id = $1)")
                    .bind(retained_id)
                    .fetch_one(&pool)
                    .await?;
            assert!(exists, "ineligible event {retained_id} should remain");
        }
        let remaining_terminal_jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM processor_jobs WHERE event_id = $1")
                .bind(old_completed)
                .fetch_one(&pool)
                .await?;
        assert_eq!(remaining_terminal_jobs, 0);

        let pending_jobs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM processor_jobs WHERE event_id = $1 AND status = 'pending'",
        )
        .bind(old_pending)
        .fetch_one(&pool)
        .await?;
        assert_eq!(pending_jobs, 1);

        Ok(())
    }
}
