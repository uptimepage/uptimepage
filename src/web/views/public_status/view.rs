//! The rendered view model: what the templates read, built from the
//! aggregator's public snapshot.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::domain::elapsed_at;
use crate::domain::{
    DayState, IncidentImpact, IncidentStatusPhase, OverallState, PublicComponent,
    PublicComponentGroup, PublicComponentStatus, PublicIncident, PublicIncidentUpdate,
    PublicMaintenance, PublicStatusPage,
};
use crate::public_status::HistoryIncidentMarker;
use crate::templates::format::humanize_duration;

pub(super) const RSS_URL: &str = "/api/public/v1/incidents.rss";
pub(super) const HISTORY_LEN: usize = 90;

pub struct StatusView {
    pub site_name: String,
    pub site_title: String,
    pub overall_label: String,
    pub overall_class: &'static str,
    pub overall_icon: &'static str,
    pub overall_aria: &'static str,
    pub generated_at: DateTime<Utc>,
    pub groups: Vec<GroupView>,
    pub active_incidents: Vec<IncidentSummary>,
    pub recent_incidents: Vec<IncidentSummary>,
    /// True when the org has more incidents past the rendered window. Drives
    /// the "older incidents" archive link in the recent-incidents section.
    pub recent_incidents_has_more: bool,
    pub active_maintenance: Vec<MaintenanceView>,
    pub upcoming_maintenance: Vec<MaintenanceView>,
    pub has_active_incident: bool,
    pub has_maintenance: bool,
    pub has_components: bool,
    pub rss_url: &'static str,
    /// Inlined at `#day-strip-data`; consumed by day_popover.js.
    pub day_strip_json: String,
}

pub struct GroupView {
    pub heading: String,
    pub components: Vec<ComponentView>,
}

pub struct ComponentView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub status_label: &'static str,
    pub status_class: &'static str,
    pub status_icon: &'static str,
    pub history: Vec<DayCell>,
    /// `"99.97%"`, or `"—"` until the component has been probed.
    pub uptime_label: String,
    pub history_summary: String,
    pub detail_url: Option<String>,
}

pub struct DayCell {
    pub class: &'static str,
    pub aria_label: String,
    /// Index into the day_strip_json blob for this component.
    pub day_index: usize,
}

#[derive(serde::Serialize)]
pub(super) struct DayStripComponent {
    name: String,
    days: Vec<DayPopoverEntry>,
}

#[derive(serde::Serialize)]
pub(super) struct DayPopoverEntry {
    date: String,
    state: &'static str,
    state_class: &'static str,
    show_badge: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    downtime: Option<String>,
    related: Vec<DayRelated>,
}

#[derive(serde::Serialize)]
pub(super) struct DayRelated {
    pub(super) title: String,
    pub(super) url: String,
}

/// Common header fields shared by the recent-incidents list and the detail page.
pub struct IncidentHeader {
    pub id: String,
    pub component_name: String,
    pub title: String,
    pub impact_label: &'static str,
    pub impact_class: &'static str,
    pub phase_label: &'static str,
    pub phase_class: &'static str,
    pub started_at: DateTime<Utc>,
    pub ongoing: bool,
    /// Elapsed seconds at page-build `now`. Populated for both closed and
    /// ongoing incidents — the public view shows a duration for all rows.
    pub duration_secs: i64,
}

pub struct IncidentSummary {
    pub header: IncidentHeader,
    pub ended_at: Option<DateTime<Utc>>,
    pub latest_message: Option<String>,
    pub permalink: String,
}

pub struct IncidentDetailView {
    pub header: IncidentHeader,
    pub ended_at: Option<DateTime<Utc>>,
    pub updates: Vec<IncidentUpdateView>,
    pub postmortem: Option<crate::domain::PublicPostmortem>,
}

pub struct IncidentUpdateView {
    pub posted_at: DateTime<Utc>,
    pub phase_label: &'static str,
    pub phase_class: &'static str,
    pub message: String,
}

