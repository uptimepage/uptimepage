//! The history listing: past incidents as compact rows, with the ones that
//! read as a single disruption folded into one row a visitor can open, and
//! maintenance windows beside them. Shared by the status page and the archive.

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::domain::{
    IncidentImpact, IncidentStatusPhase, MaintenanceUpdatePhase, PublicIncident, PublicMaintenance,
    outage_parts,
};
use crate::i18n::Tr;
use crate::public_status::history::HistoryEntry;
use fluent_templates::fluent_bundle::FluentValue;

use super::incident_display::{UPDATE_PREVIEW_CHARS, impact_classes, phase_classes};
use crate::public_status::outages::STARTED_TOGETHER;

/// Rows that ended within this long stay in view; older ones fold away
/// behind one toggle.
const RECENT_DAYS: i64 = 7;
/// Repeat incidents on one component closer together than this read as one
/// disruption.
const FOLD_GAP: ChronoDuration = ChronoDuration::hours(1);
/// Component names a row spells out before it counts the rest.
const NAMED_COMPONENTS: usize = 3;
const MAINTENANCE_CHIP: &str = "public-chip public-sev--mnt";
const IN_PROGRESS_CHIP: &str = "public-chip public-phase--monitoring";

/// Which rows a history lists, picked by the `show` query parameter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HistoryTab {
    #[default]
    All,
    Incidents,
    Maintenance,
}

impl HistoryTab {
    const EVERY: [Self; 3] = [Self::All, Self::Incidents, Self::Maintenance];

    /// An unknown value reads as the default rather than as an error page.
    pub fn from_query(raw: Option<&str>) -> Self {
        match raw {
            Some("incidents") => Self::Incidents,
            Some("maintenance") => Self::Maintenance,
            _ => Self::All,
        }
    }

    /// The `show` value that selects it; the default goes unwritten.
    pub fn query(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Incidents => Some("incidents"),
            Self::Maintenance => Some("maintenance"),
        }
    }

    pub fn shows_incidents(self) -> bool {
        self != Self::Maintenance
    }

    pub fn shows_maintenance(self) -> bool {
        self != Self::Incidents
    }

    /// A string whose wording follows the tab.
    pub fn t(self, tr: Tr, id: &str) -> String {
        self.t_args(tr, id, [])
    }

    fn t_args<const N: usize>(
        self,
        tr: Tr,
        id: &str,
        args: [(&'static str, FluentValue<'static>); N],
    ) -> String {
        let selector = self.query().unwrap_or("all");
        tr.t_args(id, std::iter::once(("tab", selector.into())).chain(args))
    }
}

pub struct TabLink {
    pub label: String,
    pub href: String,
    pub current: bool,
}

/// One link per tab to `path`. `anchor` brings a reload back to the listing.
pub fn tab_links(path: &str, anchor: &str, current: HistoryTab, tr: Tr) -> Vec<TabLink> {
    HistoryTab::EVERY
        .into_iter()
        .map(|tab| TabLink {
            label: tab.t(tr, "history-tab"),
            href: match tab.query() {
                Some(q) => format!("{path}?show={q}{anchor}"),
                None => format!("{path}{anchor}"),
            },
            current: tab == current,
        })
        .collect()
}

/// One row of the listing: a single incident, several folded together, or a
/// maintenance window.
pub struct PastEntry {
    /// Stable across refreshes, so a row a visitor opened stays open.
    pub key: String,
    pub title: String,
    /// Set on a single incident. A folded row opens instead of linking, and a
    /// maintenance window has no page of its own.
    pub permalink: Option<String>,
    pub impact_label: String,
    pub impact_class: &'static str,
    /// Shown only when it says more than "resolved", such as a postmortem, or
    /// that a maintenance window is still running.
    pub phase: Option<PhaseChip>,
    /// Still open and down; only the archive lists one, since the status page
    /// shows it under its active incidents. One back up and waiting out its
    /// recovery says so with its Monitoring phase instead.
    pub ongoing: bool,
    /// What leads the meta line: the component, how many incidents, or the
    /// components a window covered.
    pub lead: String,
    /// When it began, which is also what the listing is ordered by.
    pub started_at: DateTime<Utc>,
    /// When the last of it ended; `None` while it is still open.
    pub ended_at: Option<DateTime<Utc>>,
    /// How long it was down, less any stretch it was back up in; for a folded
    /// row the gaps between its incidents are not counted either. For a
    /// window, how long it ran.
    pub duration: String,
    /// The latest update a person wrote, or a window's description, cut to
    /// the preview length.
    pub message: Option<String>,
    /// Whether a window's row opens: its description is longer than the
    /// preview, or updates were posted to it.
    pub opens: bool,
    /// What an open window row shows first: its whole description.
    pub full_message: Option<String>,
    /// What an open window row shows after it: the updates, oldest first.
    pub thread: Vec<PastUpdate>,
    /// The folded incidents, oldest first; empty for any other row.
    pub items: Vec<PastItem>,
}

