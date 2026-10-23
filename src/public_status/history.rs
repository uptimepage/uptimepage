//! A page's history: the maintenance windows it has seen start, and the
//! merge that lists them with its incidents newest first.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::domain::{OrgId, PublicIncident, PublicMaintenance, PublicMaintenanceUpdate};
use crate::pagination::cursor::HistoryCursor;
use crate::pagination::page::CursorPage;
use crate::storage::maintenance::started_sql;

/// Which started windows to read, newest start first.
#[derive(Debug, Clone, Copy)]
pub struct WindowQuery {
    pub now: DateTime<Utc>,
    /// Only windows that started at or after this, or with `by_end` that
    /// ended at or after it.
    pub since: DateTime<Utc>,
    /// Bound `since` on the end, so a window that began before the span but
    /// ran into it is listed, as the day strip it paints links to it.
    pub by_end: bool,
    /// Leave out a window still running.
    pub ended_only: bool,
    pub cursor: Option<HistoryCursor>,
    pub limit: i64,
}

#[derive(FromRow)]
struct WindowRow {
    id: Uuid,
    title: String,
    description: Option<String>,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    component_ids: Vec<Uuid>,
    kept_targets: Vec<Uuid>,
    kept_names: Vec<String>,
}

/// Windows that have started on any of `components`, or that ended and had
/// one of them before dropping it or that `page` kept after their monitor
/// left it, cancelled ones left out. Each names the components it touches on
/// the page, in the order given, then the ones the page kept.
pub(crate) async fn page_windows(
    pg: &PgPool,
    org: OrgId,
    page: Uuid,
    components: &[Uuid],
    names: &HashMap<Uuid, String>,
    q: WindowQuery,
) -> Result<Vec<PublicMaintenance>> {
    let (cursor_ts, cursor_id) = q
        .cursor
        .map_or((None, None), |c| (Some(c.started_at), Some(c.id)));
    let rows: Vec<WindowRow> = sqlx::query_as::<_, WindowRow>(&format!(
        r#"SELECT mw.id, mw.title, mw.description, mw.starts_at, mw.ends_at,
                  COALESCE(ARRAY_AGG(mwc.target_id) FILTER (WHERE mwc.target_id IS NOT NULL),
                           '{{}}'::uuid[])
                      || COALESCE(departed.targets, '{{}}'::uuid[]) AS component_ids,
                  COALESCE(kept.targets, '{{}}'::uuid[]) AS kept_targets,
                  COALESCE(kept.names, '{{}}'::text[]) AS kept_names
           FROM maintenance_windows mw
           LEFT JOIN maintenance_window_components mwc
             ON mwc.maintenance_id = mw.id
            AND mwc.org_id = mw.org_id
            AND mwc.target_id = ANY($2)
           LEFT JOIN LATERAL (
               SELECT ARRAY_AGG(k.target_ref ORDER BY k.component_name) AS targets,
                      ARRAY_AGG(k.component_name ORDER BY k.component_name) AS names
               FROM maintenance_status_pages k
               WHERE k.maintenance_id = mw.id AND k.org_id = mw.org_id
                 AND k.status_page_id = $9 AND mw.ends_at <= $3
           ) kept ON true
           LEFT JOIN LATERAL (
               SELECT ARRAY_AGG(DISTINCT d.target_id) AS targets
               FROM maintenance_window_departures d
               WHERE d.maintenance_id = mw.id AND d.org_id = mw.org_id
                 AND d.target_id = ANY($2) AND mw.ends_at <= $3
           ) departed ON true
           WHERE mw.org_id = $1
             AND {started}
             AND (CASE WHEN $10 THEN mw.ends_at ELSE mw.starts_at END) >= $4
             AND ($5 = false OR mw.ends_at <= $3)
             AND (
                 $6::timestamptz IS NULL
                 OR (mw.starts_at, mw.id) < ($6::timestamptz, $7::uuid)
             )
             AND (mwc.target_id IS NOT NULL OR kept.targets IS NOT NULL
                  OR departed.targets IS NOT NULL)
           GROUP BY mw.id, kept.targets, kept.names, departed.targets
           ORDER BY mw.starts_at DESC, mw.id DESC
           LIMIT $8"#,
        started = started_sql("mw.", "$3"),
    ))
    .bind(org.0)
    .bind(components)
    .bind(q.now)
    .bind(q.since)
    .bind(q.ended_only)
    .bind(cursor_ts)
    .bind(cursor_id)
    .bind(q.limit)
    .bind(page)
    .bind(q.by_end)
    .fetch_all(pg)
    .await
    .context("load page maintenance history")?;

    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let mut updates = public_updates(pg, org, &ids).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut affected = names_on_page(components, &r.component_ids, names);
            affected.extend(
                r.kept_targets
                    .iter()
                    .zip(r.kept_names)
                    .filter(|(target, _)| !r.component_ids.contains(target))
                    .map(|(_, name)| name),
            );
            PublicMaintenance {
                affected_component_names: affected,
                updates: updates.remove(&r.id).unwrap_or_default(),
                id: r.id,
                title: r.title,
                description: r.description,
                starts_at: r.starts_at,
                ends_at: r.ends_at,
            }
        })
        .collect())
}