pub struct MaintenanceView {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub affects: String,
    /// Seconds until `starts_at` for upcoming windows; `None` once started.
    pub starts_in_secs: Option<i64>,
}
pub(super) fn build_view(
    page: &PublicStatusPage,
    history_markers: &[HistoryIncidentMarker],
    silenced: &std::collections::HashSet<Uuid>,
) -> StatusView {
    let now = page.generated_at;

    let groups = page
        .groups
        .iter()
        .map(|g| build_group(g, silenced))
        .collect::<Vec<_>>();
    let has_components = groups.iter().any(|g| !g.components.is_empty());

    let day_strip_json = build_day_strip_json(page, history_markers, now);

    let active = page
        .active_incidents
        .iter()
        .map(|i| build_incident_summary(i, now))
        .collect::<Vec<_>>();
    let recent = page
        .recent_incidents
        .iter()
        .map(|i| build_incident_summary(i, now))
        .collect::<Vec<_>>();

    let active_m = page
        .active_maintenance
        .iter()
        .map(|m| build_maintenance(m, now))
        .collect::<Vec<_>>();
    let upcoming_m = page
        .upcoming_maintenance
        .iter()
        .map(|m| build_maintenance(m, now))
        .collect::<Vec<_>>();

    let (overall_class, overall_icon, overall_aria) = overall_classes(page.overall.state);
    let site_title = crate::public_status::status_title(&page.site_name);

    StatusView {
        site_name: page.site_name.clone(),
        site_title,
        overall_label: page.overall.label.clone(),
        overall_class,
        overall_icon,
        overall_aria,
        generated_at: now,
        groups,
        has_active_incident: !active.is_empty(),
        active_incidents: active,
        recent_incidents: recent,
        recent_incidents_has_more: page.recent_incidents_has_more,
        has_maintenance: !active_m.is_empty() || !upcoming_m.is_empty(),
        active_maintenance: active_m,
        upcoming_maintenance: upcoming_m,
        has_components,
        rss_url: RSS_URL,
        day_strip_json,
    }
}

pub(super) fn build_group(
    g: &PublicComponentGroup,
    silenced: &std::collections::HashSet<Uuid>,
) -> GroupView {
    GroupView {
        heading: g.name.clone().unwrap_or_else(|| "Other".to_string()),
        components: g
            .components
            .iter()
            .map(|c| build_component(c, silenced))
            .collect(),
    }
}

pub(super) fn build_component(
    c: &PublicComponent,
    silenced: &std::collections::HashSet<Uuid>,
) -> ComponentView {
    // No live probe overrides the rolled-up status with a grey "no data" badge,
    // consistent with the history strip's NoData days.
    let (status_label, status_class, status_icon) = if silenced.contains(&c.id) {
        ("No data", "public-cmp--none", "\u{25CB}")
    } else {
        component_classes(c.current_status)
    };
    let history = build_history(c, &c.history);
    let uptime_label = c
        .uptime_pct
        .map(|pct| format!("{pct:.2}%"))
        .unwrap_or_else(|| "—".to_string());
    ComponentView {
        id: c.id.to_string(),
        name: c.name.clone(),
        description: c.description.clone().filter(|s| !s.is_empty()),
        status_label,
        status_class,
        status_icon,
        history,
        uptime_label,
        history_summary: history_summary(&c.history),
        detail_url: c.detail_url.clone(),
    }
}

pub(super) fn build_history(component: &PublicComponent, states: &[DayState]) -> Vec<DayCell> {
    let total = states.len().max(1);
    states
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let (class, label, _tint) = day_classes(*s);
            let day_index = idx;
            let days_ago = total - 1 - idx;
            DayCell {
                class,
                day_index,
                aria_label: format!("{} ({} days ago) — {}", component.name, days_ago, label),
            }
        })
        .collect()
}

/// Build the inline popover blob. UTC day boundary matches the
/// aggregator's `toStartOfDay`, so cell colour and popover state align.
pub(super) fn build_day_strip_json(
    page: &PublicStatusPage,
    history_markers: &[HistoryIncidentMarker],
    now: DateTime<Utc>,
) -> String {
    let today_start = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|nd| DateTime::<Utc>::from_naive_utc_and_offset(nd, Utc))
        .unwrap_or(now);
    // Bucket markers once so the day loop scans only the component's own
    // incidents instead of every org-wide marker × every day.
    let mut by_comp: std::collections::HashMap<Uuid, Vec<&HistoryIncidentMarker>> =
        std::collections::HashMap::new();
    for m in history_markers {
        by_comp.entry(m.component_id).or_default().push(m);
    }
    let empty: Vec<&HistoryIncidentMarker> = Vec::new();
    let mut blob: std::collections::BTreeMap<String, DayStripComponent> =
        std::collections::BTreeMap::new();
    for group in &page.groups {
        for c in &group.components {
            let total = c.history.len().max(1);
            let markers = by_comp.get(&c.id).unwrap_or(&empty);
            let days = c
                .history
                .iter()
                .enumerate()
                .map(|(idx, s)| {
                    let days_ago = total - 1 - idx;
                    let day_start = today_start - ChronoDuration::days(days_ago as i64);
                    let day_end = day_start + ChronoDuration::days(1);
                    let (downtime, related) = day_overlap(markers, day_start, day_end, now);
                    let (_class, state, state_class) = day_classes(*s);
                    let show_badge = !matches!(s, DayState::Operational | DayState::NoData)
                        || !related.is_empty();
                    DayPopoverEntry {
                        date: day_start.format("%-d %b %Y").to_string(),
                        state,
                        state_class,
                        show_badge,
                        downtime: (downtime > ChronoDuration::zero())
                            .then(|| humanize_duration(downtime)),
                        related,
                    }
                })
                .collect();
            blob.insert(
                c.id.to_string(),
                DayStripComponent {
                    name: c.name.clone(),
                    days,
                },
            );
        }
    }
    // Escape every `<`, `>`, `&` to JSON `\uXXXX` so a malicious incident
    // title can't terminate the inline <script> with `</script>`, slip into
    // a comment via `<!--`, or close the JSON early via `&`/CDATA tricks.
    // Browsers parse `<` etc. back to the original char on JSON.parse.
    serde_json::to_string(&blob)
        .unwrap_or_else(|_| "{}".to_string())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// Sum incident time overlapping `[day_start, day_end)`. Caller pre-filters
