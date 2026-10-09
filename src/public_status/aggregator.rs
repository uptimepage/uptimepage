//! Aggregator that assembles a public status page payload from PostgreSQL and
//! ClickHouse.
//!
//! The aggregator is intentionally **read-only**, **idempotent**, and
//! **side-effect-free** — it owns no caches and no background tasks. Caching is
//! layered on top in [`super::cache`]; the incident materialisation writer
//! lives in its own module.
//!
//! Everything is scoped to a single page: a page selects its monitors via the
//! `status_page_components` join, and each binding carries the per-page public
//! name / group / order. Incidents, maintenance, and history are filtered to
//! the page's component (target_id) set; component names resolve from the
//! page's curation map, falling back to the monitor's own name.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clickhouse::{Client as ClickhouseClient, Row};
use serde::Deserialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::domain::{
    ComponentHistoryResponse, DayState, Downtime, ImpactSpan, IncidentSeverity,
    IncidentStatusPhase, Locale, OrgId, PublicComponent, PublicComponentGroup,
    PublicComponentStatus, PublicIncident, PublicIncidentUpdate, PublicMaintenance,
    PublicStatusPage, Recovered, StatusPageId, outage_parts, stored_incident_impact,
    uptime_pct_from_downtime,
};
use crate::error::Result;
use crate::i18n::Tr;
use crate::security::Cipher;
use crate::storage::capability_token;
use crate::storage::status_pages::COMPONENT_ORDER;

use super::cache::{HistoryIncidentMarker, PageSettings};
use super::overall_status::{
    IncidentImpact, component_status, day_state, overall_state, overall_status,
};

/// Aggregator-local configuration. Holds only the knobs the aggregator reads
/// directly; the shared `AppConfig` may shadow these later when env overrides
/// are wired through.
#[derive(Debug, Clone)]
pub struct AggregatorConfig {
    pub site_name: String,
    pub history_days: u32,
    pub recent_incidents_days: u32,
    pub max_recent_incidents: u32,
    pub upcoming_maintenance_horizon: ChronoDuration,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        Self {
            site_name: "uptimepage".into(),
            history_days: 90,
            recent_incidents_days: 30,
            max_recent_incidents: 50,
            upcoming_maintenance_horizon: ChronoDuration::days(7),
        }
    }
}

// Day-resolution strip reads the hour rollup (13-month tail), so it spans the
// full `history_days` window independent of the shorter 1m-rollup TTL.
const CH_HISTORY_MV: &str = "check_results_1h";

/// Whether incident `i` shows on a page: through one of its components
/// (`components`, a uuid array), or, with no monitor, through the pages it
/// was published to (`page`).
pub(crate) fn on_page(components: &str, page: &str) -> String {
    format!(
        "(i.target_id = ANY({components}) OR EXISTS (\
             SELECT 1 FROM incident_status_pages isp \
             WHERE isp.incident_id = i.id AND isp.org_id = i.org_id \
               AND isp.status_page_id = {page}))"
    )
}

/// The name `page` gave an incident's monitor before the monitor was deleted;
/// NULL while the monitor exists and for a declared incident.
pub(crate) fn kept_component_name(page: &str) -> String {
    format!(
        "(SELECT isp.component_name FROM incident_status_pages isp \
          WHERE isp.incident_id = i.id AND isp.org_id = i.org_id \
            AND isp.status_page_id = {page}) AS kept_component_name"
    )
}

/// One monitor as it sits on a page: its target id, the resolved public name
/// (per-page override or the monitor's own name), and the page-local grouping.
struct PageComponent {
    id: Uuid,
    name: String,
    description: Option<String>,
    group: Option<String>,
    detail_url: Option<String>,
}

/// Page-scoped aggregator. Carries no ids of its own — every method takes the
/// `(page, org)` pair, so the compiler refuses a call site that forgot which
/// page is being built. `org` rides alongside `page` for tenant-scoped queries.
pub struct OrgAggregator {
    pg: PgPool,
    ch: ClickhouseClient,
    cfg: AggregatorConfig,
    /// `None` = no KEK, which is how the tokens were stored too.
    cipher: Option<Arc<Cipher>>,
}

impl OrgAggregator {
    pub fn new(
        pg: PgPool,
        ch: ClickhouseClient,
        cfg: AggregatorConfig,
        cipher: Option<Arc<Cipher>>,
    ) -> Self {
        Self {
            pg,
            ch,
            cfg,
            cipher,
        }
    }

