//! Tool-execution errors.
//!
//! Input-validation and business errors are returned to the model as
//! `isError: true` structured data (`{code, message, retryable}`), not as
//! JSON-RPC protocol errors — so the model can read the failure and self-
//! correct. Protocol errors stay reserved for unknown-tool / malformed-request
//! / auth, which the transport + auth middleware handle before a tool ever
//! runs. The `Result<Json<T>, McpToolError>` return shape lets the `#[tool]`
//! macro derive the success `outputSchema` while routing `Err` through
//! `IntoCallToolResult` to a tool-execution error.

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::CallToolResult;
use serde_json::json;

use crate::error::AppError;

use super::audit::Outcome;

/// Stable machine codes the model (and our tests) can branch on.
pub mod codes {
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    pub const NOT_FOUND: &str = "not_found";
    pub const INSUFFICIENT_SCOPE: &str = "insufficient_scope";
    /// A write tool's confirmation prompt was declined or cancelled.
    pub const NOT_CONFIRMED: &str = "not_confirmed";
    /// The client failed the elicitation round trip, so no human ever decided.
    pub const CONFIRMATION_FAILED: &str = "confirmation_failed";
    pub const UNAUTHENTICATED: &str = "unauthenticated";
    /// No probe could run right now (no live agent in the region). The
    /// arguments were fine, so an identical retry can succeed.
    pub const PROBE_UNAVAILABLE: &str = "probe_unavailable";
    /// Every probe in the region was busy with other checks. Transient and
    /// scoped to one check, so a batch carries on past it.
    pub const PROBE_BUSY: &str = "probe_busy";
    /// The org's per-category rate limit was exhausted; retry after a delay.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// The resource is declared in Terraform, which would revert a write here.
    pub const MANAGED_EXTERNALLY: &str = "managed_externally";
    /// The resource moved between reading it and confirming the write, so the
    /// approved change no longer describes what would happen.
    pub const CONFLICT: &str = "conflict";
    pub const INTERNAL: &str = "internal";
}

/// A side-effect-free tool failure surfaced as structured `isError` content.
#[derive(Debug, Clone)]
pub struct McpToolError {
    pub code: &'static str,
    pub message: String,
    /// Hint to the caller: would an identical retry plausibly succeed later?
    pub retryable: bool,
    /// Refines `code` for the audit trail. Never sent to the caller.
    pub detail: Option<String>,
}

impl McpToolError {
    pub fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The code always leads, so one `LIKE 'not_confirmed%'` still spans rows
    /// written before refusals carried a reason.
    pub fn audit_detail(&self) -> String {
        match &self.detail {
            Some(d) => format!("{}:{d}", self.code),
            None => self.code.to_string(),
        }
    }

    /// Properties of the call, not of the item: a batch stops rather than
    /// collecting the identical error N times.
    pub fn is_fatal_to_batch(&self) -> bool {
        matches!(
            self.code,
            codes::INSUFFICIENT_SCOPE
                | codes::RATE_LIMITED
                | codes::UNAUTHENTICATED
                | codes::INTERNAL
                | codes::PROBE_UNAVAILABLE
        )
    }

    /// The message is the audit reason too; a validator may echo the refused
    /// value, never a credential.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::new(codes::INVALID_ARGUMENT, message.clone(), false).with_detail(message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(codes::NOT_FOUND, message, false)
    }

    /// Scopes are fixed when the token is minted, so say what the way out is
    /// or the caller retries forever.
    pub fn insufficient_scope(scope: &str) -> Self {
        Self::new(
            codes::INSUFFICIENT_SCOPE,
            format!(
                "the connector's token is missing the required scope `{scope}`; \
                 reconnect the connector to mint a token that carries it"
            ),
            false,
        )
    }

    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(codes::UNAUTHENTICATED, message, false)
    }

    /// The org's rate limit for this tool's category is exhausted. Retryable
    /// once the window refills.
    pub fn rate_limited(retry_after_secs: u32) -> Self {
        Self::new(
            codes::RATE_LIMITED,
            format!("rate limit exceeded; retry in {retry_after_secs}s"),
            true,
        )
    }

    /// A server-side fault (DB down, serialization). Retryable — the caller's
    /// arguments were fine. The detail is logged, not leaked.
    pub fn internal(context: impl AsRef<str>) -> Self {
        tracing::warn!(target: "mcp", detail = context.as_ref(), "mcp tool internal error");
        Self::new(
            codes::INTERNAL,
            "an internal error occurred; the request can be retried",
            true,
        )
    }
}

impl IntoCallToolResult for McpToolError {
    fn into_call_tool_result(self) -> Result<CallToolResult, rmcp::ErrorData> {
        // `structured_error` sets `is_error: true`; the Result glue in rmcp
        // keeps it that way, producing a tool-execution error rather than a
        // protocol error.
        Ok(CallToolResult::structured_error(json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "retryable": self.retryable,
            }
        })))
    }
}

/// A validator rejection is a caller fault, so it must not come back retryable.
pub(super) fn config_error(e: crate::error::AppError) -> McpToolError {
    match e {
        AppError::Internal { .. } | AppError::Other(_) => McpToolError::internal(e.to_string()),
        other => McpToolError::invalid_argument(other.to_string()),
    }
}

/// A refusal the target itself earns (a heartbeat has nothing to probe, a plan
/// won't run this flow) never becomes true by waiting, so marking it retryable
/// would loop the model against the check-now limiter.
pub(super) fn probe_dispatch_error(e: crate::error::AppError) -> McpToolError {
    match e {
        AppError::ServiceUnavailable {
            code: crate::error::codes::PROBE_BUSY,
            ..
        } => McpToolError::new(codes::PROBE_BUSY, e.to_string(), true),
        AppError::ServiceUnavailable { .. } => {
            McpToolError::new(codes::PROBE_UNAVAILABLE, e.to_string(), true)
        }
        AppError::Internal { .. } | AppError::Other(_) => McpToolError::internal(e.to_string()),
        other => McpToolError::invalid_argument(other.to_string()),
    }
}

/// Map a write-tool error to an audit outcome: server faults are `error`;
/// everything else (scope, confirmation, bad input, not-found) is a caller-side
/// `denied`.
pub(super) fn outcome_for(e: &McpToolError) -> Outcome {
    match e.code {
        codes::INTERNAL | codes::PROBE_UNAVAILABLE | codes::PROBE_BUSY => Outcome::Error,
        _ => Outcome::Denied,
    }
}
