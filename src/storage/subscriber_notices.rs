//! Notices: one per thing a page's subscribers should hear, created once by
//! the planner and fanned out once into one delivery per subscriber.

use anyhow::Context;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::storage::page_sql::{PAGE_PLAN_JOIN, PAGE_SERVED};

pub const INCIDENT_UPDATE: &str = "incident_update";
pub const OUTAGE_STARTED: &str = "outage_started";
pub const OUTAGE_RESOLVED: &str = "outage_resolved";
pub const MAINTENANCE: &str = "maintenance";
pub const MAINTENANCE_UPDATE: &str = "maintenance_update";

pub const EMAIL: &str = "email";
pub const WEBHOOK: &str = "webhook";

/// Subscribers inserted per statement, so a page with tens of thousands
/// fans out in a handful of bounded writes.
const FAN_OUT_CHUNK: i64 = 10_000;
/// Rows a purge deletes per statement.
const PURGE_CHUNK: i64 = 10_000;

pub struct NewNotice {
    pub org_id: Uuid,
    pub page_id: Uuid,
    pub kind: &'static str,
    pub source_ref: Uuid,
    pub phase: String,
    pub channels: Vec<&'static str>,
    pub payload: serde_json::Value,
}

/// Inserts each notice once: a second planner pass over the same source
/// finds the notice key taken and adds nothing.
pub async fn insert(conn: &mut sqlx::PgConnection, notices: &[NewNotice]) -> Result<u64> {
    if notices.is_empty() {
        return Ok(0);
    }
    let res = sqlx::query(
        "INSERT INTO status_page_notices
             (org_id, status_page_id, kind, source_ref, phase, channels, payload)
         SELECT org_id, page_id, kind, source_ref, phase, string_to_array(channels, ','), payload
         FROM UNNEST($1::uuid[], $2::uuid[], $3::text[], $4::uuid[], $5::text[], $6::text[],
                     $7::jsonb[])
              AS n(org_id, page_id, kind, source_ref, phase, channels, payload)
         ON CONFLICT (status_page_id, kind, source_ref, phase) DO NOTHING",
    )
    .bind(notices.iter().map(|n| n.org_id).collect::<Vec<_>>())
    .bind(notices.iter().map(|n| n.page_id).collect::<Vec<_>>())
    .bind(notices.iter().map(|n| n.kind).collect::<Vec<_>>())
    .bind(notices.iter().map(|n| n.source_ref).collect::<Vec<_>>())
    .bind(notices.iter().map(|n| n.phase.clone()).collect::<Vec<_>>())
    .bind(
        notices
            .iter()
            .map(|n| n.channels.join(","))
            .collect::<Vec<_>>(),
    )
    .bind(
        notices
            .iter()
            .map(|n| n.payload.clone())
            .collect::<Vec<_>>(),
    )
    .execute(conn)
    .await
    .context("subscriber_notices::insert")?;
    Ok(res.rows_affected())
}

/// Whether a notice may still be delivered, for a query that aliases the
/// notice `n` and its page `sp`: the page is served, and a started notice's
/// outage has not ended in the meantime.
pub(crate) fn deliverable_sql() -> String {
    format!(
        "({PAGE_SERVED} AND NOT (n.kind = '{OUTAGE_STARTED}' AND EXISTS (
             SELECT 1 FROM status_page_outages o
             WHERE o.id = n.source_ref AND o.org_id = n.org_id AND o.resolved_at IS NOT NULL)))"
    )
}

/// The oldest notice not fanned out yet, leaving out `skip`: notices whose
/// fan-out already failed this pass, so one that keeps failing does not hold
/// up the rest.
pub async fn next_unfanned(pool: &PgPool, skip: &[Uuid]) -> Result<Option<Uuid>> {
    let next: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM status_page_notices
         WHERE fanned_out_at IS NULL AND id <> ALL($1)
         ORDER BY created_at, id
         LIMIT 1",
    )
    .bind(skip)
    .fetch_optional(pool)
    .await
    .context("subscriber_notices::next_unfanned")?;
    Ok(next.map(|(id,)| id))
}

/// Fans out one notice, unless another instance is fanning it out or did:
/// one delivery per verified subscriber of the page on one of its channels
/// who confirmed by the time it was created. A page no longer served, or a
/// started notice whose outage has ended, gets none. `false` when another
/// instance holds it.
pub async fn fan_out(pool: &PgPool, notice: Uuid) -> Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .context("subscriber_notices::fan_out begin")?;
    let sql = format!(
        "SELECT n.org_id, {deliverable} AS deliverable
         FROM status_page_notices n
         JOIN status_pages sp ON sp.id = n.status_page_id AND sp.org_id = n.org_id
         {PAGE_PLAN_JOIN}
         WHERE n.id = $1 AND n.fanned_out_at IS NULL
         FOR UPDATE OF n SKIP LOCKED",
        deliverable = deliverable_sql(),
    );
    let claimed: Option<(Uuid, bool)> = sqlx::query_as(&sql)
        .bind(notice)
        .fetch_optional(&mut *tx)
        .await
        .context("subscriber_notices::fan_out claim")?;
    let Some((org, deliverable)) = claimed else {
        return Ok(false);
    };
    if deliverable {
        let mut after = Uuid::nil();
        loop {
            let (taken, last): (i64, Option<Uuid>) = sqlx::query_as(
                "WITH chunk AS (
                     SELECT s.id, s.channel FROM status_page_subscribers s
                     JOIN status_page_notices n ON n.id = $1 AND n.org_id = $2
                     WHERE s.status_page_id = n.status_page_id AND s.org_id = n.org_id
                       AND s.verified_at IS NOT NULL AND s.verified_at <= n.created_at
                       AND s.channel = ANY(n.channels)
                       AND s.id > $3
                     ORDER BY s.id
                     LIMIT $4
                 ), ins AS (
                     INSERT INTO status_page_subscriber_deliveries
                         (org_id, notice_id, subscriber_id, channel)
                     SELECT $2, $1, id, channel FROM chunk
                     ON CONFLICT (notice_id, subscriber_id) DO NOTHING
                 )
                 SELECT (SELECT count(*) FROM chunk), (SELECT id FROM chunk ORDER BY id DESC LIMIT 1)",
            )
            .bind(notice)
            .bind(org)
            .bind(after)
            .bind(FAN_OUT_CHUNK)
            .fetch_one(&mut *tx)
            .await
            .context("subscriber_notices::fan_out chunk")?;
            match last {
                Some(last) if taken == FAN_OUT_CHUNK => after = last,
                _ => break,
            }
        }
    }
    sqlx::query(
        "UPDATE status_page_notices SET fanned_out_at = now() WHERE id = $1 AND org_id = $2",
    )
    .bind(notice)
    .bind(org)
    .execute(&mut *tx)
    .await
    .context("subscriber_notices::fan_out stamp")?;
    tx.commit()
        .await
        .context("subscriber_notices::fan_out commit")?;
    Ok(true)
}

/// Periodic cleanup: notices older than 30 days with their deliveries, in
/// bounded chunks.
pub async fn purge_old(pool: &PgPool) -> sqlx::Result<u64> {
    let mut purged = 0;
    loop {
        let res = sqlx::query(
            "DELETE FROM status_page_notices
             WHERE id IN (SELECT id FROM status_page_notices
                          WHERE created_at < now() - INTERVAL '30 days'
                          LIMIT $1)",
        )
        .bind(PURGE_CHUNK)
        .execute(pool)
        .await?;
        purged += res.rows_affected();
        if res.rows_affected() < PURGE_CHUNK as u64 {
            return Ok(purged);
        }
    }
}
