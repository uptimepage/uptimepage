use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

use crate::api::error::{ApiError, ApiErrorBody, codes};

pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(#[from] config::ConfigError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid bind address {addr}: {source}")]
    BindAddr {
        addr: String,
        #[source]
        source: std::net::AddrParseError,
    },

    #[error("{message}")]
    NotFound { code: &'static str, message: String },

    #[error("{message}")]
    BadRequest {
        code: &'static str,
        message: String,
        field: Option<String>,
    },

    #[error("{message}")]
    PayloadTooLarge { code: &'static str, message: String },

    #[error("{message}")]
    UnsupportedMediaType { code: &'static str, message: String },

    #[error("{message}")]
    Conflict { code: &'static str, message: String },

    #[error("{message}")]
    Unprocessable { code: &'static str, message: String },

    /// 422 with a structured `details` payload — e.g. account deletion
    /// blocked because the caller solo-owns orgs that still have members
    /// (`details.orgs` carries the offending slugs + ids).
    #[error("{message}")]
    UnprocessableDetails {
        code: &'static str,
        message: String,
        details: serde_json::Value,
    },

    /// 410 Gone — the resource existed but is permanently gone. Used by
    /// account recovery once the grace window has elapsed and the hard-purge
    /// has run: the token verified, but there is nothing left to restore.
    #[error("{message}")]
    Gone { code: &'static str, message: String },

    /// 503 — no probe available to serve the request; a pure control plane
    /// leaves probing to agents.
    #[error("{message}")]
    ServiceUnavailable { code: &'static str, message: String },

    /// A resource quota would be exceeded. Renders 422 with the stable
    /// machine-readable `details` block (`quota`, `current`, `limit`,
    /// `plan`) UI clients branch on. `code` is `QUOTA_EXCEEDED` or the
    /// specific `MIN_CHECK_INTERVAL`.
    #[error("{message}")]
    QuotaExceeded {
        code: &'static str,
        message: String,
        quota: &'static str,
        current: i64,
        limit: i64,
        plan: String,
    },

    /// A rate limit was exceeded. Renders 429 with a `Retry-After` header
    /// and a `details` block (`scope`, `retry_after_secs`).
    #[error("rate limited on {scope}")]
    RateLimited {
        scope: String,
        retry_after_secs: u32,
    },

    #[error("authentication required")]
    Unauthorized,

    /// 401 — a valid API token hit a browser-session-only endpoint. Same status
    /// as [`Self::Unauthorized`] but a distinct `SESSION_REQUIRED` code so token
    /// users aren't told their token is invalid.
    #[error("browser session required")]
    SessionRequired,

    #[error("access denied")]
    Forbidden,

    /// Coded forbidden — same HTTP 403 as [`Self::Forbidden`] but with a
    /// stable error code and message so handlers can carry context (e.g.
    /// `EMAIL_NOT_VERIFIED`).
    #[error("{message}")]
    ForbiddenCoded { code: &'static str, message: String },

    /// Internal failure carrying a *private* log detail. The client gets a
    /// generic 500 + stable `code`; `log` is written only to the error log
    /// (the auth-isolated path) and never serialised into the response body.
    /// This makes "attach context without leaking PII/secrets" the easy
    /// default — callers put the identifying bit in `log`, not in a public
    /// message.
    #[error("internal error")]
    Internal { code: &'static str, log: String },

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl AppError {
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::BadRequest {
            code,
            message: message.into(),
            field: None,
        }
    }

    pub fn bad_request_field(
        code: &'static str,
        message: impl Into<String>,
        field: impl Into<String>,
    ) -> Self {
        Self::BadRequest {
            code,
            message: message.into(),
            field: Some(field.into()),
        }
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::NotFound {
            code,
            message: message.into(),
        }
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::Conflict {
            code,
            message: message.into(),
        }
    }

    pub fn payload_too_large(code: &'static str, message: impl Into<String>) -> Self {
        Self::PayloadTooLarge {
            code,
            message: message.into(),
        }
    }

    pub fn unsupported_media_type(code: &'static str, message: impl Into<String>) -> Self {
        Self::UnsupportedMediaType {
            code,
            message: message.into(),
        }
    }

    pub fn unprocessable(code: &'static str, message: impl Into<String>) -> Self {
        Self::Unprocessable {
            code,
            message: message.into(),
        }
    }

    pub fn forbidden_code(code: &'static str, message: impl Into<String>) -> Self {
        Self::ForbiddenCoded {
            code,
            message: message.into(),
        }
    }

    pub fn unprocessable_details(
        code: &'static str,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self::UnprocessableDetails {
            code,
            message: message.into(),
            details,
        }
    }

    pub fn gone(code: &'static str, message: impl Into<String>) -> Self {
        Self::Gone {
            code,
            message: message.into(),
        }
    }

    pub fn service_unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self::ServiceUnavailable {
            code,
            message: message.into(),
        }
    }

    /// 500 with a stable `code` and a private `log` line. Use this instead of
    /// interpolating an email / URL / token into an `anyhow!` message: the
    /// detail goes to the error log only, the body stays generic.
    pub fn internal_with_context(code: &'static str, log: impl Into<String>) -> Self {
        Self::Internal {
            code,
            log: log.into(),
        }
    }

    /// Build a `QUOTA_EXCEEDED` 422 with the standard details block.
    pub fn quota_exceeded(
        quota: &'static str,
        current: i64,
        limit: i64,
        plan: impl Into<String>,
    ) -> Self {
        let plan = plan.into();
        Self::QuotaExceeded {
            code: codes::QUOTA_EXCEEDED,
            message: format!(
                "{quota} limit reached: {current} of {limit} used on the {plan} plan."
            ),
            quota,
            current,
            limit,
            plan,
        }
    }

    /// Build the specific `MIN_CHECK_INTERVAL` 422 (requested interval below
    /// the plan floor).
    pub fn min_check_interval(requested: i64, minimum: i64, plan: impl Into<String>) -> Self {
        let plan = plan.into();
        Self::QuotaExceeded {
            code: codes::MIN_CHECK_INTERVAL,
            message: format!(
                "check interval {requested}s is below the {minimum}s minimum on the {plan} plan."
            ),
            quota: "min_check_interval_secs",
            current: requested,
            limit: minimum,
            plan,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Rate limiting needs a `Retry-After` header alongside the JSON body,
        // so it can't go through the `(status, body)` tail used by the rest.
        if let AppError::RateLimited {
            scope,
            retry_after_secs,
        } = self
        {
            tracing::debug!(code = codes::RATE_LIMITED, %scope, "request rate-limited");
            let body = ApiErrorBody {
                code: codes::RATE_LIMITED,
                message: format!(
                    "Too many {scope} requests. Try again in {retry_after_secs} seconds."
                ),
                field: None,
                details: Some(serde_json::json!({
                    "scope": scope,
                    "retry_after_secs": retry_after_secs,
                })),
                trace_id: None,
            };
            let mut resp = (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ApiError { error: body }),
            )
                .into_response();
            if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after_secs.to_string()) {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, v);
            }
            return resp;
        }

        let (status, body) = match self {
            AppError::NotFound { code, message } => {
                (StatusCode::NOT_FOUND, ApiErrorBody::new(code, message))
            }
            AppError::BadRequest {
                code,
                message,
                field,
            } => (
                StatusCode::BAD_REQUEST,
                ApiErrorBody {
                    code,
                    message,
                    field,
                    details: None,
                    trace_id: None,
                },
            ),
            AppError::PayloadTooLarge { code, message } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ApiErrorBody::new(code, message),
            ),
            AppError::UnsupportedMediaType { code, message } => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ApiErrorBody::new(code, message),
            ),
            AppError::Conflict { code, message } => {
                (StatusCode::CONFLICT, ApiErrorBody::new(code, message))
            }
            AppError::Unprocessable { code, message } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorBody::new(code, message),
            ),
            AppError::UnprocessableDetails {
                code,
                message,
                details,
            } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorBody {
                    code,
                    message,
                    field: None,
                    details: Some(details),
                    trace_id: None,
                },
            ),
            AppError::Gone { code, message } => {
                (StatusCode::GONE, ApiErrorBody::new(code, message))
            }
            AppError::ServiceUnavailable { code, message } => (
                StatusCode::SERVICE_UNAVAILABLE,
                ApiErrorBody::new(code, message),
            ),
            AppError::QuotaExceeded {
                code,
                message,
                quota,
                current,
                limit,
                plan,
            } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorBody {
                    code,
                    message,
                    field: None,
                    details: Some(serde_json::json!({
                        "quota": quota,
                        "current": current,
                        "limit": limit,
                        "plan": plan,
                    })),
                    trace_id: None,
                },
            ),
            // Handled above with a Retry-After header; unreachable here but
            // the match must stay exhaustive.
            AppError::RateLimited { .. } => unreachable!("RateLimited returns early"),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                ApiErrorBody::new(codes::UNAUTHORIZED, "authentication required"),
            ),
            AppError::SessionRequired => (
                StatusCode::UNAUTHORIZED,
                ApiErrorBody::new(
                    codes::SESSION_REQUIRED,
                    "this endpoint requires a browser session, not an API token",
                ),
            ),
            AppError::Forbidden => (
                StatusCode::FORBIDDEN,
                ApiErrorBody::new(codes::FORBIDDEN, "access denied"),
            ),
            AppError::ForbiddenCoded { code, message } => {
                (StatusCode::FORBIDDEN, ApiErrorBody::new(code, message))
            }
            AppError::Internal { code, log } => {
                tracing::error!(error_code = code, detail = %log, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiErrorBody::new(code, "internal error"),
                )
            }
            ref err @ (AppError::Config(_)
            | AppError::Io(_)
            | AppError::BindAddr { .. }
            | AppError::Other(_)) => {
                tracing::error!(error = %err, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiErrorBody::new(codes::INTERNAL, "internal error"),
                )
            }
        };
        if status.is_client_error() {
            tracing::debug!(code = body.code, status = %status, "request rejected");
        }
        (status, Json(ApiError { error: body })).into_response()
    }
}
