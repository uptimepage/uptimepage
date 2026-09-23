//! Plumbing every tool leans on: the pool, rate limits, target loads, the
//! audit row, and the public URL of a status page.

use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::domain::WriteSource;
use crate::domain::target::Target;
use crate::public_status::urls::{public_base, public_status_url};
use crate::quotas::ratelimit::{RateLimitCategory, RateLimitKey};
use crate::storage::{ClampedRange, TimeRange};

use crate::mcp::audit::{self, Outcome};
use crate::mcp::auth::McpAuth;
use crate::mcp::error::{McpToolError, codes, outcome_for};

use super::McpServer;
use super::text::sanitize_data;

/// How far back to look for an open incident when reporting `since`.
const SINCE_LOOKBACK_DAYS: i64 = 30;

impl McpServer {
    pub(super) fn require_pool(&self) -> Result<&sqlx::PgPool, McpToolError> {
        self.state
            .db
            .as_ref()
            .ok_or_else(|| McpToolError::internal("db unavailable"))
    }

    /// Enforce the account's rate limit for `category` at the tool layer. The
    /// `/mcp` middleware buckets every JSON-RPC call as a read (the tool name
    /// isn't in the URL); probe-spawning and write tools pass the stricter
    /// category here. A plan-resolution error degrades to "no app-side limit" —
    /// the reads budget and Caddy's per-IP tier still hold the line.
    pub(super) async fn enforce_rate_limit(
        &self,
        org: crate::domain::OrgId,
        category: RateLimitCategory,
    ) -> Result<(), McpToolError> {
        let Ok(plan) = self.state.quotas.limit_for_org(org).await else {
            return Ok(());
        };
        let Ok(Some(account)) = self.state.quotas.account_for_org(org).await else {
            return Ok(());
        };
        self.state
            .rate_limits
            .check(
                RateLimitKey::Account(account, category),
                "per_account",
                &plan,
            )
            .map_err(|d| McpToolError::rate_limited(d.retry_after_secs))
    }

    /// Naming the kind beats an empty answer, which a model reads as "no runs yet".
    pub(super) async fn require_flow(
        &self,
        org: crate::domain::OrgId,
        id: Uuid,
    ) -> Result<(), McpToolError> {
        let target = self.load_target(org, id).await?;
        if !matches!(target.check, crate::domain::CheckSpec::Flow(_)) {
            return Err(McpToolError::invalid_argument(format!(
                "monitor is a `{}` check; flow runs exist only for `flow` monitors",
                target.check.kind()
            )));
        }
        Ok(())
    }

    /// A trailing window, held to what the plan retains at per-check detail.
    /// One clamp per tool, so every field of a response covers the same span
    /// rather than a rollup read reaching past the raw one beside it.
    pub(super) async fn clamped_raw_window(
        &self,
        org: crate::domain::OrgId,
        span: Duration,
    ) -> Result<ClampedRange, McpToolError> {
        let now = Utc::now();
        self.state
            .quotas
            .clamp_raw(
                org,
                TimeRange {
                    from: now - span,
                    to: now,
                },
            )
            .await
            .map_err(|e| McpToolError::internal(format!("resolve window: {e}")))
    }

    /// Load a target in the org, or a tool not-found error.
    pub(super) async fn load_target(
        &self,
        org: crate::domain::OrgId,
        id: Uuid,
    ) -> Result<Target, McpToolError> {
        self.state
            .target_store
            .get(org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("get monitor: {e}")))?
            .ok_or_else(|| McpToolError::not_found("monitor not found"))
    }

    /// The load every config write goes through, so the Terraform guard cannot
    /// be left off a new one.
    pub(super) async fn load_writable_target(
        &self,
        org: crate::domain::OrgId,
        id: Uuid,
    ) -> Result<Target, McpToolError> {
        let target = self.load_target(org, id).await?;
        deny_terraform(&target)?;
        Ok(target)
    }

    /// Record exactly one audit row for a write tool's outcome, then return the
    /// result unchanged. Success → `success`; a caller-fault error (scope,
    /// confirmation, bad input, not-found) → `denied`; a server fault → `error`.
    pub(super) async fn finish<T>(
        &self,
        pool: &sqlx::PgPool,
        auth: &McpAuth,
        tool: &str,
        args_json: serde_json::Value,
        result: Result<T, McpToolError>,
    ) -> Result<T, McpToolError> {
        let unconfirmed = auth
            .unconfirmed()
            .map(|reason| format!("unconfirmed:{reason}"));
        let (outcome, detail) = match &result {
            Ok(_) => (Outcome::Success, unconfirmed),
            Err(e) => {
                if e.code == codes::CONFIRMATION_FAILED {
                    tracing::warn!(
                        target: "mcp",
                        org_id = %auth.org.0,
                        token_id = %auth.token_id,
                        client = auth.client.as_deref().unwrap_or(""),
                        tool,
                        detail = e.audit_detail(),
                        "mcp write refused: client could not confirm"
                    );
                }
                // Appended, so the refusal code still leads.
                let detail = match unconfirmed {
                    Some(u) => format!("{};{u}", e.audit_detail()),
                    None => e.audit_detail(),
                };
                (outcome_for(e), Some(detail))
            }
        };
        audit::record(pool, auth, tool, args_json, outcome, detail.as_deref()).await;
        result
    }

    /// Drop the cached status-page HTML showing this incident, so a visibility
    /// flip shows up immediately instead of after the page TTL. Best-effort,
    /// exactly as the REST publish path does it.
    pub(super) async fn invalidate_status_pages(
        &self,
        org: crate::domain::OrgId,
        incident: &crate::domain::OpsIncident,
    ) {
        crate::api::handlers::invalidate_incident_pages(&self.state, org, incident).await;
    }

    /// Public URL of a status page slug, mirroring the operator UI's own
    /// computation (subdomain → absolute apex, path mode → `/status`). Empty
    /// when no public surface is mounted.
    pub(super) fn page_public_url(&self, slug: &str) -> String {
        public_base(&self.state.cfg, slug)
            .map(|origin| public_status_url(&self.state.cfg, &origin))
            .unwrap_or_default()
    }

    /// Start of the monitor's ongoing failure run, RFC 3339, from the confirmed
    /// incidents — covers every monitor, not only those with a public incident.
    /// Best-effort: a lookup error yields `None`.
    pub(super) async fn ongoing_since(
        &self,
        org: crate::domain::OrgId,
        t: &Target,
    ) -> Option<String> {
        let now = Utc::now();
        let range = TimeRange {
            from: now - Duration::try_days(SINCE_LOOKBACK_DAYS).unwrap_or_default(),
            to: now,
        };
        match self
            .state
            .incident_narration_store
            .list_for_target(org, t.id, range, 1, 0, true)
            .await
        {
            Ok(incidents) => incidents.first().map(|i| i.started_at.to_rfc3339()),
            Err(err) => {
                tracing::warn!(target: "mcp", error = %err, target_id = %t.id, "ongoing-since lookup failed");
                None
            }
        }
    }
}
/// A write to a Terraform-declared monitor lands and is then reverted by the
/// next apply, with nothing to tell the operator why. No override argument.
pub(super) fn deny_terraform(target: &Target) -> Result<(), McpToolError> {
    if target.write_source == WriteSource::Terraform {
        return Err(McpToolError::new(
            codes::MANAGED_EXTERNALLY,
            format!(
                "monitor \"{}\" is managed by Terraform. Change it in the .tf that declares it \
                 and apply, or the next `terraform apply` reverts whatever is written here.",
                sanitize_data(&target.name)
            ),
            false,
        ));
    }
    Ok(())
}
