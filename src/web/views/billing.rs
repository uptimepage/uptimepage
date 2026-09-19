//! The billing page and the two hops around it: the pay page the provider's
//! checkout opens on, and the card-update redirect the reminder mails and
//! the console banner point at.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::app::AppState;
use crate::domain::{BillingStatus, Interval, Subscription};
use crate::error::AppError;
use crate::request::{CurrentOrg, CurrentUser, Session};
use crate::storage::subscriptions::PlanCard;
use crate::storage::{accounts, subscriptions};
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::resolve_org;

const TAB_BILLING: &str = "billing";

#[derive(Debug, Default, Deserialize)]
pub struct BillingQuery {
    /// Preselects a card, from the pricing page.
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub interval: Option<String>,
    /// Back from the provider's checkout; the webhook may not have landed.
    #[serde(default)]
    pub paid: Option<String>,
}

/// One figure per cadence, so the toggle swaps text without a round trip.
pub struct Price {
    pub month: Option<String>,
    pub year: Option<String>,
    /// What a year costs against twelve months, when both are on sale.
    pub saving: Option<String>,
}

pub struct CardView {
    pub id: String,
    pub name: String,
    pub tagline: String,
    pub price: Price,
    pub lines: Vec<String>,
    pub current: bool,
    pub selected: bool,
    pub for_sale: bool,
}

pub struct CompareCell {
    pub text: String,
    /// `up` or `down` against the account's current plan, else empty.
    pub delta: &'static str,
}

pub struct CompareRow {
    pub label: &'static str,
    pub cells: Vec<CompareCell>,
}

/// What picking a card would mean, spelled out above the fold.
pub struct Pitch {
    pub id: String,
    pub name: String,
    pub tagline: String,
    pub price: Price,
    pub gains: Vec<String>,
    pub losses: Vec<String>,
    pub shown: bool,
}

