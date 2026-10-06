use crate::{
    error::AppError,
    http::AppState,
    models::{ReindexResponse, SearchRequest, SearchResponse},
    search::SearchError,
};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
};

fn map_search_error(error: SearchError) -> AppError {
    match error {
        SearchError::Invalid(message) => AppError::Invalid(message),
        SearchError::NotFound => AppError::NotFound,
        SearchError::Internal(error) => AppError::Internal(error),
    }
}

pub async fn search(
    State(st): State<AppState>,
    json: Result<Json<SearchRequest>, JsonRejection>,
) -> Result<Json<SearchResponse>, AppError> {
    let Json(req) = json.map_err(|error| {
        let message = error.to_string();
        if message.contains("length limit") || message.contains("body too large") {
            AppError::TooLarge
        } else {
            AppError::Invalid("invalid JSON request".into())
        }
    })?;
    st.search
        .search(req)
        .await
        .map(Json)
        .map_err(map_search_error)
}

pub async fn reindex(
    State(st): State<AppState>,
    Path((source, source_id)): Path<(String, String)>,
) -> Result<(axum::http::StatusCode, Json<ReindexResponse>), AppError> {
    crate::ingest::validate_identity(&source).map_err(|e| AppError::Invalid(e.to_string()))?;
    crate::ingest::validate_identity(&source_id).map_err(|e| AppError::Invalid(e.to_string()))?;
    let Some((id, state)) = st
        .repo
        .requeue(&source, &source_id)
        .await
        .map_err(AppError::Internal)?
    else {
        return Err(AppError::NotFound);
    };
    st.notify.notify_one();
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(ReindexResponse {
            source,
            source_id,
            document_id: id,
            index_state: state,
        }),
    ))
}
