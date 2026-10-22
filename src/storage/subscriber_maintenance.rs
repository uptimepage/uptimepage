//! Maintenance windows a page's subscribers have not been told about yet. The
//! automatic notices are one per `(page, window, phase)`: a `scheduled`
//! announcement when the window is known and a `completed` notice when it
//! ends. An update a person posts is one per `(page, update)`.

use anyhow::Context;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::Result;
use crate::public_status::subscriber_notice::PageRow;
use crate::storage::admin::not_held_sql;
use crate::storage::page_sql::{PAGE_PLAN_JOIN, PAGE_SERVED, page_row_columns};
use crate::storage::subscribers::FANOUT_LOOKBACK_HOURS;

/// Whether window `mw` shows on page `sp`: through one of the page's
/// components that is not held and is in the window, or was in it before the
/// window dropped it, or because the page kept the window after its monitor
/// left the page. A dropped or kept monitor makes the window history, so the
/// page hears only what was said once it ended (`said_at` on or after its end).
fn shown_on_page_sql(said_at: &str) -> String {
    format!(
        "(EXISTS (
             SELECT 1 FROM status_page_components c
             JOIN maintenance_window_components mwc
               ON mwc.target_id = c.target_id AND mwc.org_id = c.org_id
             WHERE c.status_page_id = sp.id AND c.org_id = sp.org_id
               AND mwc.maintenance_id = mw.id
               AND {not_held})
         OR EXISTS (
             SELECT 1 FROM maintenance_status_pages kept
             WHERE kept.maintenance_id = mw.id AND kept.org_id = mw.org_id
               AND kept.status_page_id = sp.id
               AND mw.ends_at <= now() AND {said_at} >= mw.ends_at)
         OR EXISTS (
             SELECT 1 FROM status_page_components c
             JOIN maintenance_window_departures d
               ON d.target_id = c.target_id AND d.org_id = c.org_id
             WHERE c.status_page_id = sp.id AND c.org_id = sp.org_id
               AND d.maintenance_id = mw.id
               AND {not_held}
               AND mw.ends_at <= now() AND {said_at} >= mw.ends_at))",
        not_held = not_held_sql("c.target_id"),
    )
}

#[derive(Debug, sqlx::FromRow)]
pub struct WindowNoticeRow {
    #[sqlx(flatten)]
    pub page: PageRow,
    pub maintenance_id: Uuid,
    pub phase: String,
    pub title: String,
    pub description: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// A window's `scheduled` or `completed` notice a served page owes. `event_at`
/// (the window's creation for `scheduled`, its end for `completed`) gates the
/// lookback so neither phase resurrects history. The candidate subquery keeps
/// only windows ending inside the lookback or still ahead, so a pass never
/// joins the whole history.
pub async fn pending_window_notices(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<WindowNoticeRow>> {
    let sql = format!(
        "SELECT page_id, org_id, page_name, public_locale, slug, custom_domain,
                custom_domain_published, maintenance_id, phase, title, description,
                starts_at, ends_at
         FROM (
             SELECT {page},
                    mw.id AS maintenance_id, mw.title, mw.description, mw.starts_at, mw.ends_at,
                    CASE WHEN mw.ends_at <= now() THEN 'completed' ELSE 'scheduled' END AS phase,
                    CASE WHEN mw.ends_at <= now() THEN mw.ends_at ELSE mw.created_at END AS event_at
             FROM maintenance_windows mw
             JOIN status_pages sp ON sp.org_id = mw.org_id
             {PAGE_PLAN_JOIN}
             WHERE mw.deleted_at IS NULL
               AND mw.ends_at >= now() - make_interval(hours => $2)
               AND {PAGE_SERVED}
               AND {SHOWN_ON_PAGE}
         ) cand
         WHERE event_at >= now() - make_interval(hours => $2)
           AND NOT EXISTS (
               SELECT 1 FROM status_page_notices n
               WHERE n.status_page_id = cand.page_id AND n.kind = 'maintenance'
                 AND n.source_ref = cand.maintenance_id AND n.phase = cand.phase)
         ORDER BY starts_at
         LIMIT $1",
        SHOWN_ON_PAGE = shown_on_page_sql("mw.ends_at"),
        page = page_row_columns(),
    );
    let rows = sqlx::query_as::<_, WindowNoticeRow>(&sql)
        .bind(limit)
        .bind(FANOUT_LOOKBACK_HOURS as i32)
        .fetch_all(conn)
        .await
        .context("subscriber_maintenance::pending_window_notices")?;
    Ok(rows)
}

/// An update posted to a window, owed to one page.
#[derive(Debug, sqlx::FromRow)]
pub struct WindowUpdateNoticeRow {
    #[sqlx(flatten)]
    pub page: PageRow,
    pub update_id: Uuid,
    pub maintenance_id: Uuid,
    pub phase: String,
    pub message: String,
    pub title: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// Updates posted to a window that is not cancelled and that a served page
/// shows, through one of its components or because the page kept the window
/// after its monitor left. Reads from the updates inside the lookback, so a
/// pass never walks the whole history.
pub async fn pending_window_update_notices(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<WindowUpdateNoticeRow>> {
    let sql = format!(
        "SELECT {page},
                u.id AS update_id, mw.id AS maintenance_id, u.phase, u.message, mw.title,
                mw.starts_at, mw.ends_at
         FROM maintenance_updates u
         JOIN maintenance_windows mw ON mw.id = u.maintenance_id AND mw.org_id = u.org_id
         JOIN status_pages sp ON sp.org_id = u.org_id
         {PAGE_PLAN_JOIN}
         WHERE u.posted_at >= now() - make_interval(hours => $2)
           AND mw.deleted_at IS NULL
           AND {PAGE_SERVED}
           AND {SHOWN_ON_PAGE}
           AND NOT EXISTS (
               SELECT 1 FROM status_page_notices n
               WHERE n.status_page_id = sp.id AND n.kind = 'maintenance_update'
                 AND n.source_ref = u.id AND n.phase = '')
         ORDER BY u.posted_at
         LIMIT $1",
        SHOWN_ON_PAGE = shown_on_page_sql("u.posted_at"),
        page = page_row_columns(),
    );
    let rows = sqlx::query_as::<_, WindowUpdateNoticeRow>(&sql)
        .bind(limit)
        .bind(FANOUT_LOOKBACK_HOURS as i32)
        .fetch_all(conn)
        .await
        .context("subscriber_maintenance::pending_window_update_notices")?;
    Ok(rows)
}
