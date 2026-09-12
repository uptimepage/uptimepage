//! The subscription columns on `accounts`, the map from provider prices to
//! plans, and the record of provider events already acted on.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::billing::provider::Interval;
use crate::domain::{AccountId, BillingStatus, Subscription, UserId};
use crate::error::{AppError, Result};

const COLUMNS: &str = "id, owner_user_id, plan_id, fallback_plan_id, subscription_status, \
     pending_plan_id, plan_change_at, current_period_end, grace_until, dunning_stage, \
     billing_provider, provider_customer_ref, provider_subscription_ref, \
     subscription_synced_at, payment_synced_at, cancel_at";

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    owner_user_id: Option<Uuid>,
    plan_id: String,
    fallback_plan_id: Option<String>,
    subscription_status: String,
    pending_plan_id: Option<String>,
    plan_change_at: Option<DateTime<Utc>>,
    cancel_at: Option<DateTime<Utc>>,
    current_period_end: Option<DateTime<Utc>>,
    grace_until: Option<DateTime<Utc>>,
    dunning_stage: i16,
    billing_provider: Option<String>,
    provider_customer_ref: Option<String>,
    provider_subscription_ref: Option<String>,
    subscription_synced_at: Option<DateTime<Utc>>,
    payment_synced_at: Option<DateTime<Utc>>,
}

impl TryFrom<Row> for Subscription {
    type Error = AppError;

    fn try_from(r: Row) -> Result<Self> {
        let status = BillingStatus::parse(&r.subscription_status).ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "account {}: unknown subscription_status {:?}",
                r.id,
                r.subscription_status
            ))
        })?;
        Ok(Subscription {
            account: AccountId(r.id),
            owner: r.owner_user_id.map(UserId),
            plan_id: r.plan_id,
            fallback_plan_id: r.fallback_plan_id,
            status,
            pending_plan_id: r.pending_plan_id,
            plan_change_at: r.plan_change_at,
            cancel_at: r.cancel_at,
            current_period_end: r.current_period_end,
            grace_until: r.grace_until,
            dunning_stage: r.dunning_stage,
            provider: r.billing_provider,
            customer_ref: r.provider_customer_ref,
            subscription_ref: r.provider_subscription_ref,
            synced_at: r.subscription_synced_at,
            payment_synced_at: r.payment_synced_at,
        })
    }
}

/// The row under `FOR UPDATE`, so the lifecycle's read-decide-write happens
/// once per account at a time.
pub async fn lock(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
) -> Result<Option<Subscription>> {
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM accounts WHERE id = $1 FOR UPDATE"
    ))
    .bind(account.0)
    .fetch_optional(&mut **tx)
    .await
    .context("subscriptions::lock")?;
    row.map(Subscription::try_from).transpose()
}

pub async fn get<'e, E: PgExecutor<'e>>(
    exec: E,
    account: AccountId,
) -> Result<Option<Subscription>> {
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM accounts WHERE id = $1"))
        .bind(account.0)
        .fetch_optional(exec)
        .await
        .context("subscriptions::get")?;
    row.map(Subscription::try_from).transpose()
}

/// The account a provider subscription belongs to, for events that carry no
/// account of their own.
pub async fn account_for_subscription_ref<'e, E: PgExecutor<'e>>(
    exec: E,
    provider: &str,
    subscription_ref: &str,
) -> Result<Option<AccountId>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM accounts WHERE billing_provider = $1 AND provider_subscription_ref = $2",
    )
    .bind(provider)
    .bind(subscription_ref)
    .fetch_optional(exec)
    .await
    .context("subscriptions::account_for_subscription_ref")?;
    Ok(row.map(|(id,)| AccountId(id)))
}

