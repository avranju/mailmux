use crate::{error::AppError, http::AppState, models::DocumentResponse, search::SearchError};
use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Params {
    pub max_chars: Option<usize>,
}

pub async fn get(
    State(st): State<AppState>,
    Path((source, id)): Path<(String, String)>,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Json<DocumentResponse>, AppError> {
    let Query(p) = query.map_err(|_| AppError::Invalid("invalid query parameters".into()))?;
    st.search
        .get(&source, &id, p.max_chars)
        .await
        .map(Json)
        .map_err(|error| match error {
            SearchError::Invalid(message) => AppError::Invalid(message),
            SearchError::NotFound => AppError::NotFound,
            SearchError::Internal(error) => AppError::Internal(error),
        })
}
