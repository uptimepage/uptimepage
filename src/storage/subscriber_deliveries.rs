//! One delivery log for every subscriber fan-out event type. The dedup unit is
//! `(subscriber, event_kind, event_ref, phase)`; `phase` is `""` for events that
//! have none (incident updates) and the window phase for maintenance.

use anyhow::Context;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::storage::subscribers::{CLAIM_ORPHAN_MINUTES, FANOUT_MAX_ATTEMPTS};

/// Claim a delivery; `true` if this caller won it, `false` if another
/// tick/replica already holds it. Re-claims only a `failed` row under the
/// attempts cap or an orphaned `queued` row, so two concurrent fresh inserts
/// can't both win.
pub async fn claim(
    pool: &PgPool,
    subscriber_id: Uuid,
    org_id: Uuid,
    event_kind: &str,
    event_ref: Uuid,
    phase: &str,
) -> Result<bool> {
    let claimed: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO status_page_subscriber_deliveries
             (subscriber_id, org_id, event_kind, event_ref, phase, status)
         VALUES ($1, $2, $3, $4, $5, 'queued')
         ON CONFLICT (subscriber_id, event_kind, event_ref, phase) DO UPDATE
             SET status = 'queued', error = NULL, created_at = now()
             WHERE (status_page_subscriber_deliveries.status = 'failed'
                    AND status_page_subscriber_deliveries.attempts < $7)
                OR (status_page_subscriber_deliveries.status = 'queued'
                    AND status_page_subscriber_deliveries.created_at
                        < now() - make_interval(mins => $6))
         RETURNING id",
    )
    .bind(subscriber_id)
    .bind(org_id)
    .bind(event_kind)
    .bind(event_ref)
    .bind(phase)
    .bind(CLAIM_ORPHAN_MINUTES as i32)
    .bind(FANOUT_MAX_ATTEMPTS)
    .fetch_optional(pool)
    .await
    .context("subscriber_deliveries::claim")?;
    Ok(claimed.is_some())
}

/// `counts` is false for a send held back by pacing: it goes out again
/// without using up an attempt.
pub async fn mark(
    pool: &PgPool,
    subscriber_id: Uuid,
    event_kind: &str,
    event_ref: Uuid,
    phase: &str,
    error: Option<&str>,
    counts: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE status_page_subscriber_deliveries
         SET status = CASE WHEN $5::text IS NULL THEN 'sent' ELSE 'failed' END,
             error = $5,
             attempts = attempts + CASE WHEN $5::text IS NULL OR NOT $6 THEN 0 ELSE 1 END,
             sent_at = CASE WHEN $5::text IS NULL THEN now() ELSE sent_at END
         WHERE subscriber_id = $1 AND event_kind = $2 AND event_ref = $3 AND phase = $4",
    )
    .bind(subscriber_id)
    .bind(event_kind)
    .bind(event_ref)
    .bind(phase)
    .bind(error)
    .bind(counts)
    .execute(pool)
    .await
    .context("subscriber_deliveries::mark")?;
    Ok(())
}

pub async fn purge_old(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "DELETE FROM status_page_subscriber_deliveries
         WHERE created_at < now() - INTERVAL '30 days'",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Event-kind discriminators stored in `status_page_subscriber_deliveries`.
pub const INCIDENT_UPDATE: &str = "incident_update";
pub const MAINTENANCE: &str = "maintenance";
pub const MAINTENANCE_UPDATE: &str = "maintenance_update";
