use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::Request;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use metrics_exporter_prometheus::PrometheusHandle;
use nonzero_ext::nonzero;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Shared state for health check endpoints.
#[derive(Clone)]
pub struct HealthState {
    pool: PgPool,
    ready: Arc<AtomicBool>,
    metrics_handle: Option<PrometheusHandle>,
    rate_limit: Arc<DefaultDirectRateLimiter>,
}

impl HealthState {
    pub fn new(pool: PgPool, metrics_handle: Option<PrometheusHandle>) -> Self {
        Self {
            pool,
            ready: Arc::new(AtomicBool::new(false)),
            metrics_handle,
            rate_limit: Arc::new(RateLimiter::direct(
                Quota::per_second(nonzero!(10u32)).allow_burst(nonzero!(20u32)),
            )),
        }
    }

    pub fn set_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }
}

/// Start the health check HTTP server.
fn router(state: HealthState) -> Router {
    let mut app = Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler));

    if state.metrics_handle.is_some() {
        app = app.route("/metrics", get(metrics_handler));
    }

    app.layer(middleware::from_fn_with_state(
        state.clone(),
        enforce_rate_limit,
    ))
    .with_state(state)
}

async fn enforce_rate_limit(
    State(state): State<HealthState>,
    request: Request,
    next: Next,
) -> Response {
    if state.rate_limit.check().is_err() {
        metrics::counter!("mailmux_health_rate_limited_total").increment(1);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", "1")],
            "rate limit exceeded",
        )
            .into_response();
    }
    next.run(request).await
}

pub async fn serve(port: u16, state: HealthState, token: CancellationToken) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(port, "health check server starting");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(port, error = %e, "failed to bind health check server");
            return;
        }
    };

    serve_listener(listener, state, token).await;
}

async fn serve_listener(
    listener: tokio::net::TcpListener,
    state: HealthState,
    token: CancellationToken,
) {
    axum::serve(listener, router(state))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn request(addr: SocketAddr, path: &str) -> String {
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn every_endpoint_shares_budget_and_excess_health_requests_do_not_touch_database() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@localhost/unused")
            .unwrap();
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle();
        let mut state = HealthState::new(pool, Some(handle));
        // A long refill interval makes the HTTP boundary test deterministic.
        state.rate_limit = Arc::new(RateLimiter::direct(Quota::per_hour(nonzero!(1u32))));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let token = CancellationToken::new();
        let server = tokio::spawn(serve_listener(listener, state, token.clone()));
        assert!(request(addr, "/ready").await.starts_with("HTTP/1.1 503"));
        for path in ["/health", "/ready", "/metrics"] {
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(1), request(addr, path))
                    .await
                    .unwrap();
            assert!(response.starts_with("HTTP/1.1 429"), "{path}: {response}");
            assert!(response.to_ascii_lowercase().contains("retry-after: 1"));
        }
        token.cancel();
        server.await.unwrap();
    }
}
