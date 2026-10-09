//! Pure status-mapping rules for the public status page.
//!
//! Every classifier derives from *confirmed* incidents — the incident
//! writer's per-region consecutive-failure gate plus region quorum — never
//! from raw check samples. A single failed probe from one region must not
//! repaint a public page; raw samples stay on the operator's region drawer.
//!
//! Independent classifiers:
//!  * [`incident_impact`] — one confirmed incident → its component impact.
//!  * [`component_status`] — worst open impact + active maintenance
//!    → [`PublicComponentStatus`].
//!  * [`overall_state`] — component statuses → [`OverallState`] (the banner).
//!  * [`day_state`] — worst incident impact overlapping one day + whether the
//!    day had checks at all → [`DayState`] cell on the daily history strip.
//!  * [`downtime_between`] — the time one component spent in each state over
//!    a range → [`DayDowntime`].
//!  * [`day_tone`] — a day's state + its downtime → the colour its cell takes.
//!
//! Each is a referentially-transparent function so the truth tables can be
//! exhaustively unit-tested below.

pub use crate::domain::IncidentImpact;
use chrono::{DateTime, Utc};

use crate::domain::{
    CheckStatus, DayDowntime, DayState, IncidentSeverity, OverallState, OverallStatus,
    PublicComponentStatus,
};
use crate::i18n::Tr;

/// Impact of one confirmed incident. `degraded` is whether the incident
/// opened on a `degraded` check status (slow / rate-limited, not hard-failed);
/// `any_region_up` is whether some region still answered when it opened —
/// present regions in `regions_up` mean a partial, not total, loss.
pub fn incident_impact(degraded: bool, any_region_up: bool) -> IncidentImpact {
    if degraded {
        IncidentImpact::Degraded
    } else if any_region_up {
        IncidentImpact::PartialOutage
    } else {
        IncidentImpact::MajorOutage
    }
}

/// [`incident_impact`] read off a stored incident row. Manual incidents carry
/// the operator's chosen severity; the check fields are placeholders on them.
/// Monitor-opened incidents derive from what the probes saw, so the same
/// outage reads the same on the day strip, the incident card and the API.
pub fn stored_incident_impact(
    origin: &str,
    severity: IncidentSeverity,
    status_at_start: &str,
    regions_up: Option<&[String]>,
) -> IncidentImpact {
    if origin == "manual" {
        return match severity {
            IncidentSeverity::Minor => IncidentImpact::Degraded,
            IncidentSeverity::Major => IncidentImpact::PartialOutage,
            IncidentSeverity::Critical => IncidentImpact::MajorOutage,
        };
    }
    incident_impact(
        status_at_start == CheckStatus::Degraded.as_str(),
        regions_up.is_some_and(|r| !r.is_empty()),
    )
}

/// Component status from the worst impact among its open incidents.
/// Maintenance dominates over any failure signal. An incident is positive
/// evidence and paints even with nothing recorded, matching [`day_state`], so
/// only a component with neither reaches `NoData`.
pub fn component_status(
    worst: Option<IncidentImpact>,
    maintenance_active: bool,
    has_evidence: bool,
) -> PublicComponentStatus {
    if maintenance_active {
        return PublicComponentStatus::Maintenance;
    }
    match worst {
        Some(IncidentImpact::MajorOutage) => PublicComponentStatus::MajorOutage,
        Some(IncidentImpact::PartialOutage) => PublicComponentStatus::PartialOutage,
        Some(IncidentImpact::Degraded) => PublicComponentStatus::Degraded,
        None if has_evidence => PublicComponentStatus::Operational,
        None => PublicComponentStatus::NoData,
    }
}

/// Overall page banner state from per-component statuses.
///
/// A `NoData` component carries no evidence, so it neither raises the banner
/// nor holds it down.
///
/// Empty component list → `Operational`.
pub fn overall_state(components: &[PublicComponentStatus]) -> OverallState {
    if components.contains(&PublicComponentStatus::MajorOutage) {
        return OverallState::MajorOutage;
    }
    if components.contains(&PublicComponentStatus::PartialOutage) {
        return OverallState::PartialOutage;
    }
    if components.contains(&PublicComponentStatus::Degraded) {
        return OverallState::MinorDisruption;
    }
    let any_maintenance = components.contains(&PublicComponentStatus::Maintenance);
    let others_operational = components.iter().all(|s| {
        matches!(
            s,
            PublicComponentStatus::Maintenance
                | PublicComponentStatus::Operational
                | PublicComponentStatus::NoData
        )
    });
    if any_maintenance && others_operational {
        return OverallState::Maintenance;
    }
    OverallState::Operational
}