    /// One atomic snapshot per call: page + 90-day popover markers + the
    /// `target_id → public name` map (cached for the incident read paths).
    pub async fn build(
        &self,
        page: StatusPageId,
        org: OrgId,
    ) -> Result<(
        PublicStatusPage,
        Vec<HistoryIncidentMarker>,
        HashMap<Uuid, String>,
        PageSettings,
    )> {
        let now = Utc::now();
        let (components, settings) = tokio::try_join!(
            self.load_page_components(page, org),
            self.load_settings(page, org),
        )?;
        let tr = Tr::new(settings.locale);
        let component_ids: Vec<Uuid> = components.iter().map(|c| c.id).collect();
        let name_by_id: HashMap<Uuid, String> =
            components.iter().map(|c| (c.id, c.name.clone())).collect();

        let days = self.cfg.history_days;
        let (
            (active_maintenance, upcoming_maintenance, maintenance_by_target),
            active_incidents,
            (recent_incidents, recent_incidents_has_more),
            marker_windows,
            paint_windows,
            day_presence,
        ) = tokio::try_join!(
            self.load_maintenance(org, now, &component_ids, &name_by_id),
            self.load_active_incidents(page, org, &component_ids, &name_by_id, tr),
            self.load_recent_incidents(page, org, now, &component_ids, &name_by_id, tr),
            self.load_marker_windows(org, now, &component_ids),
            self.load_paint_windows(org, now, &component_ids, days),
            self.load_day_presence(org, &component_ids, now, days),
        )?;

        let history_markers: Vec<HistoryIncidentMarker> = marker_windows
            .into_iter()
            .map(|w| {
                let component_name = name_by_id.get(&w.target_id).cloned().unwrap_or_default();
                HistoryIncidentMarker {
                    id: w.id,
                    component_id: w.target_id,
                    title: truncate_title(w.public_title.unwrap_or_else(|| {
                        tr.auto_incident_title(&component_name, &w.status_at_start)
                    })),
                    started_at: w.started_at,
                    ended_at: w.ended_at,
                }
            })
            .collect();

        let mut open_worst: HashMap<Uuid, IncidentImpact> = HashMap::new();
        for w in paint_windows.iter().filter(|w| w.ended_at.is_none()) {
            let impact = w.impact();
            open_worst
                .entry(w.target_id)
                .and_modify(|cur| *cur = (*cur).max(impact))
                .or_insert(impact);
        }

        let mut strip_by_target =
            paint_strips(&component_ids, &day_presence, &paint_windows, now, days);
        let mut uptime_by_target =
            component_uptime(&component_ids, &day_presence, &paint_windows, now, days);

        let mut groups: Vec<PublicComponentGroup> = Vec::new();
        for c in &components {
            let maint = maintenance_by_target.contains(&c.id);
            let DayStrip {
                states: history,
                downtime,
            } = strip_by_target
                .remove(&c.id)
                .unwrap_or_else(|| DayStrip::silent(days as usize));
            // Read the evidence off the strip the page is about to render,
            // so the pill and the history under it cannot disagree.
            let has_evidence = history.iter().any(|d| *d != DayState::NoData);
            let current = component_status(open_worst.get(&c.id).copied(), maint, has_evidence);
            let pc = PublicComponent {
                id: c.id,
                name: c.name.clone(),
                description: c.description.clone(),
                current_status: current,
                history,
                downtime,
                uptime_pct: uptime_by_target.remove(&c.id).flatten(),
                detail_url: c.detail_url.clone(),
            };
            match groups.last_mut() {
                Some(g) if g.name == c.group => g.components.push(pc),
                _ => groups.push(PublicComponentGroup {
                    name: c.group.clone(),
                    components: vec![pc],
                }),
            }
        }

        // An incident with no monitor has no component to colour, so its
        // impact reaches the headline directly.
        let component_statuses: Vec<PublicComponentStatus> = groups
            .iter()
            .flat_map(|g| g.components.iter().map(|c| c.current_status))
            .chain(
                active_incidents
                    .iter()
                    .filter(|i| i.component_id.is_none())
                    .map(|i| component_status(Some(i.impact), false, true)),
            )
            .collect();
        let overall = overall_status(overall_state(&component_statuses));

        Ok((
            PublicStatusPage {
                overall,
                generated_at: now,
                site_name: self.cfg.site_name.clone(),
                groups,
                active_incidents,
                recent_incidents,
                recent_incidents_has_more,
                active_maintenance,
                upcoming_maintenance,
            },
            history_markers,
            name_by_id,
            settings,
        ))
    }

    /// Per-component history endpoint (`GET /api/public/v1/components/{id}/history`).
    /// 404s (via the error) when the target isn't on this page.
    pub async fn component_history(
        &self,
        page: StatusPageId,
        org: OrgId,
        id: Uuid,
        days: u32,
    ) -> Result<ComponentHistoryResponse> {
        let now = Utc::now();
        let components = self.load_page_components(page, org).await?;
        let component = components
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| anyhow::anyhow!("component not on this page"))?;

        // Fetch only the requested span — this endpoint is uncached.
        let span = days.clamp(1, self.cfg.history_days);
        let ids = [id];
        let (paint_windows, day_presence) = tokio::try_join!(
            self.load_paint_windows(org, now, &ids, span),
            self.load_day_presence(org, &ids, now, span),
        )?;
        let history = paint_strips(&ids, &day_presence, &paint_windows, now, span)
            .remove(&id)
            .unwrap_or_else(|| DayStrip::silent(span as usize))
            .states;
        let history = pad_or_truncate(history, days as usize);

