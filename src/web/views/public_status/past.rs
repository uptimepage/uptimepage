//! The past-incidents listing: incidents as compact rows, with the ones that
//! read as a single disruption folded into one row a visitor can open. Shared
//! by the status page and the archive.

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::domain::{IncidentImpact, IncidentStatusPhase, PublicIncident, outage_parts};
use crate::i18n::Tr;

use super::view::{UPDATE_PREVIEW_CHARS, impact_classes, phase_classes};

/// Incidents that ended within this long stay in view; older ones fold away
/// behind one toggle.
const RECENT_DAYS: i64 = 7;
/// Repeat incidents on one component closer together than this read as one
/// disruption.
const FOLD_GAP: ChronoDuration = ChronoDuration::hours(1);
/// Incidents on different components read as one fault only when they
/// overlap and began within this long of each other, so one long incident
/// does not swallow every unrelated one that happens during it.
const STARTED_TOGETHER: ChronoDuration = ChronoDuration::minutes(15);
/// Component names a folded row spells out before it counts the rest.
const NAMED_COMPONENTS: usize = 3;

/// One row of the listing: a single incident, or several folded together.
pub struct PastEntry {
    /// Stable across refreshes, so a row a visitor opened stays open.
    pub key: String,
    pub title: String,
    /// Set on a single incident; a folded row opens instead of linking.
    pub permalink: Option<String>,
    pub impact_label: String,
    pub impact_class: &'static str,
    /// Shown only when it says more than "resolved", such as a postmortem.
    pub phase: Option<PhaseChip>,
    /// Still open; only the archive lists one, since the status page shows it
    /// under its active incidents.
    pub ongoing: bool,
    /// What leads the meta line: the component, or how many incidents.
    pub lead: String,
    /// When it began, which is also what the listing is ordered by.
    pub started_at: DateTime<Utc>,
    /// When the last of it ended; `None` while it is still open.
    pub ended_at: Option<DateTime<Utc>>,
    /// How long it was down, less any stretch it was back up in; for a folded
    /// row the gaps between its incidents are not counted either.
    pub duration: String,
    /// The latest update a person wrote, if any.
    pub message: Option<String>,
    /// The folded incidents, oldest first; empty for a single incident.
    pub items: Vec<PastItem>,
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

pub struct PastIncidents {
    pub recent: Vec<PastEntry>,
    pub earlier: Vec<PastEntry>,
    /// "12 earlier incidents", counting incidents rather than rows.
    pub earlier_label: String,
}

impl PastIncidents {
    pub fn is_empty(&self) -> bool {
        self.recent.is_empty() && self.earlier.is_empty()
    }
}

/// The status page's listing: what ended in the last week in view, the rest
/// folded away. Open incidents are left to the active list above it.
pub(super) fn build_past(
    incidents: &[PublicIncident],
    now: DateTime<Utc>,
    tr: Tr,
) -> PastIncidents {
    let cutoff = now - ChronoDuration::days(RECENT_DAYS);
    let closed: Vec<&PublicIncident> = incidents.iter().filter(|i| i.ended_at.is_some()).collect();
    let (recent, earlier): (Vec<PastEntry>, Vec<PastEntry>) = fold(&closed, now, tr)
        .into_iter()
        .partition(|e| e.ended_at.is_none_or(|end| end >= cutoff));
    let earlier_count: usize = earlier.iter().map(|e| e.items.len().max(1)).sum();
    PastIncidents {
        recent,
        earlier,
        earlier_label: tr.t_args("past-earlier", [("count", earlier_count.into())]),
    }
}

/// One month of the archive, folded the same way, newest first. An incident
/// still open is listed too, on its own row.
pub(super) fn build_rows(
    incidents: &[PublicIncident],
    now: DateTime<Utc>,
    tr: Tr,
) -> Vec<PastEntry> {
    fold(&incidents.iter().collect::<Vec<_>>(), now, tr)
}

/// Incidents grouped into rows, newest first by when each row began.
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

    let mut rows: Vec<PastEntry> = groups
        .into_iter()
        .map(|g| match g.as_slice() {
            [one] => single(one, now, tr),
            _ => folded(&g, now, tr),
        })
        .collect();
    rows.sort_by(|a, b| (b.started_at, &b.key).cmp(&(a.started_at, &a.key)));
    rows
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
        ongoing: inc.ended_at.is_none(),
        lead: inc.component_name.clone(),
        started_at: inc.started_at,
        ended_at: inc.ended_at,
        duration: tr.duration(lasted(inc, now)),
        message: narrated(inc),
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
    let title = match names.len() {
        0 => group[0].title.clone(),
        n if n <= NAMED_COMPONENTS => names.join(", "),
        n => tr.t_args(
            "past-components-more",
            [
                ("names", names[..NAMED_COMPONENTS].join(", ").into()),
                ("count", (n - NAMED_COMPONENTS).into()),
            ],
        ),
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
    use crate::domain::{IncidentSeverity, PublicIncidentUpdate};

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
        let past = build_past(&[held], now(), Tr::default());
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
        let past = build_past(&incidents, now(), Tr::default());
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
        let past = build_past(&incidents, now(), Tr::default());
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
        assert_eq!(build_past(&incidents, now(), Tr::default()).recent.len(), 2);
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
        let past = build_past(&incidents, now(), Tr::default());
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
        let past = build_past(&incidents, now(), Tr::default());
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
        let past = build_past(&incidents, now(), Tr::default());
        let rows: Vec<&str> = past.recent.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(rows, ["WEBSITE, WEBMAIL", "API error"]);
    }

    #[test]
    fn the_archive_lists_an_open_incident_on_its_own_row() {
        let c = Uuid::now_v7();
        let mut open = incident(c, "WEBMAIL", at(8, 11, 0), 0, Major);
        open.ended_at = None;
        let incidents = vec![open, incident(c, "WEBMAIL", at(8, 10, 40), 5, Major)];
        let rows = build_rows(&incidents, now(), Tr::default());
        assert_eq!(rows.len(), 2, "an open incident never folds");
        assert!(rows[0].ongoing);
        assert_eq!(rows[0].duration, "1h");
        assert!(!rows[1].ongoing);
        assert!(
            build_past(&incidents, now(), Tr::default())
                .recent
                .iter()
                .all(|e| !e.ongoing)
        );
    }

    #[test]
    fn a_long_outage_that_ended_this_week_is_in_view() {
        let start = Utc.with_ymd_and_hms(2026, 9, 30, 9, 0, 0).unwrap();
        let incidents = vec![incident(Uuid::now_v7(), "API", start, 8 * 24 * 60, Major)];
        let past = build_past(&incidents, now(), Tr::default());
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
        let past = build_past(&incidents, now(), Tr::default());
        let recent: Vec<&str> = past.recent.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(recent, ["API error", "WEBMAIL"]);
        assert_eq!(past.recent[1].started_at, at(1, 10, 0));
        assert!(past.earlier.is_empty());
    }
}
