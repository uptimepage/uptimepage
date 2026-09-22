//! Maintenance-window fan-out to confirmed subscribers. Mirrors the incident
//! delivery log, but the dedup unit is `(subscriber, window, phase)` since
//! maintenance has no update rows: a `scheduled` announcement when the window is
//! known and a `completed` notice when it ends.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::storage::admin::not_held_sql;
use crate::storage::status_pages::{PAGE_CUSTOM_DOMAIN_PUBLISHED, PAGE_NOT_HELD, PAGE_PLAN_JOIN};
use crate::storage::subscribers::{
    CLAIM_ORPHAN_MINUTES, FANOUT_LOOKBACK_HOURS, FANOUT_MAX_ATTEMPTS,
};

#[derive(Debug, sqlx::FromRow)]
pub struct PendingMaintenance {
    pub subscriber_id: Uuid,
    pub maintenance_id: Uuid,
    pub org_id: Uuid,
    pub channel: String,
    pub target: String,
    pub phase: String,
    pub title: String,
    pub description: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub page_name: String,
    pub slug: String,
    pub custom_domain: Option<String>,
    pub custom_domain_published: bool,
    pub signing_secret: Option<String>,
}

/// Verified subscribers with a maintenance window (touching their page's
/// components) whose `scheduled`/`completed` phase hasn't been sent. `event_at`
/// — the window's creation for `scheduled`, its end for `completed` — gates the
/// lookback and the subscribed-since check so neither phase resurrects history.
pub async fn list_pending(pool: &PgPool, limit: i64) -> Result<Vec<PendingMaintenance>> {
    let sql = format!(
        "SELECT DISTINCT subscriber_id, maintenance_id, org_id, channel, target, phase, title,
                description, starts_at, ends_at, page_name, slug, custom_domain,
                custom_domain_published, signing_secret
         FROM (
             SELECT s.id AS subscriber_id, mw.id AS maintenance_id, s.org_id, s.channel, s.target,
                    s.verified_at,
                    mw.title, mw.description, mw.starts_at, mw.ends_at, mw.created_at,
                    COALESCE(NULLIF(sp.public_display_name, ''), sp.name) AS page_name,
                    sp.slug::text AS slug,
                    sp.custom_domain::text AS custom_domain,
                    {PAGE_CUSTOM_DOMAIN_PUBLISHED} AS custom_domain_published,
                    s.config ->> 'signing_secret' AS signing_secret,
                    CASE WHEN mw.ends_at <= now() THEN 'completed' ELSE 'scheduled' END AS phase,
                    CASE WHEN mw.ends_at <= now() THEN mw.ends_at ELSE mw.created_at END AS event_at
             FROM status_page_subscribers s
             JOIN status_pages sp ON sp.id = s.status_page_id
             {PAGE_PLAN_JOIN}
             JOIN status_page_components c
                  ON c.status_page_id = s.status_page_id AND c.org_id = s.org_id
             JOIN maintenance_window_components mwc
                  ON mwc.target_id = c.target_id AND mwc.org_id = c.org_id
             JOIN maintenance_windows mw ON mw.id = mwc.maintenance_id AND mw.org_id = mwc.org_id
             WHERE s.channel IN ('email', 'webhook') AND s.verified_at IS NOT NULL
               AND {PAGE_NOT_HELD}
               AND {COMPONENT_NOT_HELD}
         ) cand
         WHERE event_at >= verified_at
           AND event_at >= now() - make_interval(hours => $2)
           AND NOT EXISTS (
               SELECT 1 FROM status_page_subscriber_deliveries n
               WHERE n.subscriber_id = cand.subscriber_id
                 AND n.event_kind = 'maintenance'
                 AND n.event_ref = cand.maintenance_id
                 AND n.phase = cand.phase
                 AND (n.status = 'sent'
                   OR (n.status = 'queued' AND n.created_at > now() - make_interval(mins => $3))
                   OR (n.status = 'failed' AND n.attempts >= $4)))
         ORDER BY starts_at
         LIMIT $1",
        COMPONENT_NOT_HELD = not_held_sql("c.target_id"),
    );
    let rows = sqlx::query_as::<_, PendingMaintenance>(&sql)
        .bind(limit)
        .bind(FANOUT_LOOKBACK_HOURS as i32)
        .bind(CLAIM_ORPHAN_MINUTES as i32)
        .bind(FANOUT_MAX_ATTEMPTS)
        .fetch_all(pool)
        .await
        .context("subscriber_maintenance::list_pending")?;
    Ok(rows)
}
