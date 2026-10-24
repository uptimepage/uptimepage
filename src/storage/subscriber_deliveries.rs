//! Deliveries: one per (notice, subscriber). A sender claims only the next
//! unit it will send itself, holds it under a lease it keeps renewing, and
//! writes the outcome the moment the unit is done. Every renewal and outcome
//! write matches the claim's token, so a sender that lost its rows changes
//! nothing.
//!
//! A mail batch whose send got no clear answer is stored as it went out, and
//! its deliveries wait for it under its key: they are claimed again only as
//! that whole batch, and sent as the stored bytes.

use std::time::Duration;

use anyhow::Context;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::storage::page_sql::PAGE_PLAN_JOIN;
use crate::storage::subscriber_notices::{EMAIL, WEBHOOK, deliverable_sql};

/// A delivery still unsent this long after its notice is no longer news.
const EXPIRES_AFTER_HOURS: i32 = 24;

/// One claimed delivery and where it goes.
#[derive(Debug, sqlx::FromRow)]
pub struct Claimed {
    pub id: Uuid,
    pub subscriber_id: Uuid,
    pub target: String,
    pub signing_secret: Option<String>,
    pub attempts: i32,
    pub batch_key: Option<String>,
}

/// The notice a unit belongs to.
#[derive(Debug, sqlx::FromRow)]
pub struct ClaimedNotice {
    pub id: Uuid,
    pub payload: serde_json::Value,
    pub deliverable: bool,
    pub expired: bool,
}

/// A mail batch kept as it went out.
#[derive(Debug, sqlx::FromRow)]
pub struct StoredBatch {
    pub key: String,
    pub request: Vec<u8>,
    /// Position i of the request is `delivery_ids[i]`.
    pub delivery_ids: Vec<Uuid>,
}

/// What one sender holds: deliveries of one notice, and the stored batch
/// they must go out as, if any.
#[derive(Debug)]
pub struct Unit {
    pub notice: ClaimedNotice,
    pub deliveries: Vec<Claimed>,
    pub stored: Option<StoredBatch>,
}

/// The next unit on `channel`, held under `token` for `lease`: a due stored
/// batch, whole, or else up to `max` due deliveries of one notice, oldest
/// first.
pub async fn claim(
    pool: &PgPool,
    channel: &str,
    max: i64,
    token: Uuid,
    lease: Duration,
) -> Result<Option<Unit>> {
    let held = (channel == EMAIL && claim_stored(pool, token, lease).await?)
        || claim_due(pool, channel, max, token, lease).await?;
    if !held {
        return Ok(None);
    }
    read_unit(pool, token).await
}

/// Locking the batch row lets only one sender take a stored batch.
async fn claim_stored(pool: &PgPool, token: Uuid, lease: Duration) -> Result<bool> {
    let res = sqlx::query(
        "WITH b AS (
             SELECT b.key FROM status_page_delivery_batches b
             WHERE EXISTS (SELECT 1 FROM status_page_subscriber_deliveries d
                           WHERE d.batch_key = b.key AND d.status = 'queued'
                             AND d.next_attempt_at <= now())
             ORDER BY b.created_at
             LIMIT 1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE status_page_subscriber_deliveries d
         SET status = 'sending', claim_token = $1,
             lease_until = now() + make_interval(secs => $2)
         FROM b WHERE d.batch_key = b.key AND d.status = 'queued'",
    )
    .bind(token)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await
    .context("subscriber_deliveries::claim_stored")?;
    Ok(res.rows_affected() > 0)
}

/// The head locks the notice, not a delivery: a concurrent claim then skips
/// the whole notice instead of taking the rows this one has yet to lock and
/// sending them as a second, smaller batch. Nothing here waits on a lock.
async fn claim_due(
    pool: &PgPool,
    channel: &str,
    max: i64,
    token: Uuid,
    lease: Duration,
) -> Result<bool> {
    let res = sqlx::query(
        "WITH head AS (
             SELECT n.id AS notice_id FROM status_page_subscriber_deliveries d
             JOIN status_page_notices n ON n.id = d.notice_id AND n.org_id = d.org_id
             WHERE d.channel = $1 AND d.status = 'queued' AND d.next_attempt_at <= now()
               AND d.batch_key IS NULL
             ORDER BY d.next_attempt_at
             LIMIT 1
             FOR NO KEY UPDATE OF n SKIP LOCKED
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
         FROM picked WHERE d.id = picked.id",
    )
    .bind(channel)
    .bind(max)
    .bind(token)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await
    .context("subscriber_deliveries::claim_due")?;
    Ok(res.rows_affected() > 0)
}

