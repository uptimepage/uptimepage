//! Usage transparency: `GET /api/v1/orgs/{id}/usage` and
//! `GET /api/v1/me/usage`.
//!
//! Read-only mirrors of the limits the enforcers act on — every number here
//! comes from the same source the block path uses (the account's `plans` row
//! via `QuotaService`, the per-user token cap from that plan) so
//! "current/limit" can never disagree with where a request is actually
//! rejected.
//!
//! The counts are the **account's**, pooled across every live org it owns, not
//! the one org in the path: that is the pool the caps are enforced against.

use crate::api::json::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::OrgId;
use crate::error::{AppError, Result};
use crate::web::auth::{CurrentOrg, CurrentUser};

/// A single quota's `current` against its `limit`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QuotaUsage {
    pub current: i64,
    pub limit: i64,
}

impl QuotaUsage {
    fn new(current: i64, limit: i32) -> Self {
        Self {
            current,
            limit: i64::from(limit),
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlanSummary {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Pooled counts for the account behind the org in the path.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OrgQuotas {
    pub max_orgs: QuotaUsage,
    pub max_targets: QuotaUsage,
    pub max_members: QuotaUsage,
    pub max_pending_invitations: QuotaUsage,
    pub max_public_components: QuotaUsage,
    pub max_maintenance_windows: QuotaUsage,
    pub max_notification_channels: QuotaUsage,
}

/// Non-counted policy values (floors / ceilings, not "x of y").
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UsagePolicy {
    pub min_check_interval_secs: i32,
    /// History window (days) for charts/rollups.
    pub retention_days: i32,
    /// Forensics window (days) for raw per-check detail; ≤ retention_days.
    pub raw_days: i32,
    pub max_logo_size_bytes: i32,
    pub max_api_tokens_per_user: i32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UsageRateLimits {
    pub api_writes_per_minute: i32,
    pub api_reads_per_minute: i32,
    pub bulk_ops_per_minute: i32,
    pub test_now_per_minute: i32,
    pub check_now_per_minute: i32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UsageFeatures {
    pub custom_domain_enabled: bool,
    pub white_label_enabled: bool,
    pub sms_alerts_enabled: bool,
    pub incident_narration_enabled: bool,
    pub max_flow_checks: i32,
    /// Steps one flow monitor may declare on this plan.
    pub max_flow_steps: i32,
    /// Days a failed flow run keeps the page it captured.
    pub evidence_days: i32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OrgUsageResponse {
    pub plan: PlanSummary,
    pub quotas: OrgQuotas,
    pub policy: UsagePolicy,
    pub rate_limits: UsageRateLimits,
    pub features: UsageFeatures,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MeUsageResponse {
    pub api_tokens: QuotaUsage,
    pub owned_orgs: QuotaUsage,
}

#[utoipa::path(
    get,
    path = "/api/v1/orgs/{id}/usage",
    tag = "orgs",
    summary = "Current resource usage against plan limits (member-only)",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = OrgUsageResponse),
        (status = 401, body = ApiError),
        (status = 404, body = ApiError),
    ),
)]
pub async fn get_org_usage(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<OrgUsageResponse>> {
    let pool = state.require_db()?;
    let org = OrgId(id);
    // Same gate + error as `GET /orgs/{id}`: a non-member must not learn the
    // org exists.
    if !crate::storage::orgs::is_active_member(pool, user, org).await? {
        return Err(AppError::not_found(
            codes::ORG_NOT_FOUND,
            "organisation not found",
        ));
    }

    let u = state.quotas.account_usage(org).await?;
    let p = &u.plan;
    Ok(Json(OrgUsageResponse {
        plan: PlanSummary {
            id: p.id.clone(),
            name: p.name.clone(),
            description: p.description.clone(),
        },
        quotas: OrgQuotas {
            max_orgs: QuotaUsage::new(u.orgs, p.max_orgs),
            max_targets: QuotaUsage::new(u.targets, p.max_targets),
            max_members: QuotaUsage::new(u.members, p.max_members),
            max_pending_invitations: QuotaUsage::new(
                u.pending_invitations,
                p.max_pending_invitations,
            ),
            max_public_components: QuotaUsage::new(u.public_components, p.max_public_components),
            max_maintenance_windows: QuotaUsage::new(
                u.maintenance_windows,
                p.max_maintenance_windows,
            ),
            max_notification_channels: QuotaUsage::new(
                u.notification_channels,
                p.max_notification_channels,
            ),
        },
        policy: UsagePolicy {
            min_check_interval_secs: p.min_check_interval_secs,
            retention_days: p.retention_days,
            raw_days: p.raw_days,
            max_logo_size_bytes: p.max_logo_size_bytes,
            max_api_tokens_per_user: p.max_api_tokens_per_user,
        },
        rate_limits: UsageRateLimits {
            api_writes_per_minute: p.api_writes_per_minute,
            api_reads_per_minute: p.api_reads_per_minute,
            bulk_ops_per_minute: p.bulk_ops_per_minute,
            test_now_per_minute: p.test_now_per_minute,
            check_now_per_minute: p.check_now_per_minute,
        },
        features: UsageFeatures {
            custom_domain_enabled: p.custom_domain_enabled,
            white_label_enabled: p.white_label_enabled,
            sms_alerts_enabled: p.sms_alerts_enabled,
            incident_narration_enabled: p.incident_narration_enabled,
            max_flow_checks: p.max_flow_checks,
            max_flow_steps: crate::domain::FlowCheck::allowed_steps(p.max_flow_steps) as i32,
            evidence_days: p.evidence_days,
        },
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/me/usage",
    tag = "orgs",
    summary = "Per-user usage (API tokens) and the account's org count",
    responses(
        (status = 200, body = MeUsageResponse),
        (status = 401, body = ApiError),
    ),
)]
pub async fn get_me_usage(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    CurrentOrg(org): CurrentOrg,
) -> Result<Json<MeUsageResponse>> {
    let pool = state.require_db()?;
    // Token cap is per-user but plan-scoped: resolve it from the caller's
    // active org's account plan so the reported "limit" matches the cap the
    // token-create enforcer would apply right now.
    let token_limit = state
        .quotas
        .limit_for_org(org)
        .await?
        .max_api_tokens_per_user;
    let api_tokens = crate::auth::api_tokens::count_for_user(pool, user).await?;
    // The caller's *own* account, never the active org's: a member acting
    // inside someone else's org must not read that account's numbers back out
    // of a `/me/` endpoint. Same pair `create_org_with_owner` enforces at, so
    // the reported limit and the enforced limit are one number.
    let (owned_orgs, owned_limit) =
        crate::storage::accounts::org_allowance_for_user(pool, user).await?;

    Ok(Json(MeUsageResponse {
        api_tokens: QuotaUsage::new(i64::from(api_tokens), token_limit),
        owned_orgs: QuotaUsage {
            current: owned_orgs,
            limit: owned_limit,
        },
    }))
}