        Ok(ComponentHistoryResponse {
            component_id: id,
            component_name: component.name,
            days,
            history,
        })
    }

    // ── private helpers ─────────────────────────────────────────────────────

    /// A missing row reads as hidden, so a fault publishes nothing.
    async fn load_settings(&self, page: StatusPageId, org: OrgId) -> Result<PageSettings> {
        let row: Option<(bool, String)> = sqlx::query_as(
            "SELECT public_hide_from_search, public_locale \
             FROM status_pages WHERE id = $1 AND org_id = $2",
        )
        .bind(page.0)
        .bind(org.0)
        .fetch_optional(&self.pg)
        .await
        .context("load page settings")?;
        Ok(match row {
            Some((hide_from_search, locale)) => PageSettings {
                hide_from_search,
                locale: Locale::from_db(&locale),
            },
            None => PageSettings {
                hide_from_search: true,
                locale: Locale::default(),
            },
        })
    }

    /// The page's monitors, with per-page curation applied, in render order.
    async fn load_page_components(
        &self,
        page: StatusPageId,
        org: OrgId,
    ) -> Result<Vec<PageComponent>> {
        let rows: Vec<PageComponentRow> = sqlx::query_as::<_, PageComponentRow>(&format!(
            r#"SELECT spc.target_id, t.name AS monitor_name,
                      spc.public_name, spc.public_description,
                      spc.public_group,
                      CASE WHEN spc.detail_link_enabled THEN ms.token_enc END AS share_token_enc
               FROM status_page_components spc
               JOIN targets t ON t.id = spc.target_id AND t.org_id = spc.org_id
               LEFT JOIN monitor_shares ms
                      ON ms.id = spc.share_id
                     AND ms.revoked_at IS NULL
                     AND (ms.expires_at IS NULL OR ms.expires_at > now())
               WHERE spc.status_page_id = $1 AND spc.org_id = $2
                 AND t.plan_hold_at IS NULL
               ORDER BY {COMPONENT_ORDER}, t.name"#
        ))
        .bind(page.0)
        .bind(org.0)
        .fetch_all(&self.pg)
        .await
        .context("load page components")?;
        Ok(rows
            .into_iter()
            .map(|r| PageComponent {
                id: r.target_id,
                name: r.public_name.unwrap_or(r.monitor_name),
                description: r.public_description,
                group: r.public_group,
                detail_url: r
                    .share_token_enc
                    .as_deref()
                    .and_then(|sealed| capability_token::open(sealed, self.cipher.as_deref()))
                    .map(|token| format!("/m/{token}")),
            })
            .collect())
    }

    async fn load_maintenance(
        &self,
        org: OrgId,
        now: DateTime<Utc>,
        component_ids: &[Uuid],
        name_by_id: &HashMap<Uuid, String>,
    ) -> Result<(Vec<PublicMaintenance>, Vec<PublicMaintenance>, Vec<Uuid>)> {
        let horizon_end = now + self.cfg.upcoming_maintenance_horizon;
        let rows: Vec<MaintenanceRow> = sqlx::query_as::<_, MaintenanceRow>(
            r#"SELECT mw.id, mw.title, mw.description, mw.starts_at, mw.ends_at,
                      COALESCE(
                        ARRAY_AGG(mwc.target_id) FILTER (WHERE mwc.target_id IS NOT NULL),
                        '{}'::uuid[]
                      ) AS component_ids
               FROM maintenance_windows mw
               LEFT JOIN maintenance_window_components mwc ON mwc.maintenance_id = mw.id
               WHERE mw.org_id = $3
                 AND mw.deleted_at IS NULL
                 AND mw.ends_at > $1
                 AND mw.starts_at < $2
               GROUP BY mw.id
               ORDER BY mw.starts_at ASC"#,
        )
        .bind(now)
        .bind(horizon_end)
        .bind(org.0)
        .fetch_all(&self.pg)
        .await
        .context("load maintenance windows")?;

        let on_page: std::collections::HashSet<Uuid> = component_ids.iter().copied().collect();
        let mut active = Vec::new();
        let mut upcoming = Vec::new();
        let mut active_target_ids: Vec<Uuid> = Vec::new();
        for row in rows {
            // Only surface windows that touch at least one of THIS page's components.
            let on_page_components: Vec<Uuid> = row
                .component_ids
                .iter()
                .copied()
                .filter(|id| on_page.contains(id))
                .collect();
            if on_page_components.is_empty() {
                continue;
            }
            let names: Vec<String> = on_page_components
                .iter()
                .filter_map(|id| name_by_id.get(id).cloned())
                .collect();
            let pm = PublicMaintenance {
                id: row.id,
                title: row.title,
                description: row.description,
                starts_at: row.starts_at,
                ends_at: row.ends_at,
                affected_component_names: names,
            };
            if row.starts_at <= now && row.ends_at > now {
                active_target_ids.extend(on_page_components);
                active.push(pm);
            } else if row.starts_at > now {
                upcoming.push(pm);
            }
        }
        Ok((active, upcoming, active_target_ids))
    }

    async fn load_active_incidents(
        &self,
        page: StatusPageId,
        org: OrgId,
        component_ids: &[Uuid],
        name_by_id: &HashMap<Uuid, String>,
        tr: Tr,
    ) -> Result<Vec<PublicIncident>> {
        let rows: Vec<IncidentRow> = sqlx::query_as::<_, IncidentRow>(&format!(
            r#"SELECT i.id, i.target_id,
                      i.started_at, i.ended_at, i.severity, i.status_at_start,
                      i.origin, i.regions_up, i.recovered_from, i.recovered_until,
                      i.recovering_since,
                      i.public_title, i.public_description, {kept}
               FROM incidents i
               WHERE i.org_id = $1
                 AND i.ended_at IS NULL
                 AND {on_page}
                 AND i.visibility = 'public'
               ORDER BY i.started_at DESC"#,
            on_page = on_page("$2", "$3"),
            kept = kept_component_name("$3"),
        ))
        .bind(org.0)
        .bind(component_ids)
        .bind(page.0)
        .fetch_all(&self.pg)
        .await
        .context("load active incidents")?;
        self.hydrate_incidents(org, rows, name_by_id, tr).await
    }

    async fn load_recent_incidents(
        &self,
        page: StatusPageId,
        org: OrgId,
        now: DateTime<Utc>,
        component_ids: &[Uuid],
        name_by_id: &HashMap<Uuid, String>,
        tr: Tr,
    ) -> Result<(Vec<PublicIncident>, bool)> {
        let since = now - ChronoDuration::days(self.cfg.recent_incidents_days as i64);
        let peek_limit = self.cfg.max_recent_incidents as i64 + 1;
        let mut rows: Vec<IncidentRow> = sqlx::query_as::<_, IncidentRow>(&format!(
            r#"SELECT i.id, i.target_id,
                      i.started_at, i.ended_at, i.severity, i.status_at_start,
                      i.origin, i.regions_up, i.recovered_from, i.recovered_until,
                      i.recovering_since,
                      i.public_title, i.public_description, {kept}
               FROM incidents i
               WHERE i.org_id = $3
                 AND i.started_at >= $1
                 AND {on_page}
                 AND i.visibility = 'public'
               ORDER BY i.started_at DESC, i.id DESC
               LIMIT $2"#,
            on_page = on_page("$4", "$5"),
            kept = kept_component_name("$5"),
        ))
        .bind(since)
        .bind(peek_limit)
        .bind(org.0)
        .bind(component_ids)
        .bind(page.0)
        .fetch_all(&self.pg)
        .await
        .context("load recent incidents")?;
        let has_more = rows.len() as u32 > self.cfg.max_recent_incidents;
        if has_more {
            rows.truncate(self.cfg.max_recent_incidents as usize);
        }
        let hydrated = self.hydrate_incidents(org, rows, name_by_id, tr).await?;
        Ok((hydrated, has_more))
    }

    /// 90-day slim incident pool for the popover matcher. 1000-row cap guards
    /// against an incident-spam tenant blowing the rendered JSON. Ends one
    /// waiting out its recovery where the strip ends it.
    async fn load_marker_windows(
        &self,
        org: OrgId,
        now: DateTime<Utc>,
        component_ids: &[Uuid],
    ) -> Result<Vec<MarkerWindowRow>> {
        if component_ids.is_empty() {
            return Ok(Vec::new());
        }
        let since = now - ChronoDuration::days(self.cfg.history_days as i64);
        let rows = sqlx::query_as::<_, MarkerWindowRow>(
            r#"SELECT i.id, i.target_id,
                      i.public_title, i.status_at_start,
                      i.started_at, COALESCE(i.ended_at, i.recovering_since) AS ended_at
               FROM incidents i
               WHERE i.org_id = $2
                 AND (i.ended_at IS NULL OR i.ended_at >= $1)
                 AND i.target_id = ANY($3)
                 AND i.visibility = 'public'
               ORDER BY i.started_at DESC
               LIMIT 1000"#,
        )
        .bind(since)
        .bind(org.0)
        .bind(component_ids)
        .fetch_all(&self.pg)
        .await
        .context("load marker windows")?;
        Ok(rows)
    }

    /// Uncapped confirmed incident windows feeding the open-incident component
    /// states and the day-strip paint. Auto incidents carry the writer's
    /// quorum-gated downtime, so they paint regardless of the visibility stamp;
    /// manual incidents paint only once published. Visibility still gates the
    /// curated cards, recent list, and popover markers. Paint must be complete
    /// or the strip renders green over a real outage; rows are slim (no titles)
    /// and bounded by the span window. An incident waiting out its recovery
    /// ends here where the recovery began: the component is back up. One that
    /// was back up for a while before failing again is split around it.
    async fn load_paint_windows(
        &self,
        org: OrgId,
        now: DateTime<Utc>,
        component_ids: &[Uuid],
        span_days: u32,
    ) -> Result<Vec<PaintWindowRow>> {
        if component_ids.is_empty() {
            return Ok(Vec::new());
        }
        let since = now - ChronoDuration::days(span_days as i64);
        let rows = sqlx::query_as::<_, PaintWindowRow>(
            r#"SELECT i.target_id,
                      i.status_at_start, i.severity, i.origin, i.regions_up,
                      i.started_at, COALESCE(i.ended_at, i.recovering_since) AS ended_at,
                      i.recovered_from, i.recovered_until
               FROM incidents i
               WHERE i.org_id = $2
                 AND (i.ended_at IS NULL OR i.ended_at >= $1)
                 AND i.target_id = ANY($3)
                 AND (i.origin = 'monitor' OR i.visibility = 'public')
                 -- A published declaration kept out of uptime is a notice, not an outage.
                 AND i.counts_as_downtime"#,
        )
        .bind(since)
        .bind(org.0)
        .bind(component_ids)
        .fetch_all(&self.pg)
        .await
        .context("load paint windows")?;
        Ok(rows.into_iter().flat_map(PaintWindowRow::split).collect())
    }

    async fn hydrate_incidents(
        &self,
        org: OrgId,
        rows: Vec<IncidentRow>,
        name_by_id: &HashMap<Uuid, String>,
        tr: Tr,
    ) -> Result<Vec<PublicIncident>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let updates: Vec<IncidentUpdateRow> = sqlx::query_as::<_, IncidentUpdateRow>(
            r#"SELECT incident_id, posted_at, phase, message, generated
               FROM incident_updates
               WHERE incident_id = ANY($1) AND org_id = $2
               ORDER BY incident_id, posted_at ASC"#,
        )
        .bind(&ids)
        .bind(org.0)
        .fetch_all(&self.pg)
        .await
        .context("load incident updates")?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let component_name = r
                    .target_id
                    .and_then(|t| name_by_id.get(&t).cloned())
                    .or_else(|| r.kept_component_name.clone())
                    .unwrap_or_default();
                let my_updates: Vec<PublicIncidentUpdate> = updates
                    .iter()
                    .filter(|u| u.incident_id == r.id)
                    .map(|u| PublicIncidentUpdate {
                        posted_at: u.posted_at,
                        phase: IncidentStatusPhase::from_db_str(&u.phase),
                        message: u.message.clone(),
                        generated: u.generated,
                    })
                    .collect();
                let status_phase = my_updates
                    .last()
                    .map(|u| u.phase)
                    .unwrap_or(IncidentStatusPhase::Investigating);
                let title = r
                    .public_title
                    .clone()
                    .unwrap_or_else(|| tr.auto_incident_title(&component_name, &r.status_at_start));
                let severity = IncidentSeverity::from_db_str(&r.severity);
                let recovering = r.ended_at.is_none() && r.recovering_since.is_some();
                PublicIncident {
                    id: r.id,
                    component_id: r.target_id,
                    component_name,
                    title,
                    started_at: r.started_at,
                    ended_at: r.ended_at,
                    severity,
                    impact: stored_incident_impact(
                        &r.origin,
                        severity,
                        &r.status_at_start,
                        r.regions_up.as_deref(),
                    ),
                    status_phase: status_phase.shown(recovering),
                    updates: my_updates,
                    recovered: Recovered::paired(r.recovered_from, r.recovered_until),
                    recovering,
                    postmortem: None,
                }
            })
            .collect())
    }

    /// Per-(component, day) check counts from the hour rollup — the `NoData`
    /// signal for the day strip.
    async fn load_day_presence(
        &self,
        org: OrgId,
        component_ids: &[Uuid],
        now: DateTime<Utc>,
        span_days: u32,
    ) -> Result<Vec<HistoryDayRow>> {
        if component_ids.is_empty() {
            return Ok(Vec::new());
        }
        let from = now - ChronoDuration::days(span_days as i64);
        self.ch
            .query(&format!(
                r#"SELECT
                    target_id,
                    toInt64(toUnixTimestamp(toStartOfDay(hour))) AS day,
                    countMerge(total_checks) AS day_total,
                    toInt64(toUnixTimestamp(min(hour))) AS first_hour
                FROM {CH_HISTORY_MV}
                WHERE org_id = ?
                  AND has(arrayMap(x -> toUUID(x), ?), target_id)
                  AND hour >= fromUnixTimestamp(?)
                  AND hour < fromUnixTimestamp(?)
                GROUP BY target_id, day
                ORDER BY target_id, day"#
            ))
            .bind(org.0)
            .bind(component_ids)
            .bind(from.timestamp())
            .bind(now.timestamp())
            .fetch_all::<HistoryDayRow>()
            .await
            .context("ch day presence")
            .map_err(Into::into)
    }
}

