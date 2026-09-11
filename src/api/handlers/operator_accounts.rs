//! Operator (instance-admin) surface for one account's entitlements: the plan
//! it is on and the caps overridden on top of it. Gated by [`OperatorAuth`]
//! like the rest of `/operator`. Every change goes through the path a payment
//! provider's events will, so a manual grant behaves exactly like a paid one.

use crate::api::json::Json;
use anyhow::Context;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::error::codes;
use crate::app::AppState;
use crate::billing::{Actor, PlanRequest, set_plan};
use crate::domain::AccountId;
use crate::error::{AppError, Result};
use crate::quotas::{Reconciled, overrides};
use crate::web::OperatorAuth;

const MAX_REASON: usize = 500;

/// An empty reason leaves a ledger row nobody can act on.
fn validate_reason(reason: &str) -> Result<&str> {
    let trimmed = reason.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_REASON {
        return Err(AppError::bad_request(
            codes::REASON_INVALID,
            format!("reason must be 1..={MAX_REASON} characters"),
        ));
    }
    Ok(trimmed)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPlan {
    pub plan_id: String,
    #[serde(default)]
    pub fallback_plan_id: Option<String>,
    pub reason: String,
}

#[derive(Serialize)]
pub struct PlanView {
    pub account_id: Uuid,
    pub plan_id: String,
    pub previous_plan_id: String,
    pub fallback_plan_id: Option<String>,
    pub held: usize,
    pub released: usize,
}

pub async fn set_account_plan(
    _: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<SetPlan>,
) -> Result<Json<PlanView>> {
    let reason = validate_reason(&req.reason)?;
    let account = AccountId(id);
    let change = set_plan(
        state.require_db()?,
        &state.quotas,
        account,
        PlanRequest {
            plan_id: req.plan_id.trim(),
            fallback_plan_id: req.fallback_plan_id.as_deref().map(str::trim),
            reason,
            actor: Actor::Operator,
        },
    )
    .await?;
    Ok(Json(PlanView {
        account_id: id,
        plan_id: change.to,
        previous_plan_id: change.from,
        fallback_plan_id: change.fallback,
        held: change.reconciled.held,
        released: change.reconciled.released,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetOverrides {
    pub caps: serde_json::Value,
    pub reason: String,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct OverridesView {
    pub account_id: Uuid,
    pub held: usize,
    pub released: usize,
}

impl OverridesView {
    fn new(account_id: Uuid, r: Reconciled) -> Self {
        Self {
            account_id,
            held: r.held,
            released: r.released,
        }
    }
}

pub async fn set_account_overrides(
    _: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<SetOverrides>,
) -> Result<Json<OverridesView>> {
    let reason = validate_reason(&req.reason)?;
    if req.expires_at.is_some_and(|at| at <= Utc::now()) {
        return Err(AppError::bad_request(
            codes::PLAN_OVERRIDE_INVALID,
            "expires_at must be in the future",
        ));
    }
    let pool = state.require_db()?;
    let account = require_account(pool, id).await?;
    let r = overrides::set(
        pool,
        &state.quotas,
        account,
        &req.caps,
        reason,
        req.expires_at,
    )
    .await?;
    Ok(Json(OverridesView::new(id, r)))
}

pub async fn clear_account_overrides(
    _: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<OverridesView>> {
    let pool = state.require_db()?;
    let account = require_account(pool, id).await?;
    let r = overrides::clear(pool, &state.quotas, account).await?;
    Ok(Json(OverridesView::new(id, r)))
}

/// The FK would refuse an unknown account too, as a constraint error rather
/// than a 404.
async fn require_account(pool: &sqlx::PgPool, id: Uuid) -> Result<AccountId> {
    let exists: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("account lookup")?;
    exists
        .map(|(id,)| AccountId(id))
        .ok_or_else(|| AppError::not_found(codes::ACCOUNT_NOT_FOUND, "account not found"))
}
