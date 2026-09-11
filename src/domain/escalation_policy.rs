//! Escalation policy model + the pure ladder-walk decision function.
//!
//! A policy is an ordered list of steps (rungs); each step pages a set of
//! targets and carries a `delay_secs` — how long the engine waits after paging
//! that rung before advancing to the next. When the last rung's delay elapses
//! the engine either re-walks the whole ladder (`repeat_count` more times) or
//! gives up. [`next_step`] encodes that walk with no I/O so it is exhaustively
//! testable; the engine owns the timing and delivery.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// What a step pages. `channel` routes to a notification channel today;
/// `user` and `schedule` are accepted by the schema but only become routable
/// once on-call resolution (a later phase) lands — the engine skips them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EscalationTargetType {
    User,
    Schedule,
    Channel,
}

impl EscalationTargetType {
    pub const ALL: &'static [Self] = &[Self::User, Self::Schedule, Self::Channel];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Schedule => "schedule",
            Self::Channel => "channel",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "user" => Self::User,
            "schedule" => Self::Schedule,
            _ => Self::Channel,
        }
    }
}

/// One paging destination within a step. Exactly one id is set, matching
/// `target_type` (enforced by the DB CHECK and the create-path validation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EscalationTarget {
    pub id: Uuid,
    pub target_type: EscalationTargetType,
    #[schema(nullable = true)]
    pub user_id: Option<Uuid>,
    #[schema(nullable = true)]
    pub schedule_id: Option<Uuid>,
    #[schema(nullable = true)]
    pub channel_id: Option<Uuid>,
}

/// One rung of the ladder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EscalationStep {
    pub id: Uuid,
    pub level: i32,
    /// Seconds to wait after paging this rung before advancing to the next.
    pub delay_secs: i32,
    pub targets: Vec<EscalationTarget>,
}

/// A full policy with its ordered steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EscalationPolicy {
    pub id: Uuid,
    pub name: String,
    #[schema(nullable = true)]
    pub description: Option<String>,
    /// Extra times to re-walk the whole ladder if still unacknowledged.
    pub repeat_count: i32,
    pub steps: Vec<EscalationStep>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lightweight list row (no steps loaded) for the policy index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EscalationPolicySummary {
    pub id: Uuid,
    pub name: String,
    #[schema(nullable = true)]
    pub description: Option<String>,
    pub repeat_count: i32,
    pub step_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A target to create within a step.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewEscalationTarget {
    pub target_type: EscalationTargetType,
    #[serde(default)]
    #[schema(nullable = true)]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    #[schema(nullable = true)]
    pub schedule_id: Option<Uuid>,
    #[serde(default)]
    #[schema(nullable = true)]
    pub channel_id: Option<Uuid>,
}

/// A step to create within a policy.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewEscalationStep {
    pub level: i32,
    #[serde(default = "default_step_delay")]
    pub delay_secs: i32,
    pub targets: Vec<NewEscalationTarget>,
}

fn default_step_delay() -> i32 {
    300
}

/// Create or fully replace a policy. PATCH replaces the whole step list so the
/// builder UI can edit levels/targets in one round-trip (no nested CRUD).
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewEscalationPolicy {
    pub name: String,
    #[serde(default)]
    #[schema(nullable = true)]
    pub description: Option<String>,
    #[serde(default)]
    pub repeat_count: i32,
    #[serde(default)]
    pub steps: Vec<NewEscalationStep>,
}

/// The engine's instruction for the current escalation tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationDecision {
    /// Page `level` now (round `round`) and schedule the next escalation
    /// `delay_secs` from now.
    Page {
        level: i32,
        round: i32,
        delay_secs: i32,
    },
    /// The ladder (and all repeats) is spent; stop paging.
    Exhausted,
}