/// One component's day strip, oldest first: the state each day reached and the
/// time it spent in each state, slot for slot.
struct DayStrip {
    states: Vec<DayState>,
    downtime: Vec<Downtime>,
}

impl DayStrip {
    fn silent(days: usize) -> Self {
        Self {
            states: vec![DayState::NoData; days],
            downtime: vec![Downtime::default(); days],
        }
    }
}

/// Day-strip cells: data presence from the hour rollup (`NoData` detection),
/// outage paint from confirmed incident windows — a raw check blip that never
/// confirmed into an incident leaves the day green. Returns one strip of
/// `span_days` slots per component.
fn paint_strips(
    component_ids: &[Uuid],
    presence: &[HistoryDayRow],
    windows: &[PaintWindowRow],
    now: DateTime<Utc>,
    span_days: u32,
) -> HashMap<Uuid, DayStrip> {
    let days = span_days as usize;
    let from = now - ChronoDuration::days(span_days as i64);

    let mut has_data: HashMap<Uuid, Vec<bool>> = HashMap::new();
    for r in presence.iter().filter(|r| r.day_total > 0) {
        let day = ts_to_datetime(r.day).date_naive();
        if let Some(i) = days_ago_index(day, now, span_days) {
            has_data
                .entry(r.target_id)
                .or_insert_with(|| vec![false; days])[i] = true;
        }
    }

    // Worst confirmed-incident impact per (component, day slot). Half-open on
    // the end timestamp — a window ending exactly at midnight does not touch
    // that day — matching the popover's `day_related` so cell colour and
    // popover content never disagree.
    let mut worst: HashMap<Uuid, Vec<Option<IncidentImpact>>> = HashMap::new();
    let mut spans: HashMap<Uuid, Vec<Vec<ImpactSpan>>> = HashMap::new();
    for w in windows {
        let slots = worst.entry(w.target_id).or_insert_with(|| vec![None; days]);
        let day_spans = spans
            .entry(w.target_id)
            .or_insert_with(|| vec![Vec::new(); days]);
        let impact = w.impact();
        let end = w.ended_at.unwrap_or(now).min(now);
        let mut d = w.started_at.max(from).date_naive();
        while day_start_utc(d) < end {
            if let Some(i) = days_ago_index(d, now, span_days) {
                slots[i] = slots[i].max(Some(impact));
                day_spans[i].push((w.started_at, end, impact));
            }
            match d.succ_opt() {
                Some(next) => d = next,
                None => break,
            }
        }
    }

    let today = now.date_naive();
    component_ids
        .iter()
        .map(|id| {
            let data = has_data.get(id);
            let impacts = worst.get(id);
            let day_spans = spans.get(id);
            let states = (0..days)
                .map(|i| day_state(data.is_some_and(|v| v[i]), impacts.and_then(|v| v[i])))
                .collect();
            let downtime = (0..days)
                .map(|i| {
                    let Some(spans) = day_spans.map(|s| &s[i]).filter(|s| !s.is_empty()) else {
                        return Downtime::default();
                    };
                    let day = today - chrono::Days::new((days - 1 - i) as u64);
                    let start = day_start_utc(day);
                    Downtime::between(spans, start, (start + ChronoDuration::days(1)).min(now))
                })
                .collect();
            (*id, DayStrip { states, downtime })
        })
        .collect()
}

