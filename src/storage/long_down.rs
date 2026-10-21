//! Putting a long-down monitor back on its own interval.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::OrgId;
use crate::error::Result;

/// A check someone ran on a slowed monitor passed. Clears the episode that was
/// loaded with the monitor, never a newer one, and returns whether it did.
pub async fn clear_checked(
    pool: &PgPool,
    org: OrgId,
    target_id: Uuid,
    seen: DateTime<Utc>,
) -> Result<bool> {
    let cleared = sqlx::query(
        "UPDATE targets SET slowed_at = NULL \
         WHERE org_id = $1 AND id = $2 AND slowed_at = $3",
    )
    .bind(org.0)
    .bind(target_id)
    .bind(seen)
    .execute(pool)
    .await
    .context("restore a checked long-down monitor")?
    .rows_affected();
    Ok(cleared > 0)
}
