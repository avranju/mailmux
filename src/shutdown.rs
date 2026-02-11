use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::info;

/// Spawns a task that listens for SIGTERM/SIGINT and cancels the token.
pub fn spawn_signal_handler(token: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to register SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => {
                info!("received SIGINT, initiating shutdown");
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, initiating shutdown");
            }
        }

        token.cancel();
    });
}

/// Waits for the cancellation token with a grace period.
/// Returns `true` if the grace period expired (forced shutdown).
pub async fn wait_with_grace_period(
    token: &CancellationToken,
    grace_period: Duration,
) -> bool {
    token.cancelled().await;
    info!(grace_secs = grace_period.as_secs(), "shutdown signal received, waiting for grace period");

    // Give tasks time to clean up
    tokio::time::sleep(grace_period).await;
    true
}
