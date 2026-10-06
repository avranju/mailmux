use crate::http::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse};

pub async fn health(State(st): State<AppState>) -> impl IntoResponse {
    match st.repo.health().await {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "unhealthy"),
    }
}

pub async fn ready(State(st): State<AppState>) -> impl IntoResponse {
    if st.ready.load(std::sync::atomic::Ordering::Acquire) {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}