/// Confirmed incident downtime over wall-clock time, from the first hour the
/// component was probed within the span to `now`. `None` when nothing was
/// probed. A day-count ratio would bill a four-minute outage as a tenth of a
/// ten-day history. Weighted as the day strip weighs it, and a declared outage
/// over the same hours as a monitor-opened one is one stretch of downtime,
/// not two.
fn component_uptime(
    component_ids: &[Uuid],
    presence: &[HistoryDayRow],
    windows: &[PaintWindowRow],
    now: DateTime<Utc>,
    span_days: u32,
) -> HashMap<Uuid, Option<f64>> {
    let span_from = now - ChronoDuration::days(span_days as i64);
    let mut first_seen: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
    for r in presence.iter().filter(|r| r.day_total > 0) {
        let at = ts_to_datetime(r.first_hour);
        first_seen
            .entry(r.target_id)
            .and_modify(|cur| *cur = (*cur).min(at))
            .or_insert(at);
    }
    component_ids
        .iter()
        .map(|id| {
            let pct = first_seen.get(id).map(|seen| {
                let from = (*seen).max(span_from);
                let spans: Vec<ImpactSpan> = windows
                    .iter()
                    .filter(|w| w.target_id == *id)
                    .map(|w| (w.started_at, w.ended_at.unwrap_or(now), w.impact()))
                    .collect();
                let downtime_secs = Downtime::between(&spans, from, now).weighted_secs();
                uptime_pct_from_downtime(downtime_secs, (now - from).num_seconds())
            });
            (*id, pct)
        })
        .collect()
}

