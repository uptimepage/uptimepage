//! The write path for `plan_overrides`: an operator raising or lowering one
//! account's caps without moving it to another plan.
//!
//! The read side ignores keys it cannot parse, which is right for a row and
//! wrong at the door: a typo'd key would be stored and silently do nothing.
//! Everything here is checked against the field list the read side folds in.

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::api::error::codes;
use crate::domain::AccountId;
use crate::error::{AppError, Result};
use crate::quotas::QuotaService;
use crate::quotas::holds::{Reconciled, reconcile_after_change};
use crate::quotas::service::PlanOverrides;
use crate::storage::billing_events;

/// One bad entry rejects the whole request, so nothing is stored that the
/// operator did not mean.
pub fn validate(caps: &Value) -> Result<()> {
    let Some(map) = caps.as_object() else {
        return Err(AppError::bad_request(
            codes::PLAN_OVERRIDE_INVALID,
            "caps must be an object of cap name to value",
        ));
    };
    if map.is_empty() {
        return Err(AppError::bad_request(
            codes::PLAN_OVERRIDE_INVALID,
            "caps must name at least one cap; DELETE removes the override",
        ));
    }
    for (key, value) in map {
        if !PlanOverrides::KEYS.contains(&key.as_str()) {
            return Err(AppError::bad_request(
                codes::PLAN_OVERRIDE_INVALID,
                format!(
                    "unknown cap {key:?}; expected one of: {}",
                    PlanOverrides::KEYS.join(", ")
                ),
            ));
        }
        // Bounded by the `int4` the SQL readers cast to, not the u32 the
        // struct parses into: a larger value stores fine and then fails every
        // read.
        if value.as_u64().and_then(|v| i32::try_from(v).ok()).is_none() {
            return Err(AppError::bad_request(
                codes::PLAN_OVERRIDE_INVALID,
                format!("{key} must be a whole number from 0 to {}", i32::MAX),
            ));
        }
    }
    Ok(())
}

/// An override can lower a cap as well as raise one, so the reconcile that
/// follows is not optional.
pub async fn set(
    pool: &PgPool,
    quotas: &QuotaService,
    account: AccountId,
    caps: &Value,
    reason: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Reconciled> {
    validate(caps)?;
    let mut tx = pool.begin().await.context("overrides::set: begin")?;
    sqlx::query(
        "INSERT INTO plan_overrides /* SAFE: keyed by the account, which is the quota subject */ \
         (account_id, override_json, reason, expires_at) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (account_id) DO UPDATE \
         SET override_json = EXCLUDED.override_json, reason = EXCLUDED.reason, \
             expires_at = EXCLUDED.expires_at, set_by_user_id = NULL, created_at = now()",
    )
    .bind(account.0)
    .bind(caps)
    .bind(reason)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .context("overrides::set")?;
    billing_events::record_tx(
        &mut tx,
        account,
        billing_events::OVERRIDES_SET,
        json!({ "caps": caps, "reason": reason, "expires_at": expires_at }),
    )
    .await?;
    tx.commit().await.context("overrides::set: commit")?;
    reconcile_after_change(pool, quotas, account, None).await
}

/// Nothing to remove writes no ledger row but still reconciles, so a retry
/// after a failure past the commit converges instead of skipping the step
/// that failed.
pub async fn clear(pool: &PgPool, quotas: &QuotaService, account: AccountId) -> Result<Reconciled> {
    let mut tx = pool.begin().await.context("overrides::clear: begin")?;
    let removed = sqlx::query(
        "DELETE FROM plan_overrides /* SAFE: keyed by the account, which is the quota subject */ \
         WHERE account_id = $1",
    )
    .bind(account.0)
    .execute(&mut *tx)
    .await
    .context("overrides::clear")?
    .rows_affected();
    if removed > 0 {
        billing_events::record_tx(
            &mut tx,
            account,
            billing_events::OVERRIDES_CLEARED,
            json!({}),
        )
        .await?;
        tx.commit().await.context("overrides::clear: commit")?;
    } else {
        drop(tx);
    }
    reconcile_after_change(pool, quotas, account, None).await
}
