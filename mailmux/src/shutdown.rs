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
