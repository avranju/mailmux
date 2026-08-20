use crate::{
    error::AppError,
    http::AppState,
    ingest::{normalize_message, validate_identity},
    models::IngestResponse,
};
use axum::{
    Json,
    extract::{Multipart, Path, State, multipart::MultipartRejection},
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Runs before Axum's `Multipart` extractor so a non-multipart upload gets
/// the service's JSON 415 response instead of the extractor's plain 400.
pub async fn require_multipart(request: axum::extract::Request, next: Next) -> Response {
    if request.method() == axum::http::Method::PUT
        && !request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(is_multipart_content_type)
    {
        return AppError::Unsupported.into_response();
    }
    next.run(request).await
}

fn is_multipart_content_type(value: &str) -> bool {
    value.split(';').next().is_some_and(|media_type| {
        media_type
            .trim()
            .eq_ignore_ascii_case("multipart/form-data")
    })
}

pub async fn upload(
    State(st): State<AppState>,
    Path((source, source_id)): Path<(String, String)>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<(StatusCode, Json<IngestResponse>), AppError> {
    let mut multipart =
        multipart.map_err(|_| AppError::Invalid("invalid multipart request".into()))?;
    if !headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_multipart_content_type)
    {
        return Err(AppError::Unsupported);
    }
    validate_identity(&source).map_err(|e| AppError::Invalid(e.to_string()))?;
    validate_identity(&source_id).map_err(|e| AppError::Invalid(e.to_string()))?;
    let mut metadata = None;
    let mut message = None;
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        let message = error.to_string();
        if message.contains("length limit") || message.contains("body too large") {
            AppError::TooLarge
        } else {
            AppError::Invalid(message)
        }
    })? {
        let name = field.name().unwrap_or("").to_owned();
        if name != "metadata" && name != "message" {
            return Err(AppError::Invalid("unexpected multipart field".into()));
        }
        let bytes = field.bytes().await.map_err(|_| AppError::TooLarge)?;
        if name == "metadata" {
            if metadata.is_some() {
                return Err(AppError::Invalid("duplicate metadata".into()));
            }
            metadata = Some(bytes)
        } else {
            if message.is_some() {
                return Err(AppError::Invalid("duplicate message".into()));
            }
            message = Some(bytes)
        }
    }
    let metadata = metadata.ok_or_else(|| AppError::Invalid("metadata is required".into()))?;
    let message = message.ok_or_else(|| AppError::Invalid("message is required".into()))?;
    let meta: serde_json::Value = serde_json::from_slice(&metadata)
        .map_err(|e| AppError::Invalid(format!("invalid metadata: {e}")))?;
    if !meta.is_object() {
        return Err(AppError::Invalid("metadata must be an object".into()));
    }
    let hash = crate::ingest::sha256(&message);
    if let Some(existing) = st
        .repo
        .existing_hash(&source, &source_id)
        .await
        .map_err(AppError::Internal)?
        && existing.hash == hash
    {
        if matches!(existing.state, crate::models::IndexState::Pending) {
            st.notify.notify_one()
        }
        let r = IngestResponse {
            source: source.clone(),
            source_id: source_id.clone(),
            document_id: existing.id,
            changed: false,
            index_state: existing.state,
            view_url: st.config.view_url(&source, &source_id),
        };
        return Ok((StatusCode::OK, Json(r)));
    }
    let cfg = st.config.content.clone();
    let src = source.clone();
    let sid = source_id.clone();
    let normalized =
        tokio::task::spawn_blocking(move || normalize_message(src, sid, meta, &message, &cfg))
            .await
            .map_err(|e| AppError::Internal(e.into()))?
            .map_err(|e| AppError::Parse(e.to_string()))?;
    let o = st
        .repo
        .upsert(&normalized)
        .await
        .map_err(AppError::Internal)?;
    if matches!(o.index_state, crate::models::IndexState::Pending) {
        st.notify.notify_one();
    }
    let status = if o.changed {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    let r = IngestResponse {
        source,
        source_id,
        document_id: o.document_id,
        changed: o.changed,
        index_state: o.index_state,
        view_url: st
            .config
            .view_url(&normalized.source, &normalized.source_id),
    };
    Ok((status, Json(r)))
}