// ── PG row types ────────────────────────────────────────────────────────────

#[derive(FromRow)]
struct PageComponentRow {
    target_id: Uuid,
    monitor_name: String,
    public_name: Option<String>,
    public_description: Option<String>,
    public_group: Option<String>,
    share_token_enc: Option<String>,
}

#[derive(FromRow)]
struct MaintenanceRow {
    id: Uuid,
    title: String,
    description: Option<String>,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    component_ids: Vec<Uuid>,
}

#[derive(FromRow)]
struct IncidentRow {
    id: Uuid,
    target_id: Option<Uuid>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    severity: String,
    status_at_start: String,
    origin: String,
    regions_up: Option<Vec<String>>,
    recovered_from: Vec<DateTime<Utc>>,
    recovered_until: Vec<DateTime<Utc>>,
    recovering_since: Option<DateTime<Utc>>,
    public_title: Option<String>,
    #[allow(dead_code)]
    public_description: Option<String>,
    kept_component_name: Option<String>,
}

#[derive(FromRow)]
struct MarkerWindowRow {
    id: Uuid,
    target_id: Uuid,
    public_title: Option<String>,
    status_at_start: String,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

#[derive(FromRow, Clone)]
struct PaintWindowRow {
    target_id: Uuid,
    status_at_start: String,
    severity: String,
    origin: String,
    regions_up: Option<Vec<String>>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    recovered_from: Vec<DateTime<Utc>>,
    recovered_until: Vec<DateTime<Utc>>,
}

impl PaintWindowRow {
    /// The stretches of it the component was down, one window each. The
    /// windows carry no stretches of their own: the split has used them.
    fn split(mut self) -> Vec<Self> {
        let recovered = Recovered::paired(
            std::mem::take(&mut self.recovered_from),
            std::mem::take(&mut self.recovered_until),
        );
        outage_parts(self.started_at, self.ended_at, &recovered)
            .into_iter()
            .map(|(started_at, ended_at)| Self {
                started_at,
                ended_at,
                ..self.clone()
            })
            .collect()
    }