/// to one component's markers. Open-ended incidents clamp to `now`.
pub(super) fn day_overlap(
    incidents: &[&HistoryIncidentMarker],
    day_start: DateTime<Utc>,
    day_end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> (ChronoDuration, Vec<DayRelated>) {
    let mut total = ChronoDuration::zero();
    let mut links = Vec::new();
    for inc in incidents {
        let end = inc.ended_at.unwrap_or(now);
        if inc.started_at >= day_end || end <= day_start {
            continue;
        }
        let overlap_start = inc.started_at.max(day_start);
        let overlap_end = end.min(day_end);
        let overlap = (overlap_end - overlap_start).max(ChronoDuration::zero());
        total += overlap;
        links.push(DayRelated {
            title: inc.title.clone(),
            url: format!("/status/incidents/{}", inc.id),
        });
    }
    (total, links)
}

/// Day tally under the strip: how many days had checks, and how many of those
/// were painted. The uptime figure beside it is time-based and comes from the
/// aggregator, not from these counts.
pub(super) fn history_summary(states: &[DayState]) -> String {
    let mut with_data = 0usize;
    let mut bad = 0usize;
    let mut degraded = 0usize;
    for s in states {
        match s {
            DayState::NoData => {}
            DayState::MajorOutage | DayState::PartialOutage => {
                with_data += 1;
                bad += 1;
            }
            DayState::Degraded => {
                with_data += 1;
                degraded += 1;
            }
            DayState::Operational | DayState::Maintenance => with_data += 1,
        }
    }
    if with_data == 0 {
        format!("{HISTORY_LEN} days, no data")
    } else if bad == 0 && degraded == 0 {
        format!("{with_data} days, no incidents")
    } else if bad == 0 {
        format!("{with_data} days, {degraded} degraded")
    } else {
        format!(
            "{with_data} days, {bad} outage{}, {degraded} degraded",
            if bad == 1 { "" } else { "s" },
        )
    }
}

pub(super) fn build_incident_header(i: &PublicIncident, now: DateTime<Utc>) -> IncidentHeader {
    let ongoing = i.ended_at.is_none();
    let duration_secs = elapsed_at(i.started_at, i.ended_at, now).num_seconds();
    let (impact_label, impact_class) = impact_classes(i.impact);
    let (phase_label, phase_class) = phase_classes(i.status_phase);
    IncidentHeader {
        id: i.id.to_string(),
        component_name: i.component_name.clone(),
        title: i.title.clone(),
        impact_label,
        impact_class,
        phase_label,
        phase_class,
        started_at: i.started_at,
        ongoing,
        duration_secs,
    }
}

pub(super) fn build_incident_summary(i: &PublicIncident, now: DateTime<Utc>) -> IncidentSummary {
    let header = build_incident_header(i, now);
    let latest_message = i.updates.last().map(|u: &PublicIncidentUpdate| {
        if u.message.len() > 240 {
            format!("{}…", &u.message[..240])
        } else {
            u.message.clone()
        }
    });
    IncidentSummary {
        permalink: format!("/status/incidents/{}", header.id),
        header,
        ended_at: i.ended_at,
        latest_message,
    }
}

impl IncidentDetailView {
    pub(super) fn from_incident(i: &PublicIncident, now: DateTime<Utc>) -> Self {
        let header = build_incident_header(i, now);
        let updates = i
            .updates
            .iter()
            .map(|u| {
                let (l, c) = phase_classes(u.phase);
                IncidentUpdateView {
                    posted_at: u.posted_at,
                    phase_label: l,
                    phase_class: c,
                    message: u.message.clone(),
                }
            })
            .collect();
        Self {
            header,
            ended_at: i.ended_at,
            updates,
            postmortem: i.postmortem.clone(),
        }
    }
}

pub(super) fn build_maintenance(m: &PublicMaintenance, now: DateTime<Utc>) -> MaintenanceView {
    let starts_in_secs = (m.starts_at > now).then(|| (m.starts_at - now).num_seconds());
    MaintenanceView {
        id: m.id.to_string(),
        title: m.title.clone(),
        description: m.description.clone().filter(|s| !s.is_empty()),
        starts_at: m.starts_at,
        ends_at: m.ends_at,
        affects: m.affected_component_names.join(", "),
        starts_in_secs,
    }
}

// --- Classifiers ---------------------------------------------------------

// Iconography follows the design system: solid filled circle (U+25CF) for
// go/no-go states (operational, major outage), gear (U+2699) for maintenance,
// warning sign (U+26A0) for degraded / partial outage. No emoji — every glyph
// here is a non-emoji Unicode symbol that inherits text colour via currentColor.
pub(super) fn overall_classes(s: OverallState) -> (&'static str, &'static str, &'static str) {
    match s {
        OverallState::Operational => ("public-overall--op", "\u{25CF}", "All systems operational"),
        OverallState::Maintenance => (
            "public-overall--mnt",
            "\u{2699}\u{FE0E}",
            "Maintenance in progress",
        ),
        OverallState::MinorDisruption => (
            "public-overall--minor",
            "\u{26A0}\u{FE0E}",
            "Minor service disruption",
        ),
        OverallState::PartialOutage => (
            "public-overall--part",
            "\u{26A0}\u{FE0E}",
            "Partial system outage",
        ),
        OverallState::MajorOutage => ("public-overall--maj", "\u{25CF}", "Major system outage"),
    }
}

pub(super) fn component_classes(
    s: PublicComponentStatus,
) -> (&'static str, &'static str, &'static str) {
    match s {
        PublicComponentStatus::Operational => ("Operational", "public-cmp--op", "\u{25CF}"),
        PublicComponentStatus::Degraded => ("Degraded", "public-cmp--deg", "\u{26A0}\u{FE0E}"),
        PublicComponentStatus::PartialOutage => {
            ("Partial outage", "public-cmp--part", "\u{26A0}\u{FE0E}")
        }
        PublicComponentStatus::MajorOutage => ("Major outage", "public-cmp--maj", "\u{25CF}"),
        PublicComponentStatus::Maintenance => {
            ("Maintenance", "public-cmp--mnt", "\u{2699}\u{FE0E}")
        }
        // Hollow, like the day strip's silent cells.
        PublicComponentStatus::NoData => ("No data", "public-cmp--none", "\u{25CB}"),
    }
}

/// (bar fill class, human label, popover badge tint class).
pub(super) fn day_classes(s: DayState) -> (&'static str, &'static str, &'static str) {
    match s {
        DayState::Operational => ("day-cell--op", "Operational", "day-pop-status--op"),
        DayState::Degraded => ("day-cell--deg", "Degraded", "day-pop-status--deg"),
        DayState::PartialOutage => ("day-cell--part", "Partial outage", "day-pop-status--part"),
        DayState::MajorOutage => ("day-cell--maj", "Major outage", "day-pop-status--maj"),
        DayState::Maintenance => ("day-cell--mnt", "Maintenance", "day-pop-status--mnt"),
        DayState::NoData => ("day-cell--none", "No data", "day-pop-status--none"),
    }
}

/// Same words as the day strip's [`day_classes`], so one outage reads the
/// same in the strip popover, the incident card and the detail page.
pub(super) fn impact_classes(i: IncidentImpact) -> (&'static str, &'static str) {
    match i {
        IncidentImpact::Degraded => ("Degraded", "public-chip public-sev--minor"),
        IncidentImpact::PartialOutage => ("Partial outage", "public-chip public-sev--major"),
        IncidentImpact::MajorOutage => ("Major outage", "public-chip public-sev--critical"),
    }
}

pub(super) fn phase_classes(p: IncidentStatusPhase) -> (&'static str, &'static str) {
    match p {
        IncidentStatusPhase::Investigating => {
            ("Investigating", "public-chip public-phase--investigating")
        }
        IncidentStatusPhase::Identified => ("Identified", "public-chip public-phase--identified"),
        IncidentStatusPhase::Monitoring => ("Monitoring", "public-chip public-phase--monitoring"),
        IncidentStatusPhase::Resolved => ("Resolved", "public-chip public-phase--resolved"),
        IncidentStatusPhase::Postmortem => ("Postmortem", "public-chip public-phase--postmortem"),
    }
}
