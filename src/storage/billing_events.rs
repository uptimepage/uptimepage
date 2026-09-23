//! The account's own append-only record of what was done to its
//! entitlements, in our terms rather than a provider's, so the history reads
//! the same whichever provider carried the payment.

use anyhow::Context;
use serde_json::Value;

use crate::domain::AccountId;
use crate::error::Result;

pub const PLAN_CHANGED: &str = "plan_changed";
pub const OVERRIDES_SET: &str = "overrides_set";
pub const OVERRIDES_CLEARED: &str = "overrides_cleared";
pub const GRACE_STARTED: &str = "grace_started";
pub const GRACE_EXPIRED: &str = "grace_expired";
pub const PAYMENT_REMINDER: &str = "payment_reminder";
pub const PAYMENT_RECOVERED: &str = "payment_recovered";
pub const DOWNGRADE_SCHEDULED: &str = "downgrade_scheduled";
pub const CANCEL_SCHEDULED: &str = "cancel_scheduled";
pub const PENDING_CHANGE_CLEARED: &str = "pending_change_cleared";
pub const SUBSCRIPTION_ENDED: &str = "subscription_ended";
pub const CHECKOUT_STARTED: &str = "checkout_started";
pub const PAYMENT_RECEIVED: &str = "payment_received";
pub const REFUND_RECORDED: &str = "refund_recorded";
pub const FOREIGN_SUBSCRIPTION_IGNORED: &str = "foreign_subscription_ignored";

/// Written inside the transaction that makes the change, so the ledger can
/// never claim something the commit did not do.
pub async fn record_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
    kind: &str,
    payload: Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO account_billing_events (account_id, kind, payload) VALUES ($1, $2, $3)",
    )
    .bind(account.0)
    .bind(kind)
    .bind(payload)
    .execute(&mut **tx)
    .await
    .context("billing_events::record_tx")?;
    Ok(())
}

/// What the account was charged in a recorded payment, as its `total`
/// object (`amount_minor`, `currency`), when the provider reported one.
pub async fn payment_total(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
    transaction_ref: &str,
) -> Result<Option<Value>> {
    let row: Option<(Option<Value>,)> = sqlx::query_as(
        "SELECT payload->'total' FROM account_billing_events
          WHERE account_id = $1 AND kind = 'payment_received'
            AND payload->>'transaction' = $2
          ORDER BY id DESC LIMIT 1",
    )
    .bind(account.0)
    .bind(transaction_ref)
    .fetch_optional(&mut **tx)
    .await
    .context("billing_events::payment_total")?;
    Ok(row.and_then(|(total,)| total).filter(|t| !t.is_null()))
}

/// The account that made a recorded payment, for money going back on a
/// subscription the account has since replaced.
pub async fn account_for_payment(
    pool: &sqlx::PgPool,
    transaction_ref: &str,
) -> Result<Option<AccountId>> {
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT account_id FROM account_billing_events
          WHERE kind = 'payment_received' AND payload->>'transaction' = $1
          ORDER BY id DESC LIMIT 1",
    )
    .bind(transaction_ref)
    .fetch_optional(pool)
    .await
    .context("billing_events::account_for_payment")?;
    Ok(row.map(|(id,)| AccountId(id)))
}
