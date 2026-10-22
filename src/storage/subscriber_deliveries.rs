//! Deliveries: one per (notice, subscriber). A sender claims only the next
//! unit it will send itself, holds it under a lease it keeps renewing, and
//! writes the outcome the moment the unit is done. Every renewal and outcome
//! write matches the claim's token, so a sender that lost its rows changes
//! nothing.

use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::storage::page_sql::PAGE_PLAN_JOIN;
use crate::storage::subscriber_notices::deliverable_sql;

/// One claimed delivery and where it goes.
#[derive(Debug, sqlx::FromRow)]
pub struct Claimed {
    pub id: Uuid,
    pub subscriber_id: Uuid,
    pub target: String,
    pub signing_secret: Option<String>,
    pub attempts: i32,
}

/// The notice a unit belongs to.
#[derive(Debug, sqlx::FromRow)]
pub struct ClaimedNotice {
    pub id: Uuid,
    pub payload: serde_json::Value,
    pub deliverable: bool,
}

/// Up to `max` due deliveries of one notice on one channel, oldest notice
/// first, now held under `token` for `lease`.
pub async fn claim(
    pool: &PgPool,
    channel: &str,
    max: i64,
    token: Uuid,
    lease: Duration,
) -> Result<Option<(ClaimedNotice, Vec<Claimed>)>> {
    let mut tx = pool
        .begin()
        .await
        .context("subscriber_deliveries::claim begin")?;
    let ids: Vec<(Uuid,)> = sqlx::query_as(
        "WITH head AS (
             SELECT notice_id FROM status_page_subscriber_deliveries
             WHERE channel = $1 AND status = 'queued' AND next_attempt_at <= now()
               AND batch_key IS NULL
             ORDER BY next_attempt_at
             LIMIT 1
             FOR UPDATE SKIP LOCKED
         ), picked AS (
             SELECT d.id FROM status_page_subscriber_deliveries d
             WHERE d.notice_id = (SELECT notice_id FROM head) AND d.channel = $1
               AND d.status = 'queued' AND d.next_attempt_at <= now() AND d.batch_key IS NULL
             ORDER BY d.id
             LIMIT $2
             FOR UPDATE SKIP LOCKED
         )
         UPDATE status_page_subscriber_deliveries d
         SET status = 'sending', claim_token = $3,
             lease_until = now() + make_interval(secs => $4)
         FROM picked WHERE d.id = picked.id
         RETURNING d.id",
    )
    .bind(channel)
    .bind(max)
    .bind(token)
    .bind(lease.as_secs_f64())
    .fetch_all(&mut *tx)
    .await
    .context("subscriber_deliveries::claim")?;
    tx.commit()
        .await
        .context("subscriber_deliveries::claim commit")?;
    if ids.is_empty() {
        return Ok(None);
    }
    let deliveries: Vec<Claimed> = sqlx::query_as(
        "SELECT d.id, d.subscriber_id, s.target, s.config ->> 'signing_secret' AS signing_secret,
                d.attempts
         FROM status_page_subscriber_deliveries d
         JOIN status_page_subscribers s ON s.id = d.subscriber_id AND s.org_id = d.org_id
         WHERE d.claim_token = $1 AND d.status = 'sending'
         ORDER BY d.id",
    )
    .bind(token)
    .fetch_all(pool)
    .await
    .context("subscriber_deliveries::claim read")?;
    let sql = format!(
        "SELECT n.id, n.payload, {deliverable} AS deliverable
         FROM status_page_notices n
         JOIN status_page_subscriber_deliveries d ON d.notice_id = n.id AND d.org_id = n.org_id
         JOIN status_pages sp ON sp.id = n.status_page_id AND sp.org_id = n.org_id
         {PAGE_PLAN_JOIN}
         WHERE d.claim_token = $1 AND d.status = 'sending'
         LIMIT 1",
        deliverable = deliverable_sql(),
    );
    let notice: Option<ClaimedNotice> = sqlx::query_as(&sql)
        .bind(token)
        .fetch_optional(pool)
        .await
        .context("subscriber_deliveries::claim notice")?;
    // Rows deleted between the claim and the reads (an unsubscribe) leave
    // nothing to send.
    Ok(notice.map(|n| (n, deliveries)))
}

/// Keeps a held unit's lease alive. `false` once the rows are no longer this
/// claim's.
pub async fn renew(pool: &PgPool, token: Uuid, lease: Duration) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE status_page_subscriber_deliveries
         SET lease_until = now() + make_interval(secs => $2)
         WHERE claim_token = $1 AND status = 'sending'",
    )
    .bind(token)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await
    .context("subscriber_deliveries::renew")?;
    Ok(res.rows_affected() > 0)
}

/// What became of one delivery.
pub struct Outcome {
    pub id: Uuid,
    pub status: &'static str,
    pub error: Option<String>,
    pub attempts: i32,
    /// When a delivery going back to `queued` is next due.
    pub retry_in: Duration,
}

/// Writes a unit's outcomes, only for rows this claim still holds.
pub async fn record(pool: &PgPool, token: Uuid, outcomes: &[Outcome]) -> Result<u64> {
    if outcomes.is_empty() {
        return Ok(0);
    }
    let res = sqlx::query(
        "UPDATE status_page_subscriber_deliveries d
         SET status = o.status, error = o.error, attempts = o.attempts,
             next_attempt_at = now() + make_interval(secs => o.retry_in),
             sent_at = CASE WHEN o.status = 'sent' THEN now() ELSE d.sent_at END,
             claim_token = NULL, lease_until = NULL
         FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::int[], $6::float8[])
              AS o(id, status, error, attempts, retry_in)
         WHERE d.id = o.id AND d.claim_token = $1 AND d.status = 'sending'",
    )
    .bind(token)
    .bind(outcomes.iter().map(|o| o.id).collect::<Vec<_>>())
    .bind(outcomes.iter().map(|o| o.status).collect::<Vec<_>>())
    .bind(outcomes.iter().map(|o| o.error.clone()).collect::<Vec<_>>())
    .bind(outcomes.iter().map(|o| o.attempts).collect::<Vec<_>>())
    .bind(
        outcomes
            .iter()
            .map(|o| o.retry_in.as_secs_f64())
            .collect::<Vec<_>>(),
    )
    .execute(pool)
    .await
    .context("subscriber_deliveries::record")?;
    Ok(res.rows_affected())
}

/// Hands rows whose sender stopped renewing back to the queue.
pub async fn release_expired(pool: &PgPool) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE status_page_subscriber_deliveries
         SET status = 'queued', claim_token = NULL, lease_until = NULL
         WHERE status = 'sending' AND lease_until < now()",
    )
    .execute(pool)
    .await
    .context("subscriber_deliveries::release_expired")?;
    Ok(res.rows_affected())
}
