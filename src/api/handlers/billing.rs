//! The account owner's own subscription: `GET /api/v1/account/billing` and
//! the actions under it. Session-only, like token management: a scoped API
//! token must never be able to buy, cancel or move a plan.
//!
//! Every action answers with the account's billing view as it stands after
//! the provider replied, so a caller sees an upgrade applied or a downgrade
//! booked without waiting for the webhook that confirms it.

use crate::api::json::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::billing::Billing;
use crate::domain::{AccountId, BillingStatus, Interval, OrgId, Subscription, UserId};
use crate::error::{AppError, Result};
use crate::storage::subscriptions;
use crate::web::{BrowserUser, CurrentOrg, CurrentUser};

use super::owned_account;

/// Owner-only like the holds: the org's members see the banner, its payer
/// acts on it.
async fn owned(
    state: &AppState,
    org: OrgId,
    user: UserId,
) -> Result<(Arc<Billing>, &PgPool, AccountId)> {
    let Some(billing) = state.billing.clone() else {
        return Err(AppError::not_found(
            codes::BILLING_UNAVAILABLE,
            "no payment provider is configured",
        ));
    };
    let account = owned_account(state, org, user).await?;
    Ok((billing, state.require_db()?, account))
}

/// A plan and cadence on sale, priced as the provider quotes it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Offer {
    pub plan_id: String,
    pub interval: Interval,
    /// In the currency's minor unit: 900 is $9.00.
    pub amount_minor: i32,
    /// ISO 4217.
    pub currency: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BillingView {
    pub status: BillingStatus,
    pub plan_id: String,
    /// Where the account lands when paid service ends.
    pub fallback_plan_id: Option<String>,
    /// A booked move, applied at `plan_change_at`, billing on
    /// `pending_interval` from then.
    pub pending_plan_id: Option<String>,
    pub plan_change_at: Option<DateTime<Utc>>,
    pub pending_interval: Option<Interval>,
    /// A booked cancel: paid service ends here and `fallback_plan_id` takes
    /// over.
    pub cancel_at: Option<DateTime<Utc>>,
    pub current_period_end: Option<DateTime<Utc>>,
    /// How the live subscription bills until a booked move lands; absent
    /// without one.
    pub interval: Option<Interval>,
    /// Set while a failed payment is being retried; full service until then.
    pub grace_until: Option<DateTime<Utc>>,
    /// Whether the provider's portal (invoices, card, cancel) can be opened.
    pub portal_available: bool,
    pub offers: Vec<Offer>,
}

async fn view(billing: &Billing, pool: &PgPool, sub: Subscription) -> Result<BillingView> {
    let offers = subscriptions::priced_plans(pool, billing.provider.name())
        .await?
        .into_iter()
        .filter_map(|p| {
            let interval = Interval::parse(&p.interval)?;
            Some(Offer {
                plan_id: p.plan_id,
                interval,
                amount_minor: p.amount_minor,
                currency: p.currency,
            })
        })
        .collect();
    Ok(BillingView {
        status: sub.status,
        plan_id: sub.plan_id,
        fallback_plan_id: sub.fallback_plan_id,
        pending_plan_id: sub.pending_plan_id,
        plan_change_at: sub.plan_change_at,
        pending_interval: sub.pending_interval,
        cancel_at: sub.cancel_at,
        current_period_end: sub.current_period_end,
        interval: sub.interval,
        grace_until: sub.grace_until,
        portal_available: sub.customer_ref.is_some(),
        offers,
    })
}

#[utoipa::path(
    get, path = "/api/v1/account/billing", tag = "account",
    summary = "Where the account stands with paid service",
    responses(
        (status = 200, body = BillingView),
        (status = 403, body = ApiError, description = "the caller does not own the account"),
        (status = 404, body = ApiError, description = "no payment provider is configured"),
    ),
)]
pub async fn get_billing(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    BrowserUser(CurrentUser(user)): BrowserUser,
) -> Result<Json<BillingView>> {
    let (billing, pool, account) = owned(&state, org, user).await?;
    let sub = billing.subscription(pool, account).await?;
    Ok(Json(view(&billing, pool, sub).await?))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanChoice {
    pub plan_id: String,
    pub interval: Interval,
}

/// A page on the provider's side to send the customer to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Handoff {
    pub url: String,
}