pub fn overall_label_id(state: OverallState) -> &'static str {
    match state {
        OverallState::Operational => "overall-operational",
        OverallState::Maintenance => "overall-maintenance",
        OverallState::MinorDisruption => "overall-minor",
        OverallState::PartialOutage => "overall-partial",
        OverallState::MajorOutage => "overall-major",
    }
}

/// The wire label is English whatever the page language.
pub fn overall_label(state: OverallState) -> String {
    Tr::default().t(overall_label_id(state))
}

pub fn overall_status(state: OverallState) -> OverallStatus {
    OverallStatus {
        state,
        label: overall_label(state),
    }
}

/// Daily history cell: the worst confirmed-incident impact overlapping the
/// day, or Operational. An incident is positive evidence and always paints —
/// even with zero recorded checks (paused mid-incident, manual incident on an
/// unprobed component). `NoData` is reserved for genuinely silent days.
pub fn day_state(has_checks: bool, worst: Option<IncidentImpact>) -> DayState {
    match worst {
        Some(IncidentImpact::MajorOutage) => DayState::MajorOutage,
        Some(IncidentImpact::PartialOutage) => DayState::PartialOutage,
        Some(IncidentImpact::Degraded) => DayState::Degraded,
        None if has_checks => DayState::Operational,
        None => DayState::NoData,
    }
}

/// Seconds in each state over `[from, to)`, every instant counted once at the
/// worst impact covering it, so two incidents over the same minutes are one
/// stretch of downtime rather than two. Spans are clipped to the range; an
/// open incident is passed ending at `now`.
pub fn downtime_between(
    spans: &[(DateTime<Utc>, DateTime<Utc>, IncidentImpact)],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> DayDowntime {
    let mut out = DayDowntime::default();
    if to <= from {
        return out;
    }
    let mut edges: Vec<DateTime<Utc>> = spans
        .iter()
        .flat_map(|(start, end, _)| [(*start).clamp(from, to), (*end).clamp(from, to)])
        .collect();
    edges.sort_unstable();
    edges.dedup();
    for pair in edges.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let worst = spans
            .iter()
            .filter(|(start, end, _)| *start <= a && *end >= b)
            .map(|(_, _, impact)| *impact)
            .max();
        let secs = (b - a).num_seconds();
        match worst {
            Some(IncidentImpact::MajorOutage) => out.major_secs += secs,
            Some(IncidentImpact::PartialOutage) => out.partial_secs += secs,
            Some(IncidentImpact::Degraded) => out.degraded_secs += secs,
            None => {}
        }
    }
    out
}

/// Weighted downtime at which a day reads as an outage rather than a blip.
/// Atlassian Statuspage turns a day fully yellow at 20 minutes and fully red at
/// 60; the strip takes those two points as its band edges.
const TONE_OUTAGE_SECS: i64 = 20 * 60;
const TONE_MAJOR_SECS: i64 = 60 * 60;

