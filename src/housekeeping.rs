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

async fn cleanup_old_events(pool: &PgPool, retention_days: u64) -> Result<()> {
    // Delete events older than retention period where all associated
    // processor_jobs are completed or abandoned
    let result = sqlx::query(
        r#"
        DELETE FROM events
        WHERE created_at < now() - ($1 || ' days')::interval
        AND NOT EXISTS (
            SELECT 1 FROM processor_jobs pj
            WHERE pj.event_id = events.id
            AND pj.status NOT IN ('completed', 'abandoned')
        )
        "#,
    )
    .bind(retention_days as i64)
    .execute(pool)
    .await?;

    let deleted = result.rows_affected();
    if deleted > 0 {
        info!(deleted, retention_days, "cleaned up old events");
    } else {
        debug!("no old events to clean up");
    }

    Ok(())
}