/// Writes every subscription column back. `plan_id` and `fallback_plan_id`
/// are deliberately not here.
pub async fn write(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, s: &Subscription) -> Result<()> {
    sqlx::query(
        "UPDATE accounts SET subscription_status = $2, pending_plan_id = $3, plan_change_at = $4, \
         current_period_end = $5, grace_until = $6, dunning_stage = $7, billing_provider = $8, \
         provider_customer_ref = $9, provider_subscription_ref = $10, \
         subscription_synced_at = $11, payment_synced_at = $12, cancel_at = $13, \
         updated_at = now() \
         WHERE id = $1",
    )
    .bind(s.account.0)
    .bind(s.status.as_db_str())
    .bind(&s.pending_plan_id)
    .bind(s.plan_change_at)
    .bind(s.current_period_end)
    .bind(s.grace_until)
    .bind(s.dunning_stage)
    .bind(&s.provider)
    .bind(&s.customer_ref)
    .bind(&s.subscription_ref)
    .bind(s.synced_at)
    .bind(s.payment_synced_at)
    .bind(s.cancel_at)
    .execute(&mut **tx)
    .await
    .context("subscriptions::write")?;
    Ok(())
}

/// Records the event as acted on. `false` means it already was, and the
/// caller has nothing left to do but acknowledge it. Runs inside the
/// transaction that applies the event, so a failure past this point frees
/// the id for the provider's retry.
pub async fn claim_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    provider: &str,
    event_id: &str,
    event_type: &str,
    occurred_at: DateTime<Utc>,
) -> Result<bool> {
    let inserted = sqlx::query(
        "INSERT INTO provider_events (provider, event_id, event_type, occurred_at) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(provider)
    .bind(event_id)
    .bind(event_type)
    .bind(occurred_at)
    .execute(&mut **tx)
    .await
    .context("subscriptions::claim_event")?
    .rows_affected();
    Ok(inserted == 1)
}

/// Whether this provider event was already recorded, so a redelivery can be
/// skipped before it costs an outbound provider call. `claim_event` is still
/// the authority under the transaction; this is only a cheap pre-check.
pub async fn event_seen<'e, E: PgExecutor<'e>>(
    exec: E,
    provider: &str,
    event_id: &str,
) -> Result<bool> {
    let row: Option<(bool,)> =
        sqlx::query_as("SELECT true FROM provider_events WHERE provider = $1 AND event_id = $2")
            .bind(provider)
            .bind(event_id)
            .fetch_optional(exec)
            .await
            .context("subscriptions::event_seen")?;
    Ok(row.is_some())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanPrice {
    pub price_ref: String,
    pub plan_id: String,
    pub interval: String,
}

/// A subscription may carry other recurring prices later; only plan prices
/// decide caps.
pub async fn plans_for_prices<'e, E: PgExecutor<'e>>(
    exec: E,
    provider: &str,
    price_refs: &[String],
) -> Result<Vec<PlanPrice>> {
    sqlx::query_as(
        "SELECT price_ref, plan_id, interval FROM plan_prices \
         WHERE provider = $1 AND price_ref = ANY($2)",
    )
    .bind(provider)
    .bind(price_refs)
    .fetch_all(exec)
    .await
    .context("subscriptions::plans_for_prices")
    .map_err(Into::into)
}

pub async fn price_for_plan<'e, E: PgExecutor<'e>>(
    exec: E,
    provider: &str,
    plan_id: &str,
    interval: Interval,
) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT price_ref FROM plan_prices WHERE provider = $1 AND plan_id = $2 AND interval = $3",
    )
    .bind(provider)
    .bind(plan_id)
    .bind(interval.as_db_str())
    .fetch_optional(exec)
    .await
    .context("subscriptions::price_for_plan")?;
    Ok(row.map(|(p,)| p))
}

/// Plans a customer can move to through checkout, with the cadences on sale.
pub async fn priced_plans(pool: &PgPool, provider: &str) -> Result<Vec<PlanPrice>> {
    sqlx::query_as(
        "SELECT price_ref, plan_id, interval FROM plan_prices \
         WHERE provider = $1 ORDER BY plan_id, interval",
    )
    .bind(provider)
    .fetch_all(pool)
    .await
    .context("subscriptions::priced_plans")
    .map_err(Into::into)
}

