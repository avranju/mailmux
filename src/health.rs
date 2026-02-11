use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Shared state for health check endpoints.
#[derive(Clone)]
pub struct HealthState {
    pool: PgPool,
    ready: Arc<AtomicBool>,
    metrics_handle: Option<PrometheusHandle>,
}

impl HealthState {
    pub fn new(pool: PgPool, metrics_handle: Option<PrometheusHandle>) -> Self {
        Self {
            pool,
            ready: Arc::new(AtomicBool::new(false)),
            metrics_handle,
        }
    }

    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }
}

/// Start the health check HTTP server.
pub async fn serve(port: u16, state: HealthState, token: CancellationToken) {
    let mut app = Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler));

    if state.metrics_handle.is_some() {
        app = app.route("/metrics", get(metrics_handler));
    }

    let app = app.with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(port, "health check server starting");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(port, error = %e, "failed to bind health check server");
            return;
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            token.cancelled().await;
        })
        .await
        .unwrap_or_else(|e| {
            error!(error = %e, "health check server error");
        });
}

async fn health_handler(State(state): State<HealthState>) -> impl IntoResponse {
    // Check DB connectivity
    match sqlx::query("SELECT 1").execute(&state.pool).await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unavailable"),
    }
}

async fn ready_handler(State(state): State<HealthState>) -> impl IntoResponse {
    if state.ready.load(Ordering::SeqCst) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

async fn metrics_handler(State(state): State<HealthState>) -> impl IntoResponse {
    match &state.metrics_handle {
        Some(handle) => (StatusCode::OK, handle.render()),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "metrics not available".to_string(),
        ),
    }
}
