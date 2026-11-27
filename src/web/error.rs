//! HTML error pages for the web layer.

use std::sync::Arc;

use askama::Template;
use askama_web::WebTemplate;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::error::public::PublicAppError;
use crate::robots::mark_private;
use crate::templates::filters;

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

#[derive(Template, WebTemplate)]
#[template(path = "error/4xx.html")]
struct ClientErrorPage {
    active_tab: &'static str,
    code: u16,
    reason: &'static str,
}

pub async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, NotFoundPage { active_tab: "" }).into_response()
}

/// The page whose heading names `status`.
fn error_page(status: StatusCode) -> Response {
    match status {
        StatusCode::NOT_FOUND => (status, NotFoundPage { active_tab: "" }).into_response(),
        StatusCode::SERVICE_UNAVAILABLE => {
            (status, UnavailablePage { active_tab: "" }).into_response()
        }
        _ if status.is_client_error() => (
            status,
            ClientErrorPage {
                active_tab: "",
                code: status.as_u16(),
                reason: status.canonical_reason().unwrap_or("Client Error"),
            },
        )
            .into_response(),
        _ => (status, InternalErrorPage { active_tab: "" }).into_response(),
    }
}

/// Maps a `PublicAppError` to an HTML response for the rendered routes —
/// avoids leaking the JSON envelope into the browser.
pub(crate) fn render_public_error(err: PublicAppError) -> Response {
    if let PublicAppError::Internal(e) = &err {
        tracing::error!(error = %e, "public status page internal error");
    }
    let mut resp = error_page(err.status());
    if matches!(err, PublicAppError::SignInRequired) {
        mark_private(&mut resp);
    }
    resp
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

impl WebError {
    /// See [`AppError::from_shared`].
    pub(crate) fn from_shared(err: Arc<Self>) -> Self {
        Self(Arc::try_unwrap(err).map_or_else(|shared| shared.0.duplicate(), |own| own.0))
    }
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        match &self.0 {
            AppError::NotFound { .. }
            | AppError::Gone { .. }
            | AppError::Forbidden
            | AppError::ForbiddenCoded { .. }
            | AppError::Unauthorized
            | AppError::SessionRequired => {}
            AppError::BadRequest { .. }
            | AppError::Conflict { .. }
            | AppError::Unprocessable { .. }
            | AppError::UnprocessableDetails { .. }
            | AppError::QuotaExceeded { .. }
            | AppError::RateLimited { .. }
            | AppError::PayloadTooLarge { .. }
            | AppError::UnsupportedMediaType { .. } => {
                tracing::warn!(error = %self.0, "web handler rejected request");
            }
            AppError::ServiceUnavailable { .. } => {
                tracing::warn!(error = %self.0, "web handler unavailable");
            }
            AppError::Internal { code, log } => {
                tracing::error!(error_code = code, detail = %log, "web handler failed");
            }
            err @ (AppError::Config(_)
            | AppError::Io(_)
            | AppError::BindAddr { .. }
            | AppError::Other(_)) => {
                // `%err` shows only the topmost context (anyhow Display).
                // `?err` walks the source chain so a `.context("foo")?` on
                // top of a ClickHouse / Postgres / reqwest error surfaces
                // the underlying line, not just our wrapper string.
                tracing::error!(error = ?err, "web handler failed");
            }
        }
        error_page(self.0.status())
    }
}

pub type WebResult<T> = std::result::Result<T, WebError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_caller_sharing_a_failed_load_gets_its_status() {
        let leader = Arc::new(WebError::from(AppError::not_found(
            crate::error::codes::ORG_NOT_FOUND,
            "org not found",
        )));
        let waiter = Arc::clone(&leader);
        let from_leader = WebError::from_shared(leader);
        let from_waiter = WebError::from_shared(waiter);
        for err in [from_leader, from_waiter] {
            assert_eq!(err.into_response().status(), StatusCode::NOT_FOUND);
        }
    }

    #[test]
    fn error_pages_render() {
        for html in [
            NotFoundPage { active_tab: "" }.render().unwrap(),
            InternalErrorPage { active_tab: "" }.render().unwrap(),
            UnavailablePage { active_tab: "" }.render().unwrap(),
            ClientErrorPage {
                active_tab: "",
                code: 409,
                reason: "Conflict",
            }
            .render()
            .unwrap(),
        ] {
            assert!(html.starts_with("<!doctype html>"));
            assert!(html.contains("Uptimepage"));
            assert!(
                !html.contains("data-navmenu"),
                "no app menu on an error page"
            );
        }
    }

    #[tokio::test]
    async fn html_pages_send_and_name_the_json_status() {
        let cases: [fn() -> AppError; 13] = [
            || AppError::not_found("X", "x"),
            || AppError::bad_request("X", "x"),
            || AppError::conflict("X", "x"),
            || AppError::unprocessable("X", "x"),
            || AppError::quota_exceeded("monitors", 5, 5, "free"),
            || AppError::payload_too_large("X", "x"),
            || AppError::gone("X", "x"),
            || AppError::RateLimited {
                scope: "x".into(),
                retry_after_secs: 1,
            },
            || AppError::SessionRequired,
            || AppError::Forbidden,
            || AppError::service_unavailable("X", "x"),
            || AppError::internal_with_context("X", "x"),
            || AppError::Other(anyhow::anyhow!("x")),
        ];
        for case in cases {
            let json = case().into_response().status();
            let html = WebError::from(case()).into_response();
            assert_eq!(html.status(), json, "{}", case());
            let body = axum::body::to_bytes(html.into_body(), usize::MAX)
                .await
                .unwrap();
            let heading = format!("</span>{} ", json.as_u16());
            assert!(
                String::from_utf8_lossy(&body).contains(&heading),
                "{}: heading does not name {json}",
                case()
            );
        }
    }
}
