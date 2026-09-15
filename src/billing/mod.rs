//! Moving an account between plans, and the paid lifecycle around it.
//!
//! `accounts.plan_id` has one writer, [`set_plan`], whoever asks for the move:
//! an operator, a payment provider's events, or the lifecycle's own clock.
//! One path is what makes every change carry the same guarantees: the ledger
//! row, the cache drop that applies the new caps on the next request, and
//! the reconcile.

pub mod actions;
pub mod lifecycle;
pub mod mail;
pub mod paddle;
pub mod provider;

use anyhow::Context;
use serde::Serialize;
use serde_json::json;
use sqlx::PgPool;

use crate::api::error::codes;
use crate::domain::{AccountId, BillingStatus, UserId};
use crate::error::{AppError, Result};
use crate::quotas::{QuotaService, Reconciled, reconcile_after_change};
use crate::storage::{billing_events, subscriptions};

pub use lifecycle::Billing;

/// Who asked for the change, as the ledger records it.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Actor {
    Operator,
    User {
        id: UserId,
    },
    Provider {
        name: &'static str,
    },
    /// The lifecycle's own clock: a scheduled change or an expired grace.
    System,
}

impl Actor {
    fn user(self) -> Option<UserId> {
        match self {
            Actor::User { id } => Some(id),
            _ => None,
        }
    }
}

/// `from == to` when the account was already there. Nothing is written for
/// that, but the holds are still reconciled, so a retry after a failure past
/// the commit converges instead of skipping the step that failed.
#[derive(Debug, Clone)]
pub struct PlanChange {
    pub from: String,
    pub to: String,
    pub fallback: Option<String>,
    pub reconciled: Reconciled,
}

/// `fallback_plan_id` is where the account lands once paid service ends.
/// Left unset, the first move away from a plan records that plan and later
/// moves keep it.
#[derive(Debug, Clone)]
pub struct PlanRequest<'a> {
    pub plan_id: &'a str,
    pub fallback_plan_id: Option<&'a str>,
    pub reason: &'a str,
    pub actor: Actor,
}

/// The locked write's outcome, before the reconcile.
#[derive(Debug, Clone)]
pub struct PlanWrite {
    pub from: String,
    pub to: String,
    pub fallback: Option<String>,
    pub changed: bool,
}

#[derive(sqlx::FromRow)]
struct AccountPlanRow {
    plan_id: String,
    fallback_plan_id: Option<String>,
}

pub async fn set_plan(
    pool: &PgPool,
    quotas: &QuotaService,
    account: AccountId,
    req: PlanRequest<'_>,
) -> Result<PlanChange> {
    let mut tx = pool.begin().await.context("set_plan: begin")?;
    let actor = req.actor;
    refuse_a_paid_plan_change(&mut tx, account, req.plan_id).await?;
    let write = set_plan_tx(&mut tx, account, req).await?;
    if write.changed {
        tx.commit().await.context("set_plan: commit")?;
    } else {
        drop(tx);
    }
    let reconciled = reconcile_after_change(pool, quotas, account, actor.user()).await?;
    Ok(PlanChange {
        from: write.from,
        to: write.to,
        fallback: write.fallback,
        reconciled,
    })
}

/// A paid plan is the provider's, in grace as much as paid up: the lifecycle
/// puts the paid price back on the next event, so a write here would hold
/// until then and mail the owner about a move nobody asked for. The fallback
/// alone may still be named. A courtesy on top of what is paid is an
/// override; a different plan is a price change at the provider once paid.
async fn refuse_a_paid_plan_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
    plan_id: &str,
) -> Result<()> {
    let Some(sub) = subscriptions::lock(tx, account).await? else {
        return Ok(());
    };
    let paid = matches!(sub.status, BillingStatus::Active | BillingStatus::PastDue);
    if paid && sub.plan_id != plan_id {
        return Err(AppError::conflict(
            codes::SUBSCRIPTION_STATE,
            "the account holds a paid plan; change the price at the payment provider once it is paid up, or grant caps as an override",
        ));
    }
    Ok(())
}

/// The plan write inside a caller's transaction. The caller owns the commit
/// and must follow it with [`reconcile_after_change`], which is what applies
/// the new caps; the account row is locked here, so the lifecycle's own lock
/// on it is simply re-taken.
pub async fn set_plan_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
    req: PlanRequest<'_>,
) -> Result<PlanWrite> {
    // Row lock only: the account's advisory lock belongs to the reconcile,
    // which takes it against creates the way every cap check does.
    let current: Option<AccountPlanRow> =
        sqlx::query_as("SELECT plan_id, fallback_plan_id FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(account.0)
            .fetch_optional(&mut **tx)
            .await
            .context("set_plan: current")?;
    let Some(current) = current else {
        return Err(AppError::not_found(
            codes::ACCOUNT_NOT_FOUND,
            "account not found",
        ));
    };
    for plan in [Some(req.plan_id), req.fallback_plan_id]
        .into_iter()
        .flatten()
    {
        require_plan(tx, plan).await?;
    }

    let fallback = req
        .fallback_plan_id
        .map(str::to_owned)
        .or(current.fallback_plan_id.clone())
        .or_else(|| (current.plan_id != req.plan_id).then(|| current.plan_id.clone()));
    if current.plan_id == req.plan_id && current.fallback_plan_id == fallback {
        return Ok(PlanWrite {
            from: current.plan_id.clone(),
            to: current.plan_id,
            fallback,
            changed: false,
        });
    }

    sqlx::query(
        "UPDATE accounts SET plan_id = $2, fallback_plan_id = $3, updated_at = now() WHERE id = $1",
    )
    .bind(account.0)
    .bind(req.plan_id)
    .bind(&fallback)
    .execute(&mut **tx)
    .await
    .context("set_plan: update")?;
    billing_events::record_tx(
        tx,
        account,
        billing_events::PLAN_CHANGED,
        json!({
            "from": current.plan_id,
            "to": req.plan_id,
            "fallback": fallback,
            "reason": req.reason,
            "actor": req.actor,
        }),
    )
    .await?;
    Ok(PlanWrite {
        from: current.plan_id,
        to: req.plan_id.to_owned(),
        fallback,
        changed: true,
    })
}

/// The FK would refuse an unknown plan too, as a constraint error rather
/// than a 404.
async fn require_plan(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, plan_id: &str) -> Result<()> {
    let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM plans WHERE id = $1")
        .bind(plan_id)
        .fetch_optional(&mut **tx)
        .await
        .context("set_plan: plan lookup")?;
    if exists.is_none() {
        return Err(AppError::not_found(
            codes::PLAN_NOT_FOUND,
            format!("no plan {plan_id:?}"),
        ));
    }
    Ok(())
}