/// Pure ladder walk. Given the current `level` (0 = not yet started) and
/// `round` (0-based repeat counter), decide what to page next.
///
/// Advancing past the last rung wraps back to the first when `repeat_count`
/// allows another round, otherwise the policy is exhausted. `steps` MUST be
/// sorted ascending by `level`; an empty policy is always exhausted.
pub fn next_step(
    steps: &[EscalationStep],
    repeat_count: i32,
    level: i32,
    round: i32,
) -> EscalationDecision {
    let Some(first) = steps.first() else {
        return EscalationDecision::Exhausted;
    };
    match steps.iter().find(|s| s.level > level) {
        Some(next) => EscalationDecision::Page {
            level: next.level,
            round,
            delay_secs: next.delay_secs,
        },
        None if round < repeat_count => EscalationDecision::Page {
            level: first.level,
            round: round + 1,
            delay_secs: first.delay_secs,
        },
        None => EscalationDecision::Exhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(level: i32, delay_secs: i32) -> EscalationStep {
        EscalationStep {
            id: Uuid::nil(),
            level,
            delay_secs,
            targets: vec![],
        }
    }

    #[test]
    fn empty_policy_is_exhausted() {
        assert_eq!(next_step(&[], 0, 0, 0), EscalationDecision::Exhausted);
        assert_eq!(next_step(&[], 5, 0, 0), EscalationDecision::Exhausted);
    }

    #[test]
    fn initial_tick_pages_the_first_level() {
        let steps = [step(1, 300), step(2, 600)];
        assert_eq!(
            next_step(&steps, 0, 0, 0),
            EscalationDecision::Page {
                level: 1,
                round: 0,
                delay_secs: 300,
            }
        );
    }

    #[test]
    fn advances_to_the_next_higher_level() {
        let steps = [step(1, 300), step(2, 600), step(3, 900)];
        assert_eq!(
            next_step(&steps, 0, 1, 0),
            EscalationDecision::Page {
                level: 2,
                round: 0,
                delay_secs: 600,
            }
        );
        assert_eq!(
            next_step(&steps, 0, 2, 0),
            EscalationDecision::Page {
                level: 3,
                round: 0,
                delay_secs: 900,
            }
        );
    }

    #[test]
    fn last_level_without_repeat_is_exhausted() {
        let steps = [step(1, 300), step(2, 600)];
        assert_eq!(next_step(&steps, 0, 2, 0), EscalationDecision::Exhausted);
    }

    #[test]
    fn last_level_with_repeat_wraps_to_first_and_bumps_round() {
        let steps = [step(1, 300), step(2, 600)];
        assert_eq!(
            next_step(&steps, 2, 2, 0),
            EscalationDecision::Page {
                level: 1,
                round: 1,
                delay_secs: 300,
            }
        );
        assert_eq!(
            next_step(&steps, 2, 2, 1),
            EscalationDecision::Page {
                level: 1,
                round: 2,
                delay_secs: 300,
            }
        );
        // Both repeats spent → exhausted.
        assert_eq!(next_step(&steps, 2, 2, 2), EscalationDecision::Exhausted);
    }

    #[test]
    fn non_contiguous_levels_advance_to_the_next_present_one() {
        // Levels need not be 1,2,3 — gaps are walked in order.
        let steps = [step(1, 60), step(5, 120), step(9, 180)];
        assert_eq!(
            next_step(&steps, 0, 1, 0),
            EscalationDecision::Page {
                level: 5,
                round: 0,
                delay_secs: 120,
            }
        );
        assert_eq!(
            next_step(&steps, 0, 5, 0),
            EscalationDecision::Page {
                level: 9,
                round: 0,
                delay_secs: 180,
            }
        );
        assert_eq!(next_step(&steps, 0, 9, 0), EscalationDecision::Exhausted);
    }

    #[test]
    fn db_str_roundtrips() {
        for t in EscalationTargetType::ALL {
            assert_eq!(EscalationTargetType::from_db_str(t.as_db_str()), *t);
        }
    }
}
