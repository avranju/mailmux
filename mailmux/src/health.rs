use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::Request;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, middleware};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use metrics_exporter_prometheus::PrometheusHandle;
use nonzero_ext::nonzero;
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Shared state for health check endpoints.
#[derive(Clone)]
pub struct HealthState {
    pool: PgPool,
    ready: Arc<AtomicBool>,
    metrics_handle: Option<PrometheusHandle>,
    metrics_token: Option<String>,
    rate_limit: Arc<DefaultDirectRateLimiter>,
}

impl HealthState {
    pub fn new(
        pool: PgPool,
        metrics_handle: Option<PrometheusHandle>,
        metrics_token: Option<String>,
    ) -> Self {
        Self {
            pool,
            ready: Arc::new(AtomicBool::new(false)),
            metrics_handle,
            metrics_token,
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

pub async fn serve(addr: SocketAddr, state: HealthState, token: CancellationToken) {
    info!(%addr, "health check server starting");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(%addr, error = %e, "failed to bind health check server");
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

async fn metrics_handler(State(state): State<HealthState>, headers: HeaderMap) -> Response {
    if let Some(expected) = &state.metrics_token {
        let valid = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .is_some_and(|(scheme, token)| {
                scheme.eq_ignore_ascii_case("Bearer")
                    && bool::from(token.as_bytes().ct_eq(expected.as_bytes()))
            });
        if !valid {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized",
            )
                .into_response();
        }
    }
    match &state.metrics_handle {
        Some(handle) => (StatusCode::OK, handle.render()),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "metrics not available".to_string(),
        ),
    }
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn request(addr: SocketAddr, path: &str, authorization: &str) -> String {
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{authorization}\r\n").as_bytes()).await.unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn loopback_listener_requires_metrics_token_but_readiness_still_works() {
        let config: crate::config::GeneralConfig =
            toml::from_str("data_dir = '/tmp/mailmux'").unwrap();
        let listener = tokio::net::TcpListener::bind(SocketAddr::new(config.health_bind_addr, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@localhost/unused")
            .unwrap();
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .build_recorder()
            .handle();
        let state = HealthState::new(pool, Some(handle), Some("test-token".into()));
        state.set_ready();
        let token = CancellationToken::new();
        let server = tokio::spawn(serve_listener(listener, state, token.clone()));
        for authorization in [
            "",
            "Authorization: Bearer wrong\r\n",
            "Authorization: Basic dGVzdA==\r\n",
        ] {
            let response = request(addr, "/metrics", authorization).await;
            assert!(response.starts_with("HTTP/1.1 401"), "{response}");
            assert!(
                response
                    .to_ascii_lowercase()
                    .contains("www-authenticate: bearer")
            );
        }
        for scheme in ["Bearer", "bearer"] {
            assert!(
                request(
                    addr,
                    "/metrics",
                    &format!("Authorization: {scheme} test-token\r\n")
                )
                .await
                .starts_with("HTTP/1.1 200")
            );
        }
        assert!(
            request(addr, "/ready", "")
                .await
                .starts_with("HTTP/1.1 200")
        );
        token.cancel();
        server.await.unwrap();
    }
}

#[cfg(test)]
mod rate_limit_tests {
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
        let mut state = HealthState::new(pool, Some(handle), None);
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
