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
