//! What the customer keeps when their plan no longer covers everything:
//! `GET /api/v1/account/holds` and `PUT /api/v1/account/holds`.
//!
//! An account over its caps keeps every row it has, with the excess held. The
//! default choice is that the oldest rows keep the slots, which is a guess. The
//! `PUT` replaces the guess with the customer's own list, so the monitor that
//! matters most is the one still running whatever order it was created in.
//!
//! Account-wide by design: holds are decided against the account's pooled caps,
//! so scoping this to one org would let the same picks disagree between two
//! orgs sharing a pool. It carries the monitor-write scope, which is what a
//! pick mostly decides, though the same call can move a status page too.
//!
//! Owning an *org* is not enough to reach it. An org owner is a membership
//! role and can be granted by another org owner, while an account may span
//! several orgs sharing one pool. Since the pool is zero-sum, an org owner who
//! could call this would be able to read a sibling org's monitor names and
//! push the holds onto it, stopping monitoring they cannot even see. Both
//! endpoints therefore check the caller against `accounts.owner_user_id`.

use crate::api::json::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::{AccountId, OrgId, UserId};
use crate::error::{AppError, Result};
use crate::quotas::PlanSource;
use crate::web::auth::CurrentUser;
use crate::web::{OwnerAuthorized, TargetsWrite};

/// The account behind the caller's active org, but only once the caller is
/// shown to own that account. An org owner who is not the account owner gets
/// 403 rather than the account's whole pool.
async fn owned_account(state: &AppState, org: OrgId, user: UserId) -> Result<AccountId> {
    let pool = state.require_db()?;
    let account = crate::storage::accounts::account_for_org(pool, org).await?;
    if crate::storage::accounts::account_for_user(pool, user).await? != Some(account) {
        return Err(AppError::forbidden_code(
            codes::ACCOUNT_OWNER_REQUIRED,
            "only the account owner can change what the plan keeps",
        ));
    }
    Ok(account)
}

/// One row a plan is currently holding.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HeldItem {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub held_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HoldsResponse {
    /// Monitors the plan is not covering right now.
    pub targets: Vec<HeldItem>,
    /// Status pages the plan is not covering right now.
    pub status_pages: Vec<HeldItem>,
}

/// The ids to keep, one list per resource. Anything the plan cannot cover
/// after honouring them is held, newest first, and so is anything a list
/// leaves out — naming what to keep is also declining the rest.
///
/// An omitted list means "not asked about", leaving that resource's previous
/// answer untouched; an empty list clears it and hands the choice back to the
/// default order. A caller shown only its monitors therefore cannot wipe the
/// status page pick, and `{}` is a plain reconcile.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct KeepRequest {
    #[serde(default)]
    pub keep_monitors: Option<Vec<Uuid>>,
    #[serde(default)]
    pub keep_status_pages: Option<Vec<Uuid>>,
}

/// A pick can name at most this many rows. Well past any plan's ceilings, and
/// it bounds the array the reconcile statement carries.
const MAX_KEEP: usize = 2_000;

#[utoipa::path(
    get, path = "/api/v1/account/holds", tag = "account",
    summary = "List what the plan is currently holding",
    responses((status = 200, body = HoldsResponse)),
)]
pub async fn list_holds(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<TargetsWrite>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<HoldsResponse>> {
    let account = owned_account(&state, org, user).await?;
    let pool = state.require_db()?;
    let (targets, status_pages) = crate::quotas::holds::list_held(pool, account).await?;
    Ok(Json(HoldsResponse {
        targets: targets.into_iter().map(HeldItem::from).collect(),
        status_pages: status_pages.into_iter().map(HeldItem::from).collect(),
    }))
}

#[utoipa::path(
    put, path = "/api/v1/account/holds", tag = "account",
    summary = "Choose what the plan keeps",
    request_body = KeepRequest,
    responses((status = 200, body = HoldsResponse), (status = 422, body = ApiError)),
)]
pub async fn set_holds(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<TargetsWrite>,
    CurrentUser(actor): CurrentUser,
    Json(body): Json<KeepRequest>,
) -> Result<Json<HoldsResponse>> {
    let named = |l: &Option<Vec<Uuid>>| l.as_ref().map_or(0, Vec::len);
    if named(&body.keep_monitors) > MAX_KEEP || named(&body.keep_status_pages) > MAX_KEEP {
        return Err(AppError::bad_request(
            codes::BULK_TOO_LARGE,
            "too many rows named at once",
        ));
    }
    let account = owned_account(&state, org, actor).await?;
    let pool = state.require_db()?;
    // Stored before reconciling, so the daily sweep reads the same answer and
    // cannot put back on hold what the customer just asked to keep.
    crate::quotas::holds::set_keep(
        pool,
        account,
        body.keep_monitors.as_deref(),
        body.keep_status_pages.as_deref(),
    )
    .await?;
    crate::quotas::holds::reconcile_account(
        pool,
        account,
        PlanSource::Resolve(&state.quotas, org),
        Some(actor),
    )
    .await?;
    let (targets, status_pages) = crate::quotas::holds::list_held(pool, account).await?;
    Ok(Json(HoldsResponse {
        targets: targets.into_iter().map(HeldItem::from).collect(),
        status_pages: status_pages.into_iter().map(HeldItem::from).collect(),
    }))
}

impl From<crate::quotas::holds::Held> for HeldItem {
    fn from(h: crate::quotas::holds::Held) -> Self {
        Self {
            id: h.id,
            org_id: h.org_id,
            name: h.name,
            held_at: h.held_at,
        }
    }
}