/// One update posted to a maintenance window.
pub struct PastUpdate {
    pub phase_label: String,
    pub posted_at: DateTime<Utc>,
    pub message: String,
}

impl PastEntry {
    /// How many incidents or windows the row stands for.
    fn count(&self) -> usize {
        self.items.len().max(1)
    }
}

pub struct PhaseChip {
    pub label: String,
    pub class: &'static str,
}

pub struct PastItem {
    pub title: String,
    pub permalink: String,
    pub impact_label: String,
    pub impact_class: &'static str,
    /// Empty when every incident in the row is on the same component.
    pub component_name: String,
    pub started_at: DateTime<Utc>,
    pub duration: String,
}

pub struct PastEvents {
    pub recent: Vec<PastEntry>,
    pub earlier: Vec<PastEntry>,
    /// "12 earlier incidents", "3 earlier entries": what is folded away, in
    /// the words of the tab, counting incidents rather than rows.
    pub earlier_label: String,
    /// Said instead of rows when nothing ended in the last week, or at all.
    pub quiet: Option<String>,
}

/// The status page's listing for `tab`: what ended in the last week in view,
/// the rest folded away. Open incidents are left to the active list above it.
pub(super) fn build_past(
    incidents: &[PublicIncident],
    maintenance: &[PublicMaintenance],
    tab: HistoryTab,
    now: DateTime<Utc>,
    tr: Tr,
) -> PastEvents {
    let cutoff = now - ChronoDuration::days(RECENT_DAYS);
    let closed: Vec<&PublicIncident> = if tab.shows_incidents() {
        incidents.iter().filter(|i| i.ended_at.is_some()).collect()
    } else {
        Vec::new()
    };
    let windows: &[PublicMaintenance] = if tab.shows_maintenance() {
        maintenance
    } else {
        &[]
    };
    let mut rows = fold(&closed, now, tr);
    rows.extend(windows.iter().map(|m| window(m, now, tr)));
    newest_first(&mut rows);
    let (recent, earlier): (Vec<PastEntry>, Vec<PastEntry>) = rows
        .into_iter()
        .partition(|e| e.ended_at.is_none_or(|end| end >= cutoff));
    let earlier_count: usize = earlier.iter().map(PastEntry::count).sum();
    let quiet = match (recent.is_empty(), earlier.is_empty()) {
        (true, true) => Some(tab.t(tr, "history-none")),
        (true, false) => Some(tab.t(tr, "history-none-recent")),
        _ => None,
    };
    PastEvents {
        recent,
        earlier,
        earlier_label: tab.t_args(tr, "history-earlier", [("count", earlier_count.into())]),
        quiet,
    }
}

/// Both kinds as the All tab lists them. Each was read newest first up to its
/// own cap, so past the later of the two stops only one kind is known: the
/// listing ends there for both, and the archive has the rest.
pub(super) fn mixed_within_caps<'a>(
    incidents: &'a [PublicIncident],
    incidents_capped: bool,
    windows: &'a [PublicMaintenance],
    windows_capped: bool,
) -> (&'a [PublicIncident], &'a [PublicMaintenance]) {
    let stop = [
        incidents
            .last()
            .filter(|_| incidents_capped)
            .map(|i| i.started_at),
        windows
            .last()
            .filter(|_| windows_capped)
            .map(|w| w.starts_at),
    ]
    .into_iter()
    .flatten()
    .max();
    let Some(stop) = stop else {
        return (incidents, windows);
    };
    (
        &incidents[..incidents.partition_point(|i| i.started_at >= stop)],
        &windows[..windows.partition_point(|w| w.starts_at >= stop)],
    )
}

/// One month of the archive, folded the same way, newest first. An incident
/// still open and a window still running are listed too.
pub(super) fn build_rows(entries: &[HistoryEntry], now: DateTime<Utc>, tr: Tr) -> Vec<PastEntry> {
    let mut incidents: Vec<&PublicIncident> = Vec::new();
    let mut windows: Vec<&PublicMaintenance> = Vec::new();
    for entry in entries {
        match entry {
            HistoryEntry::Incident(i) => incidents.push(i),
            HistoryEntry::Maintenance(m) => windows.push(m),
        }
    }
    let mut rows = fold(&incidents, now, tr);
    rows.extend(windows.into_iter().map(|m| window(m, now, tr)));
    newest_first(&mut rows);
    rows
}