/// Where the account is headed: the booked move's target, or the fallback
/// under a booked cancel.
pub struct PendingView {
    pub plan_id: String,
    pub plan_name: String,
    pub at: DateTime<Utc>,
    pub is_cancel: bool,
    /// The cadence a booked move lands on; a cancel has none.
    pub interval: Option<&'static str>,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/billing.html")]
pub struct BillingPage {
    pub active_tab: &'static str,
    pub owner: bool,
    pub status: &'static str,
    pub plan_name: String,
    /// Where the account lands once paid service ends.
    pub fallback_name: String,
    pub cards: Vec<CardView>,
    pub rows: Vec<CompareRow>,
    pub pitches: Vec<Pitch>,
    pub period_end: Option<DateTime<Utc>>,
    pub pending: Option<PendingView>,
    pub grace_until: Option<DateTime<Utc>>,
    pub portal_available: bool,
    /// Sent here by the checkout before the provider confirmed the purchase.
    pub confirming: bool,
    pub interval: &'static str,
    /// The cadence the live subscription bills on, if any.
    pub current_interval: Option<&'static str>,
    pub yearly_on_sale: bool,
}

/// `GET /settings/billing`: where the account stands and what the owner can
/// do about it. Members see the state; only the payer gets the controls.
pub async fn billing_page(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    user: Result<CurrentUser, AppError>,
    uri: Uri,
    Query(q): Query<BillingQuery>,
) -> WebResult<Response> {
    let back = uri
        .path_and_query()
        .map_or("/settings/billing", |pq| pq.as_str());
    let org = match resolve_org(org, back) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let (Some(billing), Some(pool)) = (state.billing.as_ref(), state.db.as_ref()) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let account = accounts::account_for_org(pool, org).await?;
    let Some(sub) = subscriptions::get(pool, account).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let owner = user.ok().is_some_and(|CurrentUser(u)| sub.owner == Some(u));
    let cards = subscriptions::plan_cards(pool, billing.provider.name(), &sub.plan_id).await?;
    let yearly_on_sale = cards.iter().any(|c| c.amount_year.is_some());
    let interval = q
        .interval
        .as_deref()
        .and_then(Interval::parse)
        .or(sub.interval)
        .filter(|i| *i == Interval::Month || yearly_on_sale)
        .unwrap_or(Interval::Month);
    let live = matches!(sub.status, BillingStatus::Active | BillingStatus::PastDue);
    let confirming = q.paid.is_some() && !live;
    let preselect = q.plan.as_deref().filter(|p| *p != sub.plan_id);
    let plan_name = cards
        .iter()
        .find(|c| c.id == sub.plan_id)
        .map_or_else(|| sub.plan_id.clone(), |c| c.name.clone());
    let pending = pending_view(pool, &sub).await?;
    let fallback_name = match &pending {
        Some(p) if p.is_cancel => p.plan_name.clone(),
        _ => name_of(pool, sub.landing_plan_preview()).await?,
    };
    let current = cards.iter().find(|c| c.id == sub.plan_id);
    let rows = compare_rows(&cards, current);
    let pitches = cards
        .iter()
        .filter(|c| c.for_sale() && c.id != sub.plan_id)
        .map(|c| Pitch {
            shown: preselect == Some(c.id.as_str()),
            ..pitch(c, current)
        })
        .collect();
    let cards = cards
        .into_iter()
        .map(|c| {
            let for_sale = c.for_sale();
            let current = c.id == sub.plan_id;
            CardView {
                selected: current && preselect.is_none() || preselect == Some(c.id.as_str()),
                current,
                for_sale,
                price: price_of(&c),
                lines: card_lines(&c),
                id: c.id,
                name: c.name,
                tagline: c.description,
            }
        })
        .collect();
    Ok(BillingPage {
        active_tab: TAB_BILLING,
        owner,
        status: sub.status.as_db_str(),
        plan_name,
        fallback_name,
        cards,
        rows,
        pitches,
        period_end: sub.current_period_end,
        pending,
        grace_until: sub
            .grace_until
            .filter(|_| sub.status == BillingStatus::PastDue),
        portal_available: sub.customer_ref.is_some(),
        confirming,
        interval: interval.as_db_str(),
        current_interval: sub.interval.map(Interval::as_db_str),
        yearly_on_sale,
    }
    .into_response())
}

async fn pending_view(
    pool: &sqlx::PgPool,
    sub: &Subscription,
) -> crate::error::Result<Option<PendingView>> {
    let (plan_id, at, is_cancel, interval) = match (
        sub.cancel_at,
        sub.pending_plan_id.as_deref(),
        sub.plan_change_at,
    ) {
        (Some(at), _, _) => (sub.landing_plan(), at, true, None),
        (None, Some(plan_id), Some(at)) => (
            plan_id,
            at,
            false,
            sub.pending_interval.map(Interval::as_db_str),
        ),
        _ => return Ok(None),
    };
    let plan_name = name_of(pool, plan_id).await?;
    Ok(Some(PendingView {
        plan_id: plan_id.to_owned(),
        plan_name,
        at,
        is_cancel,
        interval,
    }))
}

/// A plan's display name, or its id for one the catalog no longer carries.
async fn name_of(pool: &sqlx::PgPool, plan_id: &str) -> crate::error::Result<String> {
    Ok(subscriptions::plan_brief(pool, plan_id)
        .await?
        .map_or_else(|| plan_id.to_owned(), |p| p.name))
}

fn money(amount: i32, currency: &str) -> String {
    let symbol = match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        _ => "",
    };
    let major = amount / 100;
    let minor = amount % 100;
    let figure = if minor == 0 {
        format!("{symbol}{major}")
    } else {
        format!("{symbol}{major}.{minor:02}")
    };
    if symbol.is_empty() {
        format!("{figure} {currency}")
    } else {
        figure
    }
}

fn price_of(card: &PlanCard) -> Price {
    let Some(currency) = card.currency.as_deref() else {
        return Price {
            month: Some("no charge".into()),
            year: Some("no charge".into()),
            saving: None,
        };
    };
    let label = |interval: Interval| {
        card.amount(interval)
            .map(|a| format!("{} / {}", money(a, currency), interval.as_db_str()))
    };
    let saving = match (card.amount_month, card.amount_year) {
        (Some(m), Some(y)) if m * 12 > y => {
            Some(format!("save {} a year", money(m * 12 - y, currency)))
        }
        _ => None,
    };
    Price {
        month: label(Interval::Month),
        year: label(Interval::Year),
        saving,
    }
}

