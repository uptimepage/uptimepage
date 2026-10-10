//! How an incident reads wherever it is listed: its impact and phase chips,
//! and how much of an update a listing previews.

use crate::domain::{IncidentImpact, IncidentStatusPhase};

/// Characters of an update a listing shows before cutting it off.
pub(super) const UPDATE_PREVIEW_CHARS: usize = 241;

/// Same words as the day strip's [`super::view::day_classes`], so one outage
/// reads the same in the strip popover, the incident card and the detail page.
pub(super) fn impact_classes(i: IncidentImpact) -> (&'static str, &'static str) {
    match i {
        IncidentImpact::Degraded => ("state-degraded", "public-chip public-sev--minor"),
        IncidentImpact::PartialOutage => ("state-partial-outage", "public-chip public-sev--major"),
        IncidentImpact::MajorOutage => ("state-major-outage", "public-chip public-sev--critical"),
    }
}

pub(super) fn phase_classes(p: IncidentStatusPhase) -> (&'static str, &'static str) {
    match p {
        IncidentStatusPhase::Investigating => (
            "phase-investigating",
            "public-chip public-phase--investigating",
        ),
        IncidentStatusPhase::Identified => {
            ("phase-identified", "public-chip public-phase--identified")
        }
        IncidentStatusPhase::Monitoring => {
            ("phase-monitoring", "public-chip public-phase--monitoring")
        }
        IncidentStatusPhase::Resolved => ("phase-resolved", "public-chip public-phase--resolved"),
        IncidentStatusPhase::Postmortem => {
            ("phase-postmortem", "public-chip public-phase--postmortem")
        }
    }
}