/// The colour a day cell takes: how long the component was down that day, not
/// just the worst state it touched, so a four-minute blip does not paint the
/// day the way a lost afternoon does. The state still names what happened;
/// this picks which state's colour the cell borrows. Any disruption at all
/// keeps the day off green.
pub fn day_tone(state: DayState, downtime: DayDowntime) -> DayState {
    match state {
        DayState::Degraded | DayState::PartialOutage | DayState::MajorOutage => {
            match downtime.weighted_secs() {
                w if w >= TONE_MAJOR_SECS => DayState::MajorOutage,
                w if w >= TONE_OUTAGE_SECS => DayState::PartialOutage,
                _ => DayState::Degraded,
            }
        }
        DayState::Operational | DayState::Maintenance | DayState::NoData => state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── incident_impact truth table ─────────────────────────────────────────

    #[test]
    fn impact_degraded_wins_over_region_split() {
        assert_eq!(incident_impact(true, true), IncidentImpact::Degraded);
        assert_eq!(incident_impact(true, false), IncidentImpact::Degraded);
    }

    #[test]
    fn impact_some_region_up_is_partial() {
        assert_eq!(incident_impact(false, true), IncidentImpact::PartialOutage);
    }

    #[test]
    fn impact_no_region_up_is_major() {
        assert_eq!(incident_impact(false, false), IncidentImpact::MajorOutage);
    }

    #[test]
    fn impact_ord_ranks_major_worst() {
        assert!(IncidentImpact::MajorOutage > IncidentImpact::PartialOutage);
        assert!(IncidentImpact::PartialOutage > IncidentImpact::Degraded);
    }

    // ── component_status truth table ────────────────────────────────────────

    #[test]
    fn component_maintenance_dominates_even_with_major_impact() {
        assert_eq!(
            component_status(Some(IncidentImpact::MajorOutage), true, true),
            PublicComponentStatus::Maintenance
        );
    }

    #[test]
    fn component_maintenance_dominates_with_no_incident() {
        assert_eq!(
            component_status(None, true, true),
            PublicComponentStatus::Maintenance
        );
    }

    #[test]
    fn component_no_open_incident_is_operational() {
        assert_eq!(
            component_status(None, false, true),
            PublicComponentStatus::Operational
        );
    }

    #[test]
    fn component_without_evidence_is_no_data_not_operational() {
        assert_eq!(
            component_status(None, false, false),
            PublicComponentStatus::NoData,
            "a job that has never run is not operational"
        );
    }

    #[test]
    fn an_incident_paints_a_component_that_recorded_nothing() {
        // A manual incident on an unprobed component still shows as an outage.
        assert_eq!(
            component_status(Some(IncidentImpact::MajorOutage), false, false),
            PublicComponentStatus::MajorOutage
        );
        assert_eq!(
            component_status(None, true, false),
            PublicComponentStatus::Maintenance
        );
    }

    #[test]
    fn component_maps_each_impact() {
        assert_eq!(
            component_status(Some(IncidentImpact::Degraded), false, true),
            PublicComponentStatus::Degraded
        );
        assert_eq!(
            component_status(Some(IncidentImpact::PartialOutage), false, true),
            PublicComponentStatus::PartialOutage
        );
        assert_eq!(
            component_status(Some(IncidentImpact::MajorOutage), false, true),
            PublicComponentStatus::MajorOutage
        );
    }

    // ── overall_state truth table ───────────────────────────────────────────

    #[test]
    fn overall_ignores_components_with_no_evidence() {
        use PublicComponentStatus as S;
        // Indistinguishable from the empty page, which already reads operational.
        assert_eq!(overall_state(&[S::NoData]), OverallState::Operational);
        assert_eq!(
            overall_state(&[S::NoData, S::Operational]),
            OverallState::Operational
        );
        for (other, expected) in [
            (S::MajorOutage, OverallState::MajorOutage),
            (S::PartialOutage, OverallState::PartialOutage),
            (S::Degraded, OverallState::MinorDisruption),
        ] {
            assert_eq!(
                overall_state(&[S::NoData, other]),
                expected,
                "silence never masks a real outage"
            );
        }
        assert_eq!(
            overall_state(&[S::NoData, S::Maintenance]),
            OverallState::Maintenance,
            "a silent component does not block the maintenance banner"
        );
    }

    #[test]
    fn overall_empty_is_operational() {
        assert_eq!(overall_state(&[]), OverallState::Operational);
    }

    #[test]
    fn overall_all_operational_is_operational() {
        let s = [PublicComponentStatus::Operational; 3];
        assert_eq!(overall_state(&s), OverallState::Operational);
    }

    #[test]
    fn overall_maintenance_with_operational_is_maintenance() {
        let s = [
            PublicComponentStatus::Maintenance,
            PublicComponentStatus::Operational,
        ];
        assert_eq!(overall_state(&s), OverallState::Maintenance);
    }

    #[test]
    fn overall_only_maintenance_is_maintenance() {
        let s = [PublicComponentStatus::Maintenance];
        assert_eq!(overall_state(&s), OverallState::Maintenance);
    }

    #[test]
    fn overall_maintenance_plus_degraded_is_minor_disruption() {
        // Maintenance wins only when *all others* are Operational. Mixed
        // Maintenance + Degraded → MinorDisruption.
        let s = [
            PublicComponentStatus::Maintenance,
            PublicComponentStatus::Degraded,
        ];
        assert_eq!(overall_state(&s), OverallState::MinorDisruption);
    }

    #[test]
    fn overall_any_degraded_is_minor_disruption() {
        let s = [
            PublicComponentStatus::Operational,
            PublicComponentStatus::Degraded,
        ];
        assert_eq!(overall_state(&s), OverallState::MinorDisruption);
    }

    #[test]
    fn overall_partial_with_degraded_is_partial() {
        let s = [
            PublicComponentStatus::Degraded,
            PublicComponentStatus::PartialOutage,
        ];
        assert_eq!(overall_state(&s), OverallState::PartialOutage);
    }

    #[test]
    fn overall_partial_alone_is_partial() {
        let s = [PublicComponentStatus::PartialOutage];
        assert_eq!(overall_state(&s), OverallState::PartialOutage);
    }

    #[test]
    fn overall_major_dominates_partial() {
        let s = [
            PublicComponentStatus::PartialOutage,
            PublicComponentStatus::MajorOutage,
        ];
        assert_eq!(overall_state(&s), OverallState::MajorOutage);
    }

    #[test]
    fn overall_major_alone_is_major() {
        let s = [PublicComponentStatus::MajorOutage];
        assert_eq!(overall_state(&s), OverallState::MajorOutage);
    }

    #[test]
    fn overall_labels_match_expected_strings() {
        assert_eq!(
            overall_label(OverallState::Operational),
            "All Systems Operational"
        );
        assert_eq!(
            overall_label(OverallState::Maintenance),
            "Maintenance in progress"
        );
        assert_eq!(
            overall_label(OverallState::MinorDisruption),
            "Minor Service Disruption"
        );
        assert_eq!(
            overall_label(OverallState::PartialOutage),
            "Partial System Outage"
        );
        assert_eq!(
            overall_label(OverallState::MajorOutage),
            "Major System Outage"
        );
    }

    // ── day_state truth table ───────────────────────────────────────────────

    #[test]
    fn day_incident_paints_even_without_checks() {
        assert_eq!(
            day_state(false, Some(IncidentImpact::MajorOutage)),
            DayState::MajorOutage
        );
    }

    #[test]
    fn day_silent_without_incident_is_no_data() {
        assert_eq!(day_state(false, None), DayState::NoData);
    }

    #[test]
    fn day_with_checks_and_no_incident_is_operational() {
        assert_eq!(day_state(true, None), DayState::Operational);
    }

    #[test]
    fn day_maps_each_impact() {
        assert_eq!(
            day_state(true, Some(IncidentImpact::Degraded)),
            DayState::Degraded
        );
        assert_eq!(
            day_state(true, Some(IncidentImpact::PartialOutage)),
            DayState::PartialOutage
        );
        assert_eq!(
            day_state(true, Some(IncidentImpact::MajorOutage)),
            DayState::MajorOutage
        );
    }

    fn at(min: i64) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 10, 4, 0, 0, 0).unwrap() + chrono::Duration::minutes(min)
    }

    fn mins(secs: i64) -> i64 {
        secs / 60
    }

    #[test]
    fn overlapping_incidents_count_once_at_the_worse_state() {
        let spans = [
            (at(60), at(120), IncidentImpact::PartialOutage),
            (at(90), at(100), IncidentImpact::MajorOutage),
            (at(110), at(130), IncidentImpact::Degraded),
        ];
        let d = downtime_between(&spans, at(0), at(24 * 60));
        assert_eq!(mins(d.major_secs), 10);
        assert_eq!(mins(d.partial_secs), 50, "60–90 and 100–120");
        assert_eq!(mins(d.degraded_secs), 10, "only 120–130 is degraded alone");
    }

    #[test]
    fn downtime_is_clipped_to_the_day() {
        let spans = [(at(-30), at(15), IncidentImpact::MajorOutage)];
        let d = downtime_between(&spans, at(0), at(24 * 60));
        assert_eq!(mins(d.major_secs), 15);
        assert_eq!(
            downtime_between(&spans, at(15), at(15)),
            DayDowntime::default()
        );
    }

    #[test]
    fn a_partial_outage_weighs_thirty_percent_and_degraded_nothing() {
        let d = DayDowntime {
            major_secs: 600,
            partial_secs: 1_000,
            degraded_secs: 7_200,
        };
        assert_eq!(d.weighted_secs(), 900);
    }

    #[test]
    fn a_day_is_tinted_by_how_long_it_was_down() {
        let major = |m: i64| DayDowntime {
            major_secs: m * 60,
            ..Default::default()
        };
        let partial = |m: i64| DayDowntime {
            partial_secs: m * 60,
            ..Default::default()
        };
        for (state, downtime, tone) in [
            (DayState::MajorOutage, major(4), DayState::Degraded),
            (DayState::MajorOutage, major(19), DayState::Degraded),
            (DayState::MajorOutage, major(20), DayState::PartialOutage),
            (DayState::MajorOutage, major(59), DayState::PartialOutage),
            (DayState::MajorOutage, major(60), DayState::MajorOutage),
            (DayState::PartialOutage, partial(66), DayState::Degraded),
            (
                DayState::PartialOutage,
                partial(67),
                DayState::PartialOutage,
            ),
            (DayState::PartialOutage, partial(200), DayState::MajorOutage),
            (
                DayState::Degraded,
                DayDowntime::default(),
                DayState::Degraded,
            ),
            (
                DayState::Operational,
                DayDowntime::default(),
                DayState::Operational,
            ),
            (DayState::NoData, DayDowntime::default(), DayState::NoData),
            (DayState::Maintenance, major(90), DayState::Maintenance),
        ] {
            assert_eq!(day_tone(state, downtime), tone, "{state:?} {downtime:?}");
        }
    }
}
