//! HTML error pages for the web layer.

use askama::Template;
use askama_web::WebTemplate;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::web::filters;

#[derive(Template, WebTemplate)]
#[template(path = "error/404.html")]
pub struct NotFoundPage {
    pub active_tab: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "error/500.html")]
pub struct InternalErrorPage {
    pub active_tab: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "error/503.html")]
pub struct UnavailablePage {
    pub active_tab: &'static str,
}

pub async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, NotFoundPage { active_tab: "" }).into_response()
}

/// Wrapper that re-renders `AppError` as an HTML error page instead of
/// the JSON envelope produced by `AppError`'s own `IntoResponse`.
/// Web handlers return `Result<TPage, WebError>` so users never see raw
/// `ApiError` JSON in the browser.
pub struct WebError(AppError);

impl<E: Into<AppError>> From<E> for WebError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        match self.0 {
            AppError::NotFound { .. } | AppError::Gone { .. } => {
                (StatusCode::NOT_FOUND, NotFoundPage { active_tab: "" }).into_response()
            }
            AppError::BadRequest { .. }
            | AppError::Conflict { .. }
            | AppError::Unprocessable { .. }
            | AppError::UnprocessableDetails { .. }
            | AppError::QuotaExceeded { .. }
            | AppError::RateLimited { .. }
            | AppError::PayloadTooLarge { .. }
            | AppError::UnsupportedMediaType { .. } => {
                tracing::warn!(error = %self.0, "web handler rejected request");
                (
                    StatusCode::BAD_REQUEST,
                    InternalErrorPage { active_tab: "" },
                )
                    .into_response()
            }
            AppError::Unauthorized | AppError::SessionRequired => (
                StatusCode::UNAUTHORIZED,
                InternalErrorPage { active_tab: "" },
            )
                .into_response(),
            AppError::Forbidden | AppError::ForbiddenCoded { .. } => {
                (StatusCode::FORBIDDEN, NotFoundPage { active_tab: "" }).into_response()
            }
            AppError::ServiceUnavailable { .. } => {
                tracing::warn!(error = %self.0, "web handler unavailable");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    InternalErrorPage { active_tab: "" },
                )
                    .into_response()
            }
            AppError::Internal { code, log } => {
                tracing::error!(error_code = code, detail = %log, "web handler failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    InternalErrorPage { active_tab: "" },
                )
                    .into_response()
            }
            ref err @ (AppError::Config(_)
            | AppError::Io(_)
            | AppError::BindAddr { .. }
            | AppError::Other(_)) => {
                // `%err` shows only the topmost context (anyhow Display).
                // `?err` walks the source chain so a `.context("foo")?` on
                // top of a ClickHouse / Postgres / reqwest error surfaces
                // the underlying line, not just our wrapper string.
                tracing::error!(error = ?err, "web handler failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    InternalErrorPage { active_tab: "" },
                )
                    .into_response()
            }
        }
    }
}

pub type WebResult<T> = std::result::Result<T, WebError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_pages_render() {
        for html in [
            NotFoundPage { active_tab: "" }.render().unwrap(),
            InternalErrorPage { active_tab: "" }.render().unwrap(),
            UnavailablePage { active_tab: "" }.render().unwrap(),
        ] {
            assert!(html.starts_with("<!doctype html>"));
            assert!(html.contains("Uptimepage"));
        }
    }
}
