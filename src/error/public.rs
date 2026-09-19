use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;
use utoipa::ToSchema;

/// Top-level envelope for every 4xx/5xx response on the public surface.
///
/// Deliberately narrower than [`crate::error::ApiError`]: no `field`,
/// no `details`, no `trace_id`. Internal debugging info never leaks
/// to anonymous callers.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicApiError {
    pub error: PublicApiErrorBody,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PublicApiErrorBody {
    /// Stable machine-readable code (UPPER_SNAKE_CASE).
    #[schema(example = "NOT_FOUND")]
    pub code: &'static str,
    /// Human-readable, safe to display.
    #[schema(example = "The requested resource does not exist.")]
    pub message: &'static str,
}

pub mod codes {
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    pub const INVALID_DAYS: &str = "INVALID_DAYS";
    pub const STATUS_DATA_UNAVAILABLE: &str = "STATUS_DATA_UNAVAILABLE";
}

#[derive(Debug, Error)]
pub enum PublicAppError {
    #[error("not found")]
    NotFound,
    #[error("invalid days parameter")]
    InvalidDays,
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    #[error("status data unavailable")]
    Unavailable,
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl PublicAppError {
    fn parts(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                codes::NOT_FOUND,
                "The requested resource does not exist.",
            ),
            Self::InvalidDays => (
                StatusCode::BAD_REQUEST,
                codes::INVALID_DAYS,
                "Query parameter 'days' must be between 1 and 365.",
            ),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, codes::BAD_REQUEST, msg),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::STATUS_DATA_UNAVAILABLE,
                "Status data is currently unavailable. Please retry shortly.",
            ),
            Self::Internal(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::STATUS_DATA_UNAVAILABLE,
                "Status data is currently unavailable. Please retry shortly.",
            ),
        }
    }
}

impl IntoResponse for PublicAppError {
    fn into_response(self) -> Response {
        if let Self::Internal(err) = &self {
            tracing::error!(error = %err, "public surface internal error");
        }
        let (status, code, message) = self.parts();
        (
            status,
            Json(PublicApiError {
                error: PublicApiErrorBody { code, message },
            }),
        )
            .into_response()
    }
}

impl From<crate::error::AppError> for PublicAppError {
    fn from(err: crate::error::AppError) -> Self {
        // AppError carries internal context — collapse to a generic Internal
        // so the public envelope never reveals it.
        Self::Internal(anyhow::anyhow!("{err}"))
    }
}