/// Accounts with a scheduled change that is due, or a grace window open: the
/// latter may owe a reminder or have run out, which the lifecycle decides.
/// Bounded so one tick never holds the sweep on an unexpectedly large backlog.
pub async fn due_for_sweep(
    pool: &PgPool,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<AccountId>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        // Ordered by the soonest of the two deadlines, not by id: a grace
        // window stays selectable for its whole 14 days, so an id order would
        // let a backlog past the batch size starve its tail every tick. The
        // most overdue account is always served first and the set drains.
        "SELECT id FROM accounts \
         WHERE (plan_change_at IS NOT NULL AND plan_change_at <= $1) \
            OR (cancel_at IS NOT NULL AND cancel_at <= $1) \
            OR grace_until IS NOT NULL \
         ORDER BY least(coalesce(plan_change_at, 'infinity'), coalesce(cancel_at, 'infinity'), \
                        coalesce(grace_until, 'infinity')) \
         LIMIT $2",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("subscriptions::due_for_sweep")?;
    Ok(rows.into_iter().map(|(id,)| AccountId(id)).collect())
}

/// Live accounts per subscription status, for the standing gauge.
pub async fn count_by_status(pool: &PgPool) -> Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT subscription_status, count(*) FROM accounts \
         WHERE owner_user_id IS NOT NULL GROUP BY subscription_status \
         /* SAFE: operator-wide billing gauge, counts every account by design */",
    )
    .fetch_all(pool)
    .await
    .context("subscriptions::count_by_status")
    .map_err(Into::into)
}

/// The caps a hold is judged against, which a downgrade notice quotes.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanBrief {
    pub id: String,
    pub name: String,
    pub max_targets: i32,
    pub max_flow_checks: i32,
    pub max_status_pages: i32,
}

pub async fn plan_brief<'e, E: PgExecutor<'e>>(
    exec: E,
    plan_id: &str,
) -> Result<Option<PlanBrief>> {
    sqlx::query_as(
        "SELECT id, name, max_targets, max_flow_checks, max_status_pages FROM plans WHERE id = $1",
    )
    .bind(plan_id)
    .fetch_optional(exec)
    .await
    .context("subscriptions::plan_brief")
    .map_err(Into::into)
}

/// One trip per nav render, on every page: the banner's inputs joined.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Notice {
    pub owner_user_id: Option<Uuid>,
    pub subscription_status: String,
    pub grace_until: Option<DateTime<Utc>>,
    pub pending_plan_name: Option<String>,
    pub plan_change_at: Option<DateTime<Utc>>,
    pub landing_plan_name: String,
    pub cancel_at: Option<DateTime<Utc>>,
}

pub async fn notice_for_org(pool: &PgPool, org: crate::domain::OrgId) -> Result<Option<Notice>> {
    sqlx::query_as(
        "SELECT /* SAFE: the caller's own org, bound to that one org id */ \
                a.owner_user_id, a.subscription_status, a.grace_until, \
                p.name AS pending_plan_name, a.plan_change_at, \
                coalesce(f.name, a.fallback_plan_id, 'free') AS landing_plan_name, a.cancel_at \
         FROM organizations o \
         JOIN accounts a ON a.id = o.account_id \
         LEFT JOIN plans p ON p.id = a.pending_plan_id \
         LEFT JOIN plans f ON f.id = coalesce(a.fallback_plan_id, 'free') \
         WHERE o.id = $1 \
           AND (a.grace_until IS NOT NULL OR a.pending_plan_id IS NOT NULL \
                OR a.cancel_at IS NOT NULL)",
    )
    .bind(org.0)
    .fetch_optional(pool)
    .await
    .context("subscriptions::notice_for_org")
    .map_err(Into::into)
}