fn newest_first(rows: &mut [PastEntry]) {
    rows.sort_by(|a, b| (b.started_at, &b.key).cmp(&(a.started_at, &a.key)));
}

/// Incidents grouped into rows by when each row began.
/// Incidents on one component that overlap or are less than [`FOLD_GAP`]
/// apart are one disruption, as are incidents on different components that
/// overlap and began within [`STARTED_TOGETHER`]. One a person has written an
/// update for keeps its own row, so the words are not folded out of sight,
/// and so does one still open.
fn fold(incidents: &[&PublicIncident], now: DateTime<Utc>, tr: Tr) -> Vec<PastEntry> {
    let mut sorted: Vec<&PublicIncident> = incidents.to_vec();
    sorted.sort_by_key(|i| (i.started_at, i.id));
    let stands_alone = |i: &PublicIncident| i.ended_at.is_none() || narrated(i).is_some();

    let mut groups: Vec<Vec<&PublicIncident>> = Vec::new();
    for inc in sorted {
        if stands_alone(inc) {
            groups.push(vec![inc]);
            continue;
        }
        let joining: Vec<usize> = groups
            .iter()
            .enumerate()
            .filter(|(_, g)| !stands_alone(g[0]) && g.iter().any(|o| belong_together(o, inc)))
            .map(|(i, _)| i)
            .collect();
        let mut merged = vec![inc];
        for i in joining.into_iter().rev() {
            merged.extend(groups.remove(i));
        }
        merged.sort_by_key(|i| (i.started_at, i.id));
        groups.push(merged);
    }

    groups
        .into_iter()
        .map(|g| match g.as_slice() {
            [one] => single(one, now, tr),
            _ => folded(&g, now, tr),
        })
        .collect()
}

/// A maintenance window as a row. One that is still running says so. Its
/// text is the latest update, else the description; a row with updates, or a
/// description longer than the preview, opens to the description and every
/// update.
fn window(m: &PublicMaintenance, now: DateTime<Utc>, tr: Tr) -> PastEntry {
    let description = m
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let names: Vec<&str> = m
        .affected_component_names
        .iter()
        .map(String::as_str)
        .collect();
    let running = m.ends_at > now;
    PastEntry {
        key: m.id.to_string(),
        title: m.title.clone(),
        permalink: None,
        impact_label: tr.t("state-maintenance"),
        impact_class: MAINTENANCE_CHIP,
        phase: running.then(|| PhaseChip {
            label: tr.t("maintenance-in-progress"),
            class: IN_PROGRESS_CHIP,
        }),
        ongoing: false,
        lead: name_list(&names, tr),
        started_at: m.starts_at,
        ended_at: (!running).then_some(m.ends_at),
        duration: tr.duration((m.ends_at.min(now) - m.starts_at).num_seconds().max(0)),
        message: m
            .updates
            .last()
            .map(|u| u.message.as_str())
            .or(description)
            .map(|text| crate::text::truncate_chars(text, UPDATE_PREVIEW_CHARS)),
        opens: !m.updates.is_empty()
            || description.is_some_and(|d| d.chars().count() > UPDATE_PREVIEW_CHARS),
        full_message: description.map(str::to_owned),
        thread: m
            .updates
            .iter()
            .map(|u| PastUpdate {
                phase_label: tr.t(update_phase_key(u.phase)),
                posted_at: u.posted_at,
                message: u.message.clone(),
            })
            .collect(),
        items: Vec::new(),
    }
}

pub(super) fn update_phase_key(phase: MaintenanceUpdatePhase) -> &'static str {
    match phase {
        MaintenanceUpdatePhase::Scheduled => "maintenance-upcoming",
        MaintenanceUpdatePhase::InProgress => "maintenance-in-progress",
        MaintenanceUpdatePhase::Completed => "maintenance-completed",
    }
}

/// Names spelled out up to [`NAMED_COMPONENTS`], the rest counted.
fn name_list(names: &[&str], tr: Tr) -> String {
    if names.len() <= NAMED_COMPONENTS {
        return names.join(", ");
    }
    tr.t_args(
        "past-components-more",
        [
            ("names", names[..NAMED_COMPONENTS].join(", ").into()),
            ("count", (names.len() - NAMED_COMPONENTS).into()),
        ],
    )
}

/// Only closed incidents are compared, so both ends are known.
fn belong_together(a: &PublicIncident, b: &PublicIncident) -> bool {
    let (Some(a_end), Some(b_end)) = (a.ended_at, b.ended_at) else {
        return false;
    };
    let overlap = a.started_at < b_end && b.started_at < a_end;
    if a.component_id.is_some() && a.component_id == b.component_id {
        let gap = if a.started_at <= b.started_at {
            b.started_at - a_end
        } else {
            a.started_at - b_end
        };
        return overlap || gap < FOLD_GAP;
    }
    overlap && (a.started_at - b.started_at).abs() < STARTED_TOGETHER
}

