use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Spawns a task that listens for SIGTERM/SIGINT and cancels the token.
pub fn spawn_signal_handler(token: CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    error!("failed to register SIGTERM handler: {e}, only SIGINT will work");
                    // Fall back to ctrl_c only.
                    ctrl_c.await.ok();
                    info!("received SIGINT, initiating shutdown");
                    token.cancel();
                    return;
                }
            };

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