    fn impact(&self) -> IncidentImpact {
        stored_incident_impact(
            &self.origin,
            IncidentSeverity::from_db_str(&self.severity),
            &self.status_at_start,
            self.regions_up.as_deref(),
        )
    }
}

#[derive(FromRow)]
struct IncidentUpdateRow {
    incident_id: Uuid,
    posted_at: DateTime<Utc>,
    phase: String,
    message: String,
    generated: bool,
}

// ── CH row types ────────────────────────────────────────────────────────────

#[derive(Row, Deserialize)]
struct HistoryDayRow {
    #[serde(with = "clickhouse::serde::uuid")]
    target_id: Uuid,
    day: i64, // DateTime in seconds; ClickHouse `toStartOfDay` returns DateTime
    day_total: u64,
    /// First hour bucket with checks that day, in seconds.
    first_hour: i64,
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Cap popover title length so a runaway tenant can't blow up the inline strip
/// JSON. Snaps to the last char-boundary at or before the cap.
fn truncate_title(mut s: String) -> String {
    const MAX: usize = 140;
    if s.len() <= MAX {
        return s;
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push('…');
    s
}

fn ts_to_datetime(secs: i64) -> DateTime<Utc> {
    chrono::DateTime::<Utc>::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
}

fn day_start_utc(day: chrono::NaiveDate) -> DateTime<Utc> {
    day.and_hms_opt(0, 0, 0)
        .expect("midnight is valid for every date")
        .and_utc()
}

/// Maps a calendar day to its slot in the `history_days`-long oldest-first
/// strip. Returns `None` when the day falls outside the window.
fn days_ago_index(day: chrono::NaiveDate, now: DateTime<Utc>, history_days: u32) -> Option<usize> {
    let today = now.date_naive();
    let diff = (today - day).num_days();
    if diff < 0 || diff >= history_days as i64 {
        return None;
    }
    // Oldest first: idx 0 = oldest day, idx = history_days - 1 = today.
    Some((history_days as i64 - 1 - diff) as usize)
}

fn pad_or_truncate(mut v: Vec<DayState>, len: usize) -> Vec<DayState> {
    if v.len() == len {
        return v;
    }
    if v.len() > len {
        let extra = v.len() - len;
        v.drain(..extra);
        return v;
    }
    let pad = len - v.len();
    let mut out = Vec::with_capacity(len);
    out.extend(std::iter::repeat_n(DayState::NoData, pad));
    out.extend(v);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn days_ago_index_today_is_last_slot() {
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        assert_eq!(days_ago_index(now.date_naive(), now, 90), Some(89));
    }

    #[test]
    fn days_ago_index_oldest_in_window_is_first_slot() {
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let oldest = (now - ChronoDuration::days(89)).date_naive();
        assert_eq!(days_ago_index(oldest, now, 90), Some(0));
    }

    #[test]
    fn days_ago_index_out_of_window_returns_none() {
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let too_old = (now - ChronoDuration::days(90)).date_naive();
        assert!(days_ago_index(too_old, now, 90).is_none());
        let future = (now + ChronoDuration::days(1)).date_naive();
        assert!(days_ago_index(future, now, 90).is_none());
    }

    #[test]
    fn pad_or_truncate_pads_with_no_data_at_start() {
        let v = vec![DayState::Operational; 3];
        let out = pad_or_truncate(v, 5);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], DayState::NoData);
        assert_eq!(out[4], DayState::Operational);
    }