async fn read_unit(pool: &PgPool, token: Uuid) -> Result<Option<Unit>> {
    let deliveries: Vec<Claimed> = sqlx::query_as(
        "SELECT d.id, d.subscriber_id, s.target, s.config ->> 'signing_secret' AS signing_secret,
                d.attempts, d.batch_key
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
        "SELECT n.id, n.payload, {deliverable} AS deliverable,
                n.created_at < now() - make_interval(hours => $2) AS expired
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
        .bind(EXPIRES_AFTER_HOURS)
        .fetch_optional(pool)
        .await
        .context("subscriber_deliveries::claim notice")?;
    // Rows deleted between the claim and the reads (an unsubscribe) leave
    // nothing to send.
    let Some(notice) = notice else {
        return Ok(None);
    };
    let stored = match deliveries.iter().find_map(|d| d.batch_key.as_deref()) {
        Some(key) => sqlx::query_as::<_, StoredBatch>(
            "SELECT key, request, delivery_ids FROM status_page_delivery_batches WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(pool)
        .await
        .context("subscriber_deliveries::claim stored batch")?,
        None => None,
    };
    Ok(Some(Unit {
        notice,
        deliveries,
        stored,
    }))
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
    /// Set while the delivery waits for its stored batch to go out again.
    pub batch_key: Option<String>,
}

/// The request a mail unit went out as.
pub struct SentBatch<'a> {
    pub key: &'a str,
    pub notice_id: Uuid,
    pub request: &'a [u8],
    pub delivery_ids: &'a [Uuid],
}

/// Writes a unit's outcomes, only for rows this claim still holds, and
/// returns the status each written row now has. A batch any outcome still
/// waits for is stored first; one nothing waits for any more is dropped.
pub async fn record(
    pool: &PgPool,
    token: Uuid,
    outcomes: &[Outcome],
    batch: Option<SentBatch<'_>>,
) -> Result<Vec<String>> {
    if outcomes.is_empty() {
        return Ok(Vec::new());
    }
    let mut tx = pool
        .begin()
        .await
        .context("subscriber_deliveries::record begin")?;
    if let Some(b) = &batch
        && outcomes.iter().any(|o| o.batch_key.is_some())
    {
        sqlx::query(
            "INSERT INTO status_page_delivery_batches (key, org_id, notice_id, request, delivery_ids)
             SELECT $1, n.org_id, n.id, $3, $4 FROM status_page_notices n WHERE n.id = $2
             ON CONFLICT (key) DO NOTHING",
        )
        .bind(b.key)
        .bind(b.notice_id)
        .bind(b.request)
        .bind(b.delivery_ids)
        .execute(&mut *tx)
        .await
        .context("subscriber_deliveries::record store batch")?;
    }
    let statuses: Vec<String> = sqlx::query_scalar(
        "UPDATE status_page_subscriber_deliveries d
         SET status = o.status, error = o.error, attempts = o.attempts,
             next_attempt_at = now() + make_interval(secs => o.retry_in),
             sent_at = CASE WHEN o.status = 'sent' THEN now() ELSE d.sent_at END,
             batch_key = o.batch_key, claim_token = NULL, lease_until = NULL
         FROM UNNEST($2::uuid[], $3::text[], $4::text[], $5::int[], $6::float8[], $7::text[])
              AS o(id, status, error, attempts, retry_in, batch_key)
         WHERE d.id = o.id AND d.claim_token = $1 AND d.status = 'sending'
         RETURNING d.status",
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
    .bind(
        outcomes
            .iter()
            .map(|o| o.batch_key.clone())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *tx)
    .await
    .context("subscriber_deliveries::record")?;
    if let Some(b) = &batch {
        sqlx::query(
            "DELETE FROM status_page_delivery_batches b
             WHERE b.key = $1
               AND NOT EXISTS (SELECT 1 FROM status_page_subscriber_deliveries d
                               WHERE d.batch_key = b.key)",
        )
        .bind(b.key)
        .execute(&mut *tx)
        .await
        .context("subscriber_deliveries::record settle batch")?;
    }
    tx.commit()
        .await
        .context("subscriber_deliveries::record commit")?;
    Ok(statuses)
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

/// How long the oldest due delivery on each channel has waited for a sender.
pub async fn oldest_due(pool: &PgPool) -> Result<Vec<(String, f64)>> {
    sqlx::query_as(
        "SELECT c.channel,
                COALESCE(EXTRACT(EPOCH FROM now() - (
                    SELECT min(d.next_attempt_at) FROM status_page_subscriber_deliveries d
                    WHERE d.channel = c.channel AND d.status = 'queued'
                      AND d.next_attempt_at <= now())), 0)::float8
         FROM UNNEST($1::text[]) AS c(channel)",
    )
    .bind([EMAIL, WEBHOOK])
    .fetch_all(pool)
    .await
    .context("subscriber_deliveries::oldest_due")
    .map_err(Into::into)
}

/// Periodic cleanup: stored batches nothing waits for, which a delivery
/// removed with its subscriber can leave behind. Past a day the provider has
/// forgotten their key anyway.
pub async fn purge_batches(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "DELETE FROM status_page_delivery_batches b
         WHERE b.created_at < now() - make_interval(hours => $1)
           AND NOT EXISTS (SELECT 1 FROM status_page_subscriber_deliveries d
                           WHERE d.batch_key = b.key)",
    )
    .bind(EXPIRES_AFTER_HOURS)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