enum Metric {
    Count(i32),
    Days(i32),
    /// Lower is better.
    Interval(i32),
    Regions(i32),
    Flag(bool),
}

impl Metric {
    fn text(&self) -> String {
        match *self {
            Metric::Count(n) => n.to_string(),
            Metric::Days(n) if n >= 360 => format!("{} months", (n + 15) / 30),
            Metric::Days(n) => format!("{n} days"),
            Metric::Interval(s) if s % 60 == 0 => format!("{} min", s / 60),
            Metric::Interval(s) => format!("{s} sec"),
            Metric::Regions(n) if n >= 1000 => "all".into(),
            Metric::Regions(n) => n.to_string(),
            Metric::Flag(true) => "yes".into(),
            Metric::Flag(false) => "—".into(),
        }
    }

    fn rank(&self) -> i64 {
        match *self {
            Metric::Count(n) | Metric::Days(n) | Metric::Regions(n) => i64::from(n),
            Metric::Interval(s) => -i64::from(s),
            Metric::Flag(b) => i64::from(b),
        }
    }

    fn gain(&self, label: &str) -> String {
        match *self {
            Metric::Count(_) => format!("{} {label}", self.text()),
            Metric::Days(_) => format!("{} of history", self.text()),
            Metric::Interval(_) => format!("checks every {}", self.text()),
            Metric::Regions(n) if n >= 1000 => "every region".into(),
            Metric::Regions(n) => format!("{n} regions"),
            Metric::Flag(_) => label.to_owned(),
        }
    }

    /// The same axis read from the other side: a cap, not a gift.
    fn loss(&self, label: &str) -> String {
        match *self {
            Metric::Count(_) => format!("down to {} {label}", self.text()),
            Metric::Days(_) => format!("history down to {}", self.text()),
            Metric::Interval(_) => format!("checks no faster than every {}", self.text()),
            Metric::Regions(n) => format!("down to {n} regions"),
            Metric::Flag(_) => format!("no {label}"),
        }
    }
}

type Read = fn(&PlanCard) -> Metric;

const COMPARE: &[(&str, Read)] = &[
    ("monitors", |c| Metric::Count(c.max_targets)),
    ("browser flows", |c| Metric::Count(c.max_flow_checks)),
    ("check interval", |c| {
        Metric::Interval(c.min_check_interval_secs)
    }),
    ("regions", |c| Metric::Regions(c.max_regions)),
    ("history", |c| Metric::Days(c.retention_days)),
    ("teammates", |c| Metric::Count(c.max_members)),
    ("status pages", |c| Metric::Count(c.max_status_pages)),
    ("public components", |c| {
        Metric::Count(c.max_public_components)
    }),
    ("share links per monitor", |c| {
        Metric::Count(c.max_share_links_per_monitor)
    }),
    ("bring your own SMS", |c| Metric::Flag(c.sms_alerts_enabled)),
    ("white-label status pages", |c| {
        Metric::Flag(c.white_label_enabled)
    }),
    ("custom domain", |c| Metric::Flag(c.custom_domain_enabled)),
    ("on-call and escalation", |c| {
        Metric::Flag(c.on_call_enabled)
    }),
];

fn delta(m: &Metric, current: Option<&PlanCard>, read: Read) -> &'static str {
    let Some(cur) = current else { return "" };
    match m.rank().cmp(&read(cur).rank()) {
        std::cmp::Ordering::Greater => "up",
        std::cmp::Ordering::Less => "down",
        std::cmp::Ordering::Equal => "",
    }
}

fn compare_rows(cards: &[PlanCard], current: Option<&PlanCard>) -> Vec<CompareRow> {
    COMPARE
        .iter()
        .map(|(label, read)| CompareRow {
            label,
            cells: cards
                .iter()
                .map(|c| {
                    let m = read(c);
                    CompareCell {
                        delta: if c.id == current.map_or("", |p| &p.id) {
                            ""
                        } else {
                            delta(&m, current, *read)
                        },
                        text: m.text(),
                    }
                })
                .collect(),
        })
        .collect()
}