/// The latest update a person posted, as the row shows it.
fn narrated(inc: &PublicIncident) -> Option<String> {
    inc.updates
        .iter()
        .rev()
        .find(|u| !u.generated)
        .map(|u| crate::text::truncate_chars(&u.message, UPDATE_PREVIEW_CHARS))
}

fn lasted(inc: &PublicIncident, now: DateTime<Utc>) -> i64 {
    downtime(&[inc], now)
}

fn single(inc: &PublicIncident, now: DateTime<Utc>, tr: Tr) -> PastEntry {
    let (impact_id, impact_class) = impact_classes(inc.impact);
    PastEntry {
        key: inc.id.to_string(),
        title: inc.title.clone(),
        permalink: Some(permalink(inc)),
        impact_label: tr.t(impact_id),
        impact_class,
        phase: telling_phase(inc.status_phase, tr),
        ongoing: inc.ended_at.is_none() && !inc.recovering,
        lead: inc.component_name.clone(),
        started_at: inc.started_at,
        ended_at: inc.ended_at,
        duration: tr.duration(lasted(inc, now)),
        message: narrated(inc),
        opens: false,
        full_message: None,
        thread: Vec::new(),
        items: Vec::new(),
    }
}

fn folded(group: &[&PublicIncident], now: DateTime<Utc>, tr: Tr) -> PastEntry {
    let worst = group
        .iter()
        .map(|i| i.impact)
        .max()
        .unwrap_or(IncidentImpact::Degraded);
    let (impact_id, impact_class) = impact_classes(worst);
    let mut names: Vec<&str> = Vec::new();
    for inc in group {
        if !inc.component_name.is_empty() && !names.contains(&inc.component_name.as_str()) {
            names.push(&inc.component_name);
        }
    }
    let title = if names.is_empty() {
        group[0].title.clone()
    } else {
        name_list(&names, tr)
    };
    let one_component = names.len() <= 1;
    PastEntry {
        key: group[0].id.to_string(),
        title,
        permalink: None,
        impact_label: tr.t(impact_id),
        impact_class,
        phase: None,
        ongoing: false,
        lead: tr.t_args("past-incident-count", [("count", group.len().into())]),
        started_at: group[0].started_at,
        ended_at: group.iter().filter_map(|i| i.ended_at).max(),
        duration: tr.duration(downtime(group, now)),
        message: None,
        opens: false,
        full_message: None,
        thread: Vec::new(),
        items: group
            .iter()
            .map(|inc| {
                let (impact_id, impact_class) = impact_classes(inc.impact);
                PastItem {
                    title: inc.title.clone(),
                    permalink: permalink(inc),
                    impact_label: tr.t(impact_id),
                    impact_class,
                    component_name: if one_component {
                        String::new()
                    } else {
                        inc.component_name.clone()
                    },
                    started_at: inc.started_at,
                    duration: tr.duration(lasted(inc, now)),
                }
            })
            .collect(),
    }
}

/// How long any of them was down, in seconds: overlaps count once, and the
/// healthy gaps between them and the stretches one was back up in not at all.
fn downtime(group: &[&PublicIncident], now: DateTime<Utc>) -> i64 {
    let mut parts: Vec<(DateTime<Utc>, DateTime<Utc>)> = group
        .iter()
        .flat_map(|inc| outage_parts(inc.started_at, inc.ended_at, &inc.recovered))
        .map(|(from, until)| (from, until.unwrap_or(now)))
        .collect();
    parts.sort_unstable();
    let mut total = ChronoDuration::zero();
    let mut reach = DateTime::<Utc>::MIN_UTC;
    for (start, end) in parts {
        let from = start.max(reach);
        if end > from {
            total += end - from;
        }
        reach = reach.max(end);
    }
    total.num_seconds()
}

fn permalink(inc: &PublicIncident) -> String {
    format!("/status/incidents/{}", inc.id)
}

