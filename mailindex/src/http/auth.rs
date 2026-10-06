use crate::{error::AppError, http::AppState};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use constant_time_eq::constant_time_eq;

pub async fn require(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let Some(expected) = state.token else {
        return Ok(next.run(req).await);
    };
    let auth = req
        .headers()
        .get("authorization")
        .and_then(|x| x.to_str().ok())
        .unwrap_or("");
    let supplied = auth.strip_prefix("Bearer ").unwrap_or("");
    if supplied.is_empty() || !constant_time_eq(supplied.as_bytes(), expected.as_bytes()) {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(req).await)
}
