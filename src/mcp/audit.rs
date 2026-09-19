//! Audit trail for MCP write tools.
//!
//! Every state-changing tool — whether it executes, is denied (scope), or is
//! declined (elicitation) — records here: a structured tracing event (live in
//! Grafana/Loki) **and** a durable `mcp_audit` row (survives log rollover,
//! powers a future per-org activity view). Reads are never audited. The DB
//! write is best-effort *after* the action: if it fails the action still
//! happened, so we log loudly rather than unwind it.

use serde_json::Value;
use sqlx::PgPool;

use super::auth::McpAuth;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Error,
    /// Refused before any mutation (insufficient scope, or user declined).
    Denied,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Error => "error",
            Outcome::Denied => "denied",
        }
    }
}

/// Record one write-tool outcome. `arguments` is the tool's input (no secrets);
/// `detail` is a safe note (denial reason / sanitized error).
pub async fn record(
    pool: &PgPool,
    auth: &McpAuth,
    tool: &str,
    arguments: Value,
    outcome: Outcome,
    detail: Option<&str>,
) {
    tracing::info!(
        target: "mcp_audit",
        actor_type = "mcp",
        token_id = %auth.token_id,
        user_id = %auth.user_id.0,
        org_id = %auth.org.0,
        client = auth.client.as_deref().unwrap_or(""),
        tool,
        outcome = outcome.as_str(),
        detail = detail.unwrap_or(""),
        "mcp write tool",
    );
    let res = sqlx::query(
        "INSERT INTO mcp_audit \
           (token_id, user_id, org_id, client, tool, arguments, outcome, detail) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(auth.token_id)
    .bind(auth.user_id.0)
    .bind(auth.org.0)
    .bind(auth.client.as_deref())
    .bind(tool)
    .bind(sqlx::types::Json(arguments))
    .bind(outcome.as_str())
    .bind(detail)
    .execute(pool)
    .await;
    if let Err(e) = res {
        // The action already happened; a lost audit row must be loud, not fatal.
        tracing::error!(target: "mcp_audit", error = %e, tool, "mcp_audit insert failed");
    }
}