fn telling_phase(p: IncidentStatusPhase, tr: Tr) -> Option<PhaseChip> {
    (p != IncidentStatusPhase::Resolved).then(|| {
        let (id, class) = phase_classes(p);
        PhaseChip {
            label: tr.t(id),
            class,
        }
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::domain::{IncidentSeverity, PublicIncidentUpdate, PublicMaintenanceUpdate};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
    }

    fn at(day: u32, h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, h, m, 0).unwrap()
    }

    fn incident(
        component: Uuid,
        name: &str,
        start: DateTime<Utc>,
        minutes: i64,
        impact: IncidentImpact,
    ) -> PublicIncident {
        let update = |message: &str, generated| PublicIncidentUpdate {
            posted_at: start,
            phase: IncidentStatusPhase::Resolved,
            message: message.into(),
            generated,
        };
        PublicIncident {
            id: Uuid::now_v7(),
            component_id: Some(component),
            component_name: name.into(),
            title: format!("{name} error"),
            started_at: start,
            ended_at: Some(start + ChronoDuration::minutes(minutes)),
            severity: IncidentSeverity::Major,
            impact,
            status_phase: IncidentStatusPhase::Resolved,
            updates: vec![
                update(
                    "Automatically detected — monitoring checks are failing.",
                    true,
                ),
                update(
                    "Automatically resolved — monitoring checks have recovered.",
                    true,
                ),
            ],
            postmortem: None,
            recovered: Vec::new(),
            recovering: false,
        }
    }

    use IncidentImpact::{MajorOutage as Major, PartialOutage as Partial};

    #[test]
    fn a_stretch_an_incident_was_back_up_in_is_not_its_length() {
        let c = Uuid::now_v7();
        let mut held = incident(c, "API", at(5, 10, 0), 30, Major);
        held.recovered = vec![crate::domain::Recovered {
            from: at(5, 10, 5),
            until: at(5, 10, 25),
        }];
        let past = build_past(&[held], &[], HistoryTab::All, now(), Tr::default());
        assert_eq!(past.recent[0].duration, "10m");
    }

    #[test]
    fn a_flapping_service_is_one_row() {
        let webmail = Uuid::now_v7();
        let incidents: Vec<PublicIncident> = [
            (4, 31, 19, Major),
            (4, 57, 24, Major),
            (5, 27, 4, Partial),
            (5, 35, 3, Partial),
            (6, 26, 5, Major),
            (6, 37, 4, Major),
            (6, 47, 3, Partial),
        ]
        .into_iter()
        .map(|(h, m, mins, impact)| incident(webmail, "WEBMAIL", at(4, h, m), mins, impact))
        .rev()
        .collect();
        let past = build_past(&incidents, &[], HistoryTab::All, now(), Tr::default());
        let [row] = past.recent.as_slice() else {
            panic!("one row, got {}", past.recent.len());
        };
        assert_eq!(row.title, "WEBMAIL");
        assert_eq!(row.lead, "7 incidents");
        assert_eq!(row.impact_label, "Major outage", "the worst of them");
        assert_eq!(row.started_at, at(4, 4, 31));
        assert_eq!(
            row.duration, "1h 2m",
            "the gaps between them are not counted"
        );
        assert_eq!(row.items.len(), 7);
        assert_eq!(row.items[0].started_at, at(4, 4, 31), "oldest first inside");
        assert!(row.items.iter().all(|i| i.component_name.is_empty()));
        assert!(row.message.is_none(), "generated updates are boilerplate");
    }

    #[test]
    fn services_failing_together_are_one_row_and_a_later_blip_is_its_own() {
        let names = ["CLUSTER WEB", "ÁREA DE CLIENTE", "PAINEL CLOUD", "NS 1"];
        let mut incidents: Vec<PublicIncident> = names
            .iter()
            .enumerate()
            .map(|(i, n)| incident(Uuid::now_v7(), n, at(6, 13, 21 + i as u32 % 2), 16, Major))
            .collect();
        incidents.push(incident(
            Uuid::now_v7(),
            "NODE 01",
            at(6, 13, 40),
            2,
            Partial,
        ));
        let past = build_past(&incidents, &[], HistoryTab::All, now(), Tr::default());
        assert_eq!(past.recent.len(), 2);
        let (blip, outage) = (&past.recent[0], &past.recent[1]);
        assert!(blip.items.is_empty());
        assert_eq!(blip.title, "NODE 01 error");
        assert_eq!(blip.lead, "NODE 01");
        assert!(blip.permalink.is_some());
        assert_eq!(outage.items.len(), 4);
        assert_eq!(outage.duration, "17m", "overlaps count once");
        assert!(outage.title.ends_with(" and 1 more"), "{}", outage.title);
        assert!(outage.items.iter().all(|i| !i.component_name.is_empty()));
    }

    #[test]
    fn repeats_an_hour_apart_stay_separate() {
        let c = Uuid::now_v7();
        let incidents = vec![
            incident(c, "WEBSITE", at(5, 10, 0), 4, Major),
            incident(c, "WEBSITE", at(5, 11, 5), 4, Major),
        ];
        assert_eq!(
            build_past(&incidents, &[], HistoryTab::All, now(), Tr::default())
                .recent
                .len(),
            2
        );
    }

    #[test]
    fn an_incident_someone_wrote_about_keeps_its_own_row_and_words() {
        let c = Uuid::now_v7();
        let mut told = incident(c, "API", at(5, 10, 10), 30, Major);
        told.updates.push(PublicIncidentUpdate {
            posted_at: at(5, 10, 30),
            phase: IncidentStatusPhase::Postmortem,
            message: "A bad deploy; rolled back.".into(),
            generated: false,
        });
        told.status_phase = IncidentStatusPhase::Postmortem;
        let incidents = vec![incident(c, "API", at(5, 10, 0), 5, Major), told];
        let past = build_past(&incidents, &[], HistoryTab::All, now(), Tr::default());
        assert_eq!(past.recent.len(), 2);
        let row = &past.recent[0];
        assert_eq!(row.message.as_deref(), Some("A bad deploy; rolled back."));
        assert_eq!(
            row.phase.as_ref().map(|p| p.label.as_str()),
            Some("Postmortem")
        );
        assert!(past.recent[1].phase.is_none(), "resolved says nothing new");
    }

    #[test]
    fn older_incidents_fold_away_and_open_ones_are_not_past() {
        let c = Uuid::now_v7();
        let mut open = incident(c, "WEBMAIL", at(8, 11, 0), 0, Major);
        open.ended_at = None;
        let incidents = vec![
            open,
            incident(
                Uuid::now_v7(),
                "ANTI-SPAM",
                Utc.with_ymd_and_hms(2026, 9, 30, 15, 39, 0).unwrap(),
                4,
                Partial,
            ),
            incident(
                Uuid::now_v7(),
                "WEBSITE",
                Utc.with_ymd_and_hms(2026, 9, 16, 2, 17, 0).unwrap(),
                4,
                Major,
            ),
        ];
        let past = build_past(&incidents, &[], HistoryTab::Incidents, now(), Tr::default());
        assert!(past.recent.is_empty());
        assert_eq!(past.earlier.len(), 2);
        assert_eq!(past.earlier_label, "2 earlier incidents");
        assert_eq!(past.earlier[0].title, "ANTI-SPAM error", "newest first");
    }

    #[test]
    fn one_long_incident_does_not_swallow_the_others_during_it() {
        let incidents = vec![
            incident(
                Uuid::now_v7(),
                "API",
                at(5, 0, 0),
                48 * 60,
                IncidentImpact::Degraded,
            ),
            incident(Uuid::now_v7(), "WEBSITE", at(5, 10, 0), 4, Major),
            incident(Uuid::now_v7(), "WEBMAIL", at(5, 10, 2), 4, Major),
        ];
        let past = build_past(&incidents, &[], HistoryTab::All, now(), Tr::default());
        let rows: Vec<&str> = past.recent.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(rows, ["WEBSITE, WEBMAIL", "API error"]);
    }

    #[test]
    fn the_archive_lists_an_open_incident_on_its_own_row() {
        let c = Uuid::now_v7();
        let mut open = incident(c, "WEBMAIL", at(8, 11, 0), 0, Major);
        open.ended_at = None;
        let incidents = vec![open, incident(c, "WEBMAIL", at(8, 10, 40), 5, Major)];
        let entries: Vec<HistoryEntry> = incidents
            .iter()
            .cloned()
            .map(HistoryEntry::Incident)
            .collect();
        let rows = build_rows(&entries, now(), Tr::default());
        assert_eq!(rows.len(), 2, "an open incident never folds");
        assert!(rows[0].ongoing);
        assert_eq!(rows[0].duration, "1h");
        assert!(!rows[1].ongoing);
        assert!(
            build_past(&incidents, &[], HistoryTab::All, now(), Tr::default())
                .recent
                .iter()
                .all(|e| !e.ongoing)
        );
    }

    #[test]
    fn a_long_outage_that_ended_this_week_is_in_view() {
        let start = Utc.with_ymd_and_hms(2026, 9, 30, 9, 0, 0).unwrap();
        let incidents = vec![incident(Uuid::now_v7(), "API", start, 8 * 24 * 60, Major)];
        let past = build_past(&incidents, &[], HistoryTab::All, now(), Tr::default());
        assert_eq!(past.recent.len(), 1);
        assert!(past.earlier.is_empty());
    }

    #[test]
    fn a_row_that_ran_into_the_last_week_stays_in_view_in_order_of_its_start() {
        // A run of blips that began before the week's cutoff and ended after
        // it is part of the week, and sits below a later single one.
        let c = Uuid::now_v7();
        let incidents: Vec<PublicIncident> = (0..5)
            .map(|i| {
                incident(
                    c,
                    "WEBMAIL",
                    at(1, 10, 0) + ChronoDuration::minutes(50 * i),
                    10,
                    Major,
                )
            })
            .chain([incident(Uuid::now_v7(), "API", at(1, 12, 10), 5, Partial)])
            .collect();
        let past = build_past(&incidents, &[], HistoryTab::All, now(), Tr::default());
        let recent: Vec<&str> = past.recent.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(recent, ["API error", "WEBMAIL"]);
        assert_eq!(past.recent[1].started_at, at(1, 10, 0));
        assert!(past.earlier.is_empty());
    }

    fn maintenance(start: DateTime<Utc>, hours: i64, names: &[&str]) -> PublicMaintenance {
        PublicMaintenance {
            updates: Vec::new(),
            id: Uuid::now_v7(),
            title: "Database upgrade".into(),
            description: Some("Primary moves to new hardware.".into()),
            starts_at: start,
            ends_at: start + ChronoDuration::hours(hours),
            affected_component_names: names.iter().map(|n| (*n).to_owned()).collect(),
        }
    }

    #[test]
    fn a_window_is_its_own_row_among_the_incidents_by_start() {
        let c = Uuid::now_v7();
        let incidents = vec![
            incident(c, "API", at(6, 10, 0), 5, Major),
            incident(c, "API", at(6, 10, 20), 5, Major),
        ];
        let windows = vec![
            maintenance(at(6, 10, 10), 1, &["API"]),
            maintenance(at(5, 2, 0), 2, &["API", "DB", "Mail", "DNS", "CDN"]),
        ];
        let past = build_past(&incidents, &windows, HistoryTab::All, now(), Tr::default());
        let titles: Vec<&str> = past.recent.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Database upgrade", "API", "Database upgrade"]);
        let window = &past.recent[0];
        assert!(window.items.is_empty(), "a window never folds");
        assert!(window.permalink.is_none());
        assert_eq!(window.impact_label, "Maintenance");
        assert_eq!(window.impact_class, MAINTENANCE_CHIP);
        assert!(window.phase.is_none(), "it has ended");
        assert_eq!(window.lead, "API");
        assert_eq!(window.duration, "1h");
        assert_eq!(
            window.message.as_deref(),
            Some("Primary moves to new hardware.")
        );
        assert!(
            !window.opens,
            "a short description with no updates stays one line"
        );
        assert_eq!(past.recent[1].lead, "2 incidents", "the blips still fold");
        assert_eq!(past.recent[2].lead, "API, DB, Mail and 2 more");
    }

    #[test]
    fn each_tab_lists_its_own_kind_and_counts_it() {
        let incidents = vec![incident(Uuid::now_v7(), "API", at(1, 10, 0), 5, Major)];
        let windows = vec![maintenance(at(1, 2, 0), 1, &["API"])];
        let tr = Tr::default();

        let all = build_past(&incidents, &windows, HistoryTab::All, now(), tr);
        assert_eq!(all.earlier.len(), 2);
        assert_eq!(all.earlier_label, "2 earlier entries");
        assert_eq!(
            all.quiet.as_deref(),
            Some("No resolved incidents or completed maintenance in the past 7 days.")
        );

        let only_incidents = build_past(&incidents, &windows, HistoryTab::Incidents, now(), tr);
        assert_eq!(only_incidents.earlier.len(), 1);
        assert_eq!(only_incidents.earlier_label, "1 earlier incident");
        assert_eq!(
            only_incidents.quiet.as_deref(),
            Some("No resolved incidents in the past 7 days.")
        );

        let only_windows = build_past(&incidents, &windows, HistoryTab::Maintenance, now(), tr);
        assert_eq!(only_windows.earlier.len(), 1);
        assert_eq!(only_windows.earlier[0].title, "Database upgrade");
        assert_eq!(only_windows.earlier_label, "1 earlier maintenance window");
    }

    #[test]
    fn a_quiet_month_says_so_in_the_words_of_the_tab() {
        let tr = Tr::default();
        let says = |tab| build_past(&[], &[], tab, now(), tr).quiet;
        assert_eq!(
            says(HistoryTab::All).as_deref(),
            Some("No resolved incidents or completed maintenance in the past 30 days.")
        );
        assert_eq!(
            says(HistoryTab::Incidents).as_deref(),
            Some("No resolved incidents in the past 30 days.")
        );
        assert_eq!(
            says(HistoryTab::Maintenance).as_deref(),
            Some("No completed maintenance in the past 30 days.")
        );
        let windows = vec![maintenance(at(7, 2, 0), 1, &["API"])];
        assert!(
            build_past(&[], &windows, HistoryTab::All, now(), tr)
                .quiet
                .is_none()
        );
    }

    #[test]
    fn a_long_description_opens_to_the_whole_of_it() {
        let mut long = maintenance(at(7, 2, 0), 1, &["API"]);
        long.description = Some("x".repeat(UPDATE_PREVIEW_CHARS + 1));
        let mut blank = maintenance(at(7, 4, 0), 1, &["API"]);
        blank.description = Some("   ".into());
        let past = build_past(&[], &[long, blank], HistoryTab::All, now(), Tr::default());
        assert!(past.recent[0].message.is_none(), "blank says nothing");
        let row = &past.recent[1];
        assert!(row.message.as_deref().is_some_and(|m| m.ends_with('…')));
        assert_eq!(
            row.full_message.as_deref().map(str::len),
            Some(UPDATE_PREVIEW_CHARS + 1)
        );
    }

    #[test]
    fn a_window_with_updates_reads_as_its_latest_and_opens_to_all_of_them() {
        let mut w = maintenance(at(7, 2, 0), 1, &["API"]);
        w.updates = vec![
            PublicMaintenanceUpdate {
                posted_at: at(7, 2, 20),
                phase: MaintenanceUpdatePhase::InProgress,
                message: "Running late, done by 03:30 UTC.".into(),
            },
            PublicMaintenanceUpdate {
                posted_at: at(7, 3, 40),
                phase: MaintenanceUpdatePhase::Completed,
                message: "Finished at 03:24 UTC.".into(),
            },
        ];
        let past = build_past(&[], &[w], HistoryTab::All, now(), Tr::default());
        let row = &past.recent[0];
        assert_eq!(row.message.as_deref(), Some("Finished at 03:24 UTC."));
        assert!(
            row.opens,
            "updates open the row even under a short description"
        );
        assert_eq!(
            row.full_message.as_deref(),
            Some("Primary moves to new hardware.")
        );
        let thread: Vec<(&str, &str)> = row
            .thread
            .iter()
            .map(|u| (u.phase_label.as_str(), u.message.as_str()))
            .collect();
        assert_eq!(
            thread,
            [
                ("In progress", "Running late, done by 03:30 UTC."),
                ("Completed", "Finished at 03:24 UTC."),
            ]
        );
    }

    #[test]
    fn the_archive_lists_a_running_window_as_in_progress() {
        let running = maintenance(at(8, 11, 0), 3, &["API"]);
        let rows = build_rows(&[HistoryEntry::Maintenance(running)], now(), Tr::default());
        assert_eq!(rows[0].ended_at, None);
        assert_eq!(rows[0].duration, "1h", "so far");
        assert_eq!(
            rows[0].phase.as_ref().map(|p| p.label.as_str()),
            Some("In progress")
        );
        assert!(!rows[0].ongoing, "not an outage");
    }

    #[test]
    fn tabs_link_to_the_listing_and_the_default_goes_unwritten() {
        let tabs = tab_links("/", "#history", HistoryTab::Maintenance, Tr::default());
        let hrefs: Vec<&str> = tabs.iter().map(|t| t.href.as_str()).collect();
        assert_eq!(
            hrefs,
            [
                "/#history",
                "/?show=incidents#history",
                "/?show=maintenance#history"
            ]
        );
        let labels: Vec<&str> = tabs.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["All", "Incidents", "Maintenance"]);
        assert_eq!(
            tabs.iter().map(|t| t.current).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(HistoryTab::from_query(Some("bogus")), HistoryTab::All);
        assert_eq!(HistoryTab::from_query(None), HistoryTab::All);
        assert_eq!(
            HistoryTab::from_query(Some("incidents")).query(),
            Some("incidents")
        );
    }

    #[test]
    fn a_capped_kind_ends_the_mixed_listing_where_it_stops() {
        let c = Uuid::now_v7();
        let incidents = vec![
            incident(c, "API", at(7, 10, 0), 5, Major),
            incident(c, "API", at(6, 10, 0), 5, Major),
            incident(c, "API", at(5, 10, 0), 5, Major),
        ];
        let windows = vec![
            maintenance(at(7, 2, 0), 1, &["API"]),
            maintenance(at(5, 10, 0), 1, &["API"]),
            maintenance(at(2, 2, 0), 1, &["API"]),
        ];
        let (i, w) = mixed_within_caps(&incidents, true, &windows, false);
        assert_eq!(i.len(), 3);
        assert_eq!(
            w.len(),
            2,
            "a window from before the last incident read is cut"
        );

        let (i, w) = mixed_within_caps(&incidents, false, &windows, true);
        assert_eq!((i.len(), w.len()), (3, 3), "the windows reach further back");

        let (i, w) = mixed_within_caps(&incidents, true, &windows, true);
        assert_eq!((i.len(), w.len()), (3, 2), "the later stop wins");

        let (i, w) = mixed_within_caps(&incidents, false, &windows, false);
        assert_eq!((i.len(), w.len()), (3, 3));
    }
}
