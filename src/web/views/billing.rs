//! The two web hops billing needs outside the JSON API: the pay page the
//! provider's checkout opens on, and the card-update redirect the reminder
//! mails and the console banner point at.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};

use crate::app::AppState;
use crate::domain::BillingStatus;
use crate::storage::{accounts, subscriptions};
use crate::web::Session;
use crate::web::error::WebResult;
use crate::web::filters;

/// Unauthenticated by design: the provider's own mails send a customer here
/// to update a card, and the transaction in the query string is the proof.
#[derive(Template, WebTemplate)]
#[template(path = "pay.html")]
pub struct PayPage {
    pub client_token: String,
    pub sandbox: bool,
}

pub async fn pay_page(State(state): State<AppState>) -> Response {
    let paddle = &state.cfg.billing.paddle;
    PayPage {
        client_token: paddle.client_token.clone(),
        sandbox: paddle.environment == "sandbox",
    }
    .into_response()
}

/// `GET /settings/billing/payment-method`: mints a portal session and sends
/// the account owner to the provider's card form. A session, so it cannot
/// be a stored link.
pub async fn payment_method(
    State(state): State<AppState>,
    session: Session,
) -> WebResult<Response> {
    let (Some(billing), Some(pool), Some(user)) = (
        state.billing.as_ref(),
        state.db.as_ref(),
        session.user.as_ref(),
    ) else {
        return Ok(
            crate::web::auth::login_redirect("/settings/billing/payment-method").into_response(),
        );
    };
    let Some(account) = accounts::account_for_user(pool, user.id).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(sub) = subscriptions::get(pool, account).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let (Some(customer), BillingStatus::Active | BillingStatus::PastDue) =
        (sub.customer_ref.as_deref(), sub.status)
    else {
        return Ok(Redirect::to("/settings/usage").into_response());
    };
    let links = billing
        .provider
        .portal(customer, sub.subscription_ref.as_deref())
        .await?;
    let target = links.update_payment_method.unwrap_or(links.overview);
    Ok(Redirect::to(target.as_str()).into_response())
}

/// The banner's view of the account behind the active org: only what the
/// nav needs to say, and whether the viewer may act on it.
pub struct BillingNotice {
    pub past_due_days_left: Option<i64>,
    pub pending_plan: Option<String>,
    pub owner: bool,
}

pub async fn notice_for(
    state: &AppState,
    org: crate::domain::OrgId,
    user: crate::domain::UserId,
) -> Option<BillingNotice> {
    let pool = state.db.as_ref()?;
    state.billing.as_ref()?;
    let notice = subscriptions::notice_for_org(pool, org).await.ok()??;
    let now = chrono::Utc::now();
    let past_due_days_left = match (
        BillingStatus::parse(&notice.subscription_status),
        notice.grace_until,
    ) {
        (Some(BillingStatus::PastDue), Some(until)) => Some((until - now).num_days().max(0)),
        _ => None,
    };
    let pending_plan = match (
        notice.cancel_at,
        notice.pending_plan_name,
        notice.plan_change_at,
    ) {
        (Some(at), _, _) if at > now => Some(notice.landing_plan_name),
        (None, Some(name), Some(at)) if at > now => Some(name),
        _ => None,
    };
    if past_due_days_left.is_none() && pending_plan.is_none() {
        return None;
    }
    Some(BillingNotice {
        past_due_days_left,
        pending_plan,
        owner: notice.owner_user_id == Some(user.0),
    })
}
