use crate::{error::AppError, http::AppState, storage::models::IndexStatusCounts};
use axum::{Json, extract::State};

pub async fn get(State(st): State<AppState>) -> Result<Json<IndexStatusCounts>, AppError> {
    st.repo
        .status_counts()
        .await
        .map(Json)
        .map_err(AppError::Internal)
}