    #[test]
    fn pad_or_truncate_truncates_from_front() {
        let mut v = vec![DayState::NoData; 5];
        v.push(DayState::Operational);
        v.push(DayState::MajorOutage);
        let out = pad_or_truncate(v, 3);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2], DayState::MajorOutage);
    }

    fn window(
        target_id: Uuid,
        origin: &str,
        severity: &str,
        status: &str,
        regions_up: Option<Vec<String>>,
        started_at: DateTime<Utc>,
        ended_at: Option<DateTime<Utc>>,
    ) -> PaintWindowRow {
        PaintWindowRow {
            target_id,
            status_at_start: status.into(),
            severity: severity.into(),
            origin: origin.into(),
            regions_up,
            started_at,
            ended_at,
            recovered_from: Vec::new(),
            recovered_until: Vec::new(),
        }
    }

    fn presence_row(target_id: Uuid, at: DateTime<Utc>) -> HistoryDayRow {
        HistoryDayRow {
            target_id,
            day: day_start_utc(at.date_naive()).timestamp(),
            day_total: 1,
            first_hour: at.timestamp() / 3600 * 3600,
        }
    }

    #[test]
    fn manual_incident_impact_follows_severity() {
        let t = Uuid::nil();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        for (severity, expected) in [
            ("minor", IncidentImpact::Degraded),
            ("major", IncidentImpact::PartialOutage),
            ("critical", IncidentImpact::MajorOutage),
        ] {
            let w = window(t, "manual", severity, "down", None, now, None);
            assert_eq!(w.impact(), expected, "severity {severity}");
        }
    }

    #[test]
    fn auto_incident_impact_ignores_severity_column() {
        let t = Uuid::nil();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let partial = window(
            t,
            "monitor",
            "major",
            "down",
            Some(vec!["eu".into()]),
            now,
            None,
        );
        assert_eq!(partial.impact(), IncidentImpact::PartialOutage);
        let total = window(t, "monitor", "minor", "down", Some(vec![]), now, None);
        assert_eq!(total.impact(), IncidentImpact::MajorOutage);
        let degraded = window(t, "monitor", "critical", "degraded", None, now, None);
        assert_eq!(degraded.impact(), IncidentImpact::Degraded);
    }

    #[test]
    fn paint_ends_exactly_at_midnight_excludes_that_day() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let midnight = Utc.with_ymd_and_hms(2026, 5, 13, 0, 0, 0).unwrap();
        let w = window(
            t,
            "monitor",
            "major",
            "down",
            None,
            midnight - ChronoDuration::hours(3),
            Some(midnight),
        );
        let presence = [
            presence_row(t, now),
            presence_row(t, midnight - ChronoDuration::hours(3)),
        ];
        let strips = paint_strips(&[t], &presence, &[w], now, 90);
        let strip = &strips[&t].states;
        // Yesterday (the incident's real day) painted, today untouched.
        assert_eq!(strip[88], DayState::MajorOutage);
        assert_eq!(strip[89], DayState::Operational);
    }

    #[test]
    fn each_day_carries_the_time_it_spent_down() {
        // A four-minute total outage before midnight and a two-hour partial
        // one that runs across it: each day gets only its own share.
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let midnight = Utc.with_ymd_and_hms(2026, 5, 13, 0, 0, 0).unwrap();
        let blip = window(
            t,
            "monitor",
            "major",
            "down",
            None,
            midnight - ChronoDuration::hours(5),
            Some(midnight - ChronoDuration::hours(5) + ChronoDuration::minutes(4)),
        );
        let partial = window(
            t,
            "monitor",
            "major",
            "down",
            Some(vec!["eu".into()]),
            midnight - ChronoDuration::minutes(30),
            Some(midnight + ChronoDuration::minutes(90)),
        );
        let strips = paint_strips(&[t], &[], &[blip, partial], now, 90);
        let strip = &strips[&t];
        assert_eq!(strip.downtime[88].major_secs, 240);
        assert_eq!(strip.downtime[88].partial_secs, 30 * 60);
        assert_eq!(strip.downtime[89].partial_secs, 90 * 60);
        assert_eq!(strip.downtime[89].major_secs, 0);
        assert_eq!(strip.downtime[87], Downtime::default());
    }

    #[test]
    fn an_open_incident_counts_until_now() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let open = window(
            t,
            "monitor",
            "major",
            "down",
            None,
            now - ChronoDuration::minutes(25),
            None,
        );
        let strips = paint_strips(&[t], &[], &[open], now, 90);
        assert_eq!(strips[&t].downtime[89].major_secs, 25 * 60);
    }

    #[test]
    fn uptime_is_downtime_over_probed_time_not_a_day_count() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        // Ten days of checks, one four-minute outage.
        let presence: Vec<HistoryDayRow> = (0..10)
            .map(|d| presence_row(t, now - ChronoDuration::days(d)))
            .collect();
        let outage_start = now - ChronoDuration::days(3);
        let w = window(
            t,
            "monitor",
            "major",
            "down",
            None,
            outage_start,
            Some(outage_start + ChronoDuration::minutes(4)),
        );
        let pct = component_uptime(&[t], &presence, &[w], now, 90)[&t].expect("probed");
        assert!((99.96..99.98).contains(&pct), "{pct}");

        let silent = Uuid::now_v7();
        assert_eq!(
            component_uptime(&[silent], &[], &[], now, 90)[&silent],
            None
        );
    }

    #[test]
    fn uptime_merges_a_declared_outage_over_a_measured_one() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let presence = [presence_row(t, now - ChronoDuration::days(1))];
        let start = now - ChronoDuration::hours(12);
        let measured = window(t, "monitor", "major", "down", None, start, Some(now));
        let declared = window(
            t,
            "manual",
            "critical",
            "down",
            None,
            start - ChronoDuration::hours(1),
            Some(now - ChronoDuration::hours(6)),
        );
        let pct =
            component_uptime(&[t], &presence, &[measured, declared], now, 90)[&t].expect("probed");
        // Thirteen hours down out of twenty-four, counted once.
        assert!((45.0..46.0).contains(&pct), "{pct}");
    }

    #[test]
    fn a_stretch_back_up_inside_an_incident_is_neither_down_nor_painted() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let presence = [presence_row(t, now - ChronoDuration::days(1))];
        let mut w = window(
            t,
            "monitor",
            "major",
            "down",
            None,
            now - ChronoDuration::hours(12),
            Some(now - ChronoDuration::hours(6)),
        );
        w.recovered_from = vec![now - ChronoDuration::hours(11)];
        w.recovered_until = vec![now - ChronoDuration::hours(7)];
        let parts = w.split();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| p.recovered_from.is_empty()));
        let pct = component_uptime(&[t], &presence, &parts, now, 90)[&t].expect("probed");
        // Two hours down out of twenty-four, not six.
        assert!((91.6..91.7).contains(&pct), "{pct}");
    }

    #[test]
    fn uptime_weighs_a_partial_outage_and_ignores_degraded_performance() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let presence = [presence_row(t, now - ChronoDuration::days(1))];
        let partial = window(
            t,
            "monitor",
            "major",
            "down",
            Some(vec!["eu-helsinki".into()]),
            now - ChronoDuration::hours(10),
            Some(now),
        );
        let slow = window(
            t,
            "monitor",
            "major",
            "degraded",
            None,
            now - ChronoDuration::hours(20),
            Some(now - ChronoDuration::hours(12)),
        );
        let pct = component_uptime(&[t], &presence, &[partial, slow], now, 90)[&t].expect("probed");
        // Ten hours of partial outage count as three of twenty-four.
        assert!((87.4..87.6).contains(&pct), "{pct}");
    }

    #[test]
    fn uptime_counts_an_open_incident_up_to_now_and_clips_to_the_span() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let presence = [presence_row(t, now - ChronoDuration::days(200))];
        let w = window(
            t,
            "monitor",
            "major",
            "down",
            None,
            now - ChronoDuration::days(200),
            None,
        );
        let pct = component_uptime(&[t], &presence, &[w], now, 90)[&t].expect("probed");
        assert_eq!(pct, 0.0, "down for the whole span");
    }

    #[test]
    fn paint_open_incident_wins_over_missing_checks() {
        let t = Uuid::now_v7();
        let now = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
        let w = window(
            t,
            "monitor",
            "major",
            "down",
            None,
            now - ChronoDuration::hours(1),
            None,
        );
        let strips = paint_strips(&[t], &[], &[w], now, 90);
        let strip = &strips[&t].states;
        assert_eq!(
            strip[89],
            DayState::MajorOutage,
            "incident paints without checks"
        );
        assert_eq!(strip[0], DayState::NoData, "silent day stays NoData");
    }
}