fn pitch(card: &PlanCard, current: Option<&PlanCard>) -> Pitch {
    let mut gains = Vec::new();
    let mut losses = Vec::new();
    for (label, read) in COMPARE {
        let m = read(card);
        match delta(&m, current, *read) {
            "up" => gains.push(m.gain(label)),
            "down" => losses.push(m.loss(label)),
            _ => {}
        }
    }
    Pitch {
        id: card.id.clone(),
        name: card.name.clone(),
        tagline: card.description.clone(),
        price: price_of(card),
        gains,
        losses,
        shown: false,
    }
}

fn card_lines(c: &PlanCard) -> Vec<String> {
    let mut lines = vec![
        format!("{} monitors", c.max_targets),
        Metric::Interval(c.min_check_interval_secs).gain(""),
        format!(
            "{} status page{}",
            c.max_status_pages,
            if c.max_status_pages == 1 { "" } else { "s" }
        ),
        format!("{} teammates", c.max_members),
        format!("{}-day history", c.retention_days),
    ];
    if c.max_flow_checks > 0 {
        lines.insert(
            1,
            format!(
                "{} browser flow{}",
                c.max_flow_checks,
                if c.max_flow_checks == 1 { "" } else { "s" }
            ),
        );
    }
    if c.white_label_enabled {
        lines.push("white-label status pages".into());
    }
    if c.custom_domain_enabled {
        lines.push("custom domain".into());
    }
    if c.on_call_enabled {
        lines.push("on-call and escalation".into());
    }
    lines
}

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
            crate::request::auth::login_redirect("/settings/billing/payment-method")
                .into_response(),
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
        return Ok(Redirect::to("/settings/billing").into_response());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, targets: i32, secs: i32, month: Option<i32>, year: Option<i32>) -> PlanCard {
        PlanCard {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            is_listed: true,
            amount_month: month,
            amount_year: year,
            currency: month.or(year).map(|_| "USD".into()),
            max_targets: targets,
            max_flow_checks: 1,
            min_check_interval_secs: secs,
            max_regions: 5,
            retention_days: 90,
            max_members: 5,
            max_status_pages: 2,
            max_public_components: 30,
            max_share_links_per_monitor: 3,
            sms_alerts_enabled: true,
            white_label_enabled: false,
            custom_domain_enabled: false,
            on_call_enabled: false,
        }
    }

    #[test]
    fn a_pitch_reads_each_axis_from_the_right_side() {
        let team = card("team", 150, 30, Some(1900), Some(19000));
        let pro = card("pro", 50, 60, Some(900), Some(9000));
        let up = pitch(&team, Some(&pro));
        assert_eq!(up.gains, vec!["150 monitors", "checks every 30 sec"]);
        assert!(up.losses.is_empty());
        let down = pitch(&pro, Some(&team));
        assert!(down.gains.is_empty());
        assert_eq!(
            down.losses,
            vec!["down to 50 monitors", "checks no faster than every 1 min"]
        );
    }

    #[test]
    fn prices_carry_both_cadences_and_the_yearly_saving() {
        let p = price_of(&card("pro", 50, 60, Some(900), Some(9000)));
        assert_eq!(p.month.as_deref(), Some("$9 / month"));
        assert_eq!(p.year.as_deref(), Some("$90 / year"));
        assert_eq!(p.saving.as_deref(), Some("save $18 a year"));
        let monthly_only = price_of(&card("team", 150, 30, Some(1950), None));
        assert_eq!(monthly_only.month.as_deref(), Some("$19.50 / month"));
        assert_eq!(monthly_only.year, None);
        assert_eq!(monthly_only.saving, None);
        let free = price_of(&card("free", 20, 180, None, None));
        assert_eq!(free.month.as_deref(), Some("no charge"));
    }

    #[test]
    fn a_delisted_plan_is_not_for_sale_even_with_prices() {
        let mut c = card("pro", 50, 60, Some(900), None);
        assert!(c.for_sale());
        c.is_listed = false;
        assert!(!c.for_sale());
    }
}