/// The updates posted to each of `windows`, oldest first.
pub(crate) async fn public_updates(
    pg: &PgPool,
    org: OrgId,
    windows: &[Uuid],
) -> Result<HashMap<Uuid, Vec<PublicMaintenanceUpdate>>> {
    if windows.is_empty() {
        return Ok(HashMap::new());
    }
    let loaded = crate::storage::maintenance::load_updates(pg, windows, org.0)
        .await
        .map_err(|e| anyhow::anyhow!("load maintenance updates: {e}"))?;
    Ok(loaded
        .into_iter()
        .map(|(id, updates)| (id, updates.into_iter().map(Into::into).collect()))
        .collect())
}

/// The public names of the `touched` components that sit on the page, in the
/// page's order, so a window reads the same while it runs and once it ended.
pub(crate) fn names_on_page(
    components: &[Uuid],
    touched: &[Uuid],
    names: &HashMap<Uuid, String>,
) -> Vec<String> {
    let touched: HashSet<&Uuid> = touched.iter().collect();
    components
        .iter()
        .filter(|id| touched.contains(id))
        .filter_map(|id| names.get(id).cloned())
        .collect()
}

impl From<&PublicIncident> for HistoryCursor {
    fn from(i: &PublicIncident) -> Self {
        Self {
            started_at: i.started_at,
            id: i.id,
        }
    }
}

impl From<&PublicMaintenance> for HistoryCursor {
    fn from(m: &PublicMaintenance) -> Self {
        Self {
            started_at: m.starts_at,
            id: m.id,
        }
    }
}

/// One row of a history list.
#[derive(Debug, Clone)]
pub enum HistoryEntry {
    Incident(PublicIncident),
    Maintenance(PublicMaintenance),
}

impl HistoryEntry {
    pub fn started_at(&self) -> DateTime<Utc> {
        self.position().started_at
    }

    fn position(&self) -> HistoryCursor {
        match self {
            Self::Incident(i) => i.into(),
            Self::Maintenance(m) => m.into(),
        }
    }
}

/// One page of a merged history.
#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    /// Where the next page starts; `None` on the last.
    pub next_cursor: Option<String>,
}

/// Two pages read past the same cursor, merged newest first and cut to
/// `limit`. The next cursor is the last row kept, so a row the cut dropped is
/// read again on the next page.
pub fn merge(
    incidents: CursorPage<PublicIncident>,
    maintenance: CursorPage<PublicMaintenance>,
    limit: usize,
) -> HistoryPage {
    let more_behind = incidents.next_cursor.is_some() || maintenance.next_cursor.is_some();
    let mut entries: Vec<HistoryEntry> = incidents
        .items
        .into_iter()
        .map(HistoryEntry::Incident)
        .chain(maintenance.items.into_iter().map(HistoryEntry::Maintenance))
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.position()));
    let cut = entries.len() > limit;
    entries.truncate(limit);
    let next_cursor = (cut || more_behind)
        .then(|| entries.last().map(|e| e.position().encode()))
        .flatten();
    HistoryPage {
        entries,
        next_cursor,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::domain::{IncidentImpact, IncidentSeverity, IncidentStatusPhase};

    fn at(minute: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + Duration::minutes(minute)
    }

    fn incident(minute: i64, id: u128) -> PublicIncident {
        PublicIncident {
            id: Uuid::from_u128(id),
            component_id: None,
            component_name: String::new(),
            title: format!("incident {id}"),
            started_at: at(minute),
            ended_at: Some(at(minute + 5)),
            severity: IncidentSeverity::Major,
            impact: IncidentImpact::MajorOutage,
            status_phase: IncidentStatusPhase::Resolved,
            updates: Vec::new(),
            postmortem: None,
            recovered: Vec::new(),
            paused: Default::default(),
            recovering: false,
        }
    }

    fn window(minute: i64, id: u128) -> PublicMaintenance {
        PublicMaintenance {
            updates: Vec::new(),
            id: Uuid::from_u128(id),
            title: format!("window {id}"),
            description: None,
            starts_at: at(minute),
            ends_at: at(minute + 60),
            affected_component_names: Vec::new(),
        }
    }

    fn ids(page: &HistoryPage) -> Vec<u128> {
        page.entries
            .iter()
            .map(|e| e.position().id.as_u128())
            .collect()
    }

    #[test]
    fn both_kinds_interleave_newest_first() {
        let page = merge(
            CursorPage::new(vec![incident(30, 1), incident(10, 2)], None),
            CursorPage::new(vec![window(20, 3), window(0, 4)], None),
            10,
        );
        assert_eq!(ids(&page), [1, 3, 2, 4]);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn the_cut_continues_from_the_last_row_kept() {
        let page = merge(
            CursorPage::new(vec![incident(30, 1), incident(10, 2)], None),
            CursorPage::new(vec![window(20, 3), window(0, 4)], None),
            2,
        );
        assert_eq!(ids(&page), [1, 3]);
        let next = HistoryCursor::decode(page.next_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(next.id, Uuid::from_u128(3));
        assert_eq!(next.started_at, at(20));
    }

    #[test]
    fn more_on_one_side_keeps_a_cursor_though_nothing_was_cut() {
        let page = merge(
            CursorPage::new(vec![incident(30, 1)], Some("more".into())),
            CursorPage::new(Vec::new(), None),
            1,
        );
        assert_eq!(ids(&page), [1]);
        assert!(page.next_cursor.is_some());
    }

    #[test]
    fn a_shared_start_breaks_the_tie_by_id() {
        let page = merge(
            CursorPage::new(vec![incident(10, 1)], None),
            CursorPage::new(vec![window(10, 2)], None),
            10,
        );
        assert_eq!(ids(&page), [2, 1]);
    }
}
