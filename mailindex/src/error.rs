use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("payload too large")]
    TooLarge,
    #[error("unsupported media type")]
    Unsupported,
    #[error("parse failed: {0}")]
    Parse(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, msg) = match &self {
            Self::Invalid(m) => (StatusCode::BAD_REQUEST, "invalid_request", m.as_str()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "document not found"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized"),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "request too large",
            ),
            Self::Unsupported => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "multipart/form-data is required",
            ),
            Self::Parse(m) => (StatusCode::UNPROCESSABLE_ENTITY, "parse_error", m.as_str()),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal server error",
            ),
        };
        (
            status,
            Json(ErrorBody {
                error: ErrorDetail { code, message: msg },
            }),
        )
            .into_response()
    }
}

impl From<turso::Error> for AppError {
    fn from(e: turso::Error) -> Self {
        Self::Internal(anyhow::anyhow!(e))
    }
}
