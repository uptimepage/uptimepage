//! What an operator last said about a service no probe can judge.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{CheckStatus, UserId};

/// Longest note an operator can attach to a state. It rides every result and
/// the incident's cause, so it is a reason, not a write-up.
pub const MAX_MANUAL_NOTE_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ManualStatus {
    Up,
    Degraded,
    Down,
}

impl ManualStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Degraded => "degraded",
            Self::Down => "down",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "up" => Some(Self::Up),
            "degraded" => Some(Self::Degraded),
            "down" => Some(Self::Down),
            _ => None,
        }
    }

    pub const fn check_status(self) -> CheckStatus {
        match self {
            Self::Up => CheckStatus::Up,
            Self::Degraded => CheckStatus::Degraded,
            Self::Down => CheckStatus::Down,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ManualState {
    pub status: ManualStatus,
    /// The reason given with the state, carried into the incident it opens.
    pub note: Option<String>,
    /// When the state was last set, or the monitor's creation if never.
    pub set_at: DateTime<Utc>,
    /// Who set it. Empty until the first set, and once the setter's deleted
    /// account is purged.
    #[serde(default)]
    #[schema(nullable = true)]
    pub set_by: Option<UserId>,
}

impl ManualState {
    /// A monitor nobody has set yet is up since it was created, the way a
    /// status page component starts operational.
    pub fn initial(created_at: DateTime<Utc>) -> Self {
        Self {
            status: ManualStatus::Up,
            note: None,
            set_at: created_at,
            set_by: None,
        }
    }

    /// The cause a bad state reports, read by notifications and the incident.
    pub fn error(&self) -> Option<String> {
        let status = match self.status {
            ManualStatus::Up => return None,
            other => other.as_str(),
        };
        Some(match self.note.as_deref() {
            Some(note) => format!("marked {status}: {note}"),
            None => format!("marked {status}"),
        })
    }

    /// Whether setting this would change nothing.
    pub fn matches(&self, status: ManualStatus, note: &Option<String>) -> bool {
        self.status == status && &self.note == note
    }

    /// Two nodes, or a refresh racing a set, converge on the later set.
    pub fn merge_newer(&mut self, other: Self) {
        if other.set_at > self.set_at {
            *self = other;
        }
    }
}

/// Trimmed, empty dropped, bounded. `None` means no note.
pub fn normalize_note(note: Option<&str>) -> Result<Option<String>, String> {
    let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if note.chars().count() > MAX_MANUAL_NOTE_CHARS {
        return Err(format!(
            "note must be at most {MAX_MANUAL_NOTE_CHARS} characters"
        ));
    }
    if note
        .chars()
        .any(|c| c.is_control() || super::text::is_invisible(c))
    {
        return Err("note must be a single line of visible text".to_string());
    }
    Ok(Some(note.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: ManualStatus, note: Option<&str>) -> ManualState {
        ManualState {
            status,
            note: note.map(str::to_string),
            set_at: Utc::now(),
            set_by: None,
        }
    }

    #[test]
    fn up_reports_no_cause_even_with_a_note() {
        assert_eq!(
            state(ManualStatus::Up, Some("carrier fixed it")).error(),
            None
        );
    }

    #[test]
    fn a_bad_state_names_itself_and_the_reason() {
        assert_eq!(
            state(ManualStatus::Down, None).error().as_deref(),
            Some("marked down")
        );
        assert_eq!(
            state(ManualStatus::Degraded, Some("one trunk of two"))
                .error()
                .as_deref(),
            Some("marked degraded: one trunk of two")
        );
    }

    #[test]
    fn the_later_set_wins_either_way_round() {
        let older = ManualState::initial(Utc::now() - chrono::Duration::minutes(5));
        let newer = state(ManualStatus::Down, None);
        let mut a = older.clone();
        a.merge_newer(newer.clone());
        assert_eq!(a, newer);
        let mut b = newer.clone();
        b.merge_newer(older);
        assert_eq!(b, newer);
    }

    #[test]
    fn notes_are_trimmed_bounded_and_single_line() {
        assert_eq!(normalize_note(None), Ok(None));
        assert_eq!(normalize_note(Some("   ")), Ok(None));
        assert_eq!(
            normalize_note(Some("  carrier outage ")),
            Ok(Some("carrier outage".to_string()))
        );
        assert!(normalize_note(Some(&"x".repeat(MAX_MANUAL_NOTE_CHARS + 1))).is_err());
        assert!(normalize_note(Some(&"ї".repeat(MAX_MANUAL_NOTE_CHARS))).is_ok());
        for hidden in [
            "line one\nline two",
            "trunk\u{2028}restored",
            "a\u{202E}b",
            "a\u{200B}b",
        ] {
            assert!(normalize_note(Some(hidden)).is_err(), "{hidden:?}");
        }
    }

    #[test]
    fn statuses_round_trip_their_stored_form() {
        for s in [ManualStatus::Up, ManualStatus::Degraded, ManualStatus::Down] {
            assert_eq!(ManualStatus::from_db_str(s.as_str()), Some(s));
        }
        assert_eq!(ManualStatus::from_db_str("error"), None);
    }
}