#[utoipa::path(
    post, path = "/api/v1/account/billing/checkout", tag = "account",
    summary = "Start a checkout for a plan",
    request_body = PlanChoice,
    responses(
        (status = 200, body = Handoff),
        (status = 409, body = ApiError, description = "the account already has a subscription"),
        (status = 422, body = ApiError, description = "the plan is not sold on that cadence"),
        (status = 503, body = ApiError, description = "the provider gave no answer; retry later"),
    ),
)]
pub async fn checkout(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    BrowserUser(CurrentUser(user)): BrowserUser,
    Json(choice): Json<PlanChoice>,
) -> Result<Json<Handoff>> {
    let (billing, pool, account) = owned(&state, org, user).await?;
    let url = billing
        .checkout(
            pool,
            &state.quotas,
            account,
            choice.plan_id.trim(),
            choice.interval,
        )
        .await?;
    Ok(Json(Handoff { url: url.into() }))
}

#[utoipa::path(
    post, path = "/api/v1/account/billing/portal", tag = "account",
    summary = "Open the provider's customer portal",
    responses(
        (status = 200, body = Handoff),
        (status = 409, body = ApiError),
        (status = 503, body = ApiError, description = "the provider gave no answer; retry later"),
    ),
)]
pub async fn portal(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    BrowserUser(CurrentUser(user)): BrowserUser,
) -> Result<Json<Handoff>> {
    let (billing, pool, account) = owned(&state, org, user).await?;
    let url = billing.portal(pool, account).await?;
    Ok(Json(Handoff { url: url.into() }))
}

#[utoipa::path(
    put, path = "/api/v1/account/billing/plan", tag = "account",
    summary = "Move the subscription to another plan",
    description = "A bigger plan applies at once, prorated. A smaller one is booked for the end of the paid period.",
    request_body = PlanChoice,
    responses(
        (status = 200, body = BillingView),
        (status = 409, body = ApiError, description = "no active subscription, nothing to change, a cancel booked, another change still being applied, or the provider declined"),
        (status = 422, body = ApiError),
        (status = 503, body = ApiError, description = "the provider gave no answer; retry later"),
    ),
)]
pub async fn change_plan(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    BrowserUser(CurrentUser(user)): BrowserUser,
    Json(choice): Json<PlanChoice>,
) -> Result<Json<BillingView>> {
    let (billing, pool, account) = owned(&state, org, user).await?;
    let sub = billing
        .change_plan(
            pool,
            &state.quotas,
            account,
            choice.plan_id.trim(),
            choice.interval,
        )
        .await?;
    Ok(Json(view(&billing, pool, sub).await?))
}

#[utoipa::path(
    post, path = "/api/v1/account/billing/cancel", tag = "account",
    summary = "Cancel at the end of the paid period",
    responses(
        (status = 200, body = BillingView),
        (status = 409, body = ApiError, description = "no active subscription, another change still being applied, or the provider declined"),
        (status = 503, body = ApiError, description = "the provider gave no answer; retry later"),
    ),
)]
pub async fn cancel(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    BrowserUser(CurrentUser(user)): BrowserUser,
) -> Result<Json<BillingView>> {
    let (billing, pool, account) = owned(&state, org, user).await?;
    let sub = billing.cancel(pool, &state.quotas, account).await?;
    Ok(Json(view(&billing, pool, sub).await?))
}

#[utoipa::path(
    delete, path = "/api/v1/account/billing/cancel", tag = "account",
    summary = "Withdraw a scheduled cancel",
    responses(
        (status = 200, body = BillingView),
        (status = 409, body = ApiError, description = "no cancel scheduled, another change still being applied, or the provider declined"),
        (status = 503, body = ApiError, description = "the provider gave no answer; retry later"),
    ),
)]
pub async fn revoke_cancel(
    State(state): State<AppState>,
    CurrentOrg(org): CurrentOrg,
    BrowserUser(CurrentUser(user)): BrowserUser,
) -> Result<Json<BillingView>> {
    let (billing, pool, account) = owned(&state, org, user).await?;
    let sub = billing.revoke_cancel(pool, &state.quotas, account).await?;
    Ok(Json(view(&billing, pool, sub).await?))
}
