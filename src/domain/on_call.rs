//! On-call schedules + the pure who-is-on-call resolver.
//!
//! A schedule is a stack of rotation layers; the highest-order layer that has
//! participants determines the on-call user, and a one-off override beats the
//! computed rotation for its window. Who-is-on-call is never stored — the
//! escalation engine calls [`resolve_on_call`] at page time. The resolver is
//! referentially transparent (no I/O) so rotation/DST/override math is
//! exhaustively unit-tested here; the store loads the rows and the engine maps
//! the resolved users to their contact channels.

use std::str::FromStr;

use chrono::{
    DateTime, Duration as ChronoDuration, LocalResult, NaiveDateTime, Offset, TimeZone, Utc,
};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::user::UserId;

/// How a layer hands off. `daily`/`weekly` boundaries land at the same wall
/// clock time in the schedule's timezone each period (DST-stable); `custom` is
/// a fixed second interval measured from the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RotationType {
    Daily,
    Weekly,
    Custom,
}

impl RotationType {
    pub const ALL: &'static [Self] = &[Self::Daily, Self::Weekly, Self::Custom];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Custom => "custom",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            _ => Self::Custom,
        }
    }
}

/// One responder slot in a layer's rotation, ordered by `position`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnCallParticipant {
    pub id: Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    pub position: i32,
}

/// One rotation within a schedule. The on-call user is the participant whose
/// rotation slot contains the query instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnCallLayer {
    pub id: Uuid,
    #[schema(nullable = true)]
    pub name: Option<String>,
    pub rotation_type: RotationType,
    /// Handoff interval in seconds. For `daily`/`weekly` this is a whole number
    /// of days; the boundary time-of-day is taken from `handoff_at`.
    pub rotation_length_secs: i32,
    pub handoff_at: DateTime<Utc>,
    /// Higher wins when layers are stacked.
    pub layer_order: i32,
    pub created_at: DateTime<Utc>,
    pub participants: Vec<OnCallParticipant>,
}

/// A one-off coverage swap: this user is on call for `[starts_at, ends_at)`,
/// overriding the computed rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnCallOverride {
    pub id: Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    #[schema(value_type = Option<String>, format = "uuid", nullable = true)]
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

/// Schedule metadata. The IANA `timezone` anchors daily/weekly handoff math.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnCallSchedule {
    pub id: Uuid,
    pub name: String,
    pub timezone: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A schedule with its layers (participants nested) — the full aggregate the
/// editor renders and the resolver consumes. Overrides ride separately because
/// the calendar manages them out of band from the rotation editor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnCallScheduleDetail {
    pub schedule: OnCallSchedule,
    pub layers: Vec<OnCallLayer>,
    pub overrides: Vec<OnCallOverride>,
}

/// Lightweight list row (no layers loaded) for the schedule index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnCallScheduleSummary {
    pub id: Uuid,
    pub name: String,
    pub timezone: String,
    pub layer_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Create/replace payloads ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOnCallParticipant {
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOnCallLayer {
    #[serde(default)]
    #[schema(nullable = true)]
    pub name: Option<String>,
    pub rotation_type: RotationType,
    pub rotation_length_secs: i32,
    pub handoff_at: DateTime<Utc>,
    #[serde(default)]
    pub layer_order: i32,
    /// Ordered participants; their list position is the rotation order.
    pub participants: Vec<NewOnCallParticipant>,
}

/// Create or fully replace a schedule's metadata + layer stack in one call
/// (mirrors the escalation-policy builder). Overrides are managed separately.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOnCallSchedule {
    pub name: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default)]
    pub layers: Vec<NewOnCallLayer>,
}

fn default_timezone() -> String {
    "UTC".into()
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewOnCallOverride {
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

// ── The pure resolver ────────────────────────────────────────────────────

/// Who is on call for `schedule` at `at`. Overrides covering the instant win
/// (their users, deduped); otherwise the highest-order layer with participants
/// supplies its current rotation slot. Empty when nothing covers the instant.
///
/// Pure: no I/O. `layers`/`overrides` need not be pre-sorted — the resolver
/// orders them itself so callers can hand over rows in any order.
pub fn resolve_on_call(
    schedule: &OnCallSchedule,
    layers: &[OnCallLayer],
    overrides: &[OnCallOverride],
    at: DateTime<Utc>,
) -> Vec<UserId> {
    let mut covering: Vec<&OnCallOverride> = overrides
        .iter()
        .filter(|o| o.starts_at <= at && at < o.ends_at)
        .collect();
    if !covering.is_empty() {
        covering.sort_by_key(|o| o.starts_at);
        let mut out: Vec<UserId> = Vec::new();
        for o in covering {
            if !out.contains(&o.user_id) {
                out.push(o.user_id);
            }
        }
        return out;
    }

    let tz = Tz::from_str(&schedule.timezone).unwrap_or(Tz::UTC);
    let mut ranked: Vec<&OnCallLayer> = layers
        .iter()
        .filter(|l| !l.participants.is_empty())
        .collect();
    // Top of the stack wins; created_at breaks an order tie deterministically.
    ranked.sort_by(|a, b| {
        b.layer_order
            .cmp(&a.layer_order)
            .then(a.created_at.cmp(&b.created_at))
    });
    for layer in ranked {
        if let Some(uid) = layer_on_call(layer, tz, at) {
            return vec![uid];
        }
    }
    vec![]
}

/// The participant on call in one layer at `at`, or `None` for an empty layer.
fn layer_on_call(layer: &OnCallLayer, tz: Tz, at: DateTime<Utc>) -> Option<UserId> {
    let mut ps: Vec<&OnCallParticipant> = layer.participants.iter().collect();
    if ps.is_empty() {
        return None;
    }
    ps.sort_by_key(|p| p.position);
    let idx = rotation_index(layer, tz, at);
    let slot = idx.rem_euclid(ps.len() as i64) as usize;
    Some(ps[slot].user_id)
}

/// How many handoffs have elapsed since the anchor at `at` (0 before the
/// anchor). `daily`/`weekly` count whole local-calendar periods so a handoff
/// stays at the same wall-clock time across a DST transition; `custom` is a
/// fixed second interval.
fn rotation_index(layer: &OnCallLayer, tz: Tz, at: DateTime<Utc>) -> i64 {
    match layer.rotation_type {
        RotationType::Custom => {
            let secs = (at - layer.handoff_at).num_seconds();
            let len = i64::from(layer.rotation_length_secs).max(1);
            if secs < 0 { 0 } else { secs / len }
        }
        RotationType::Daily | RotationType::Weekly => {
            let period_days = (i64::from(layer.rotation_length_secs) / 86_400).max(1);
            let anchor_local = layer.handoff_at.with_timezone(&tz);
            let now_local = at.with_timezone(&tz);
            let anchor_date = anchor_local.date_naive();
            let now_date = now_local.date_naive();
            if now_date < anchor_date {
                return 0;
            }
            // Whether today's handoff has landed is decided by comparing real
            // instants, not naive wall-clock times: the handoff-of-today is the
            // anchor's time-of-day on `now`'s local date resolved to an actual
            // UTC instant. A naive time-of-day compare misranks by an hour
            // during the repeated fall-back hour.
            let handoff_today = local_to_utc(tz, now_date.and_time(anchor_local.time()));
            let mut days = (now_date - anchor_date).num_days();
            if at < handoff_today {
                days -= 1;
            }
            if days < 0 { 0 } else { days / period_days }
        }
    }
}

/// Resolve a local wall-clock time to a concrete UTC instant, DST-safe: on the
/// ambiguous fall-back hour take the earliest occurrence; in the spring-forward
/// gap (the wall time never happens) take the instant the clock jumps to.
fn local_to_utc(tz: Tz, naive: NaiveDateTime) -> DateTime<Utc> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt.to_utc(),
        LocalResult::Ambiguous(earliest, _) => earliest.to_utc(),
        LocalResult::None => {
            // Spring-forward gap: this wall time never occurs. Step forward to
            // the first instant that does (real gaps are <=1h; bound the search
            // so a pathological zone can't loop). Derive UTC from the zone's
            // offset as a last resort rather than mislabelling local as UTC.
            for mins in [60, 120, 180, 240] {
                if let Some(dt) = tz
                    .from_local_datetime(&(naive + ChronoDuration::minutes(mins)))
                    .earliest()
                {
                    return dt.to_utc();
                }
            }
            let offset = tz.offset_from_utc_datetime(&naive).fix();
            (naive - ChronoDuration::seconds(offset.local_minus_utc() as i64)).and_utc()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn schedule(tz: &str) -> OnCallSchedule {
        OnCallSchedule {
            id: Uuid::nil(),
            name: "primary".into(),
            timezone: tz.into(),
            created_at: t("2026-01-01T00:00:00Z"),
            updated_at: t("2026-01-01T00:00:00Z"),
        }
    }

    fn participant(user: UserId, position: i32) -> OnCallParticipant {
        OnCallParticipant {
            id: Uuid::now_v7(),
            user_id: user,
            position,
        }
    }

    fn layer(
        order: i32,
        rotation: RotationType,
        len_secs: i32,
        handoff: &str,
        participants: Vec<OnCallParticipant>,
    ) -> OnCallLayer {
        OnCallLayer {
            id: Uuid::now_v7(),
            name: None,
            rotation_type: rotation,
            rotation_length_secs: len_secs,
            handoff_at: t(handoff),
            layer_order: order,
            created_at: t("2026-01-01T00:00:00Z"),
            participants,
        }
    }

    #[test]
    fn empty_schedule_resolves_to_no_one() {
        let s = schedule("UTC");
        assert!(resolve_on_call(&s, &[], &[], t("2026-06-01T12:00:00Z")).is_empty());
        // A layer with no participants contributes nothing.
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T00:00:00Z",
            vec![],
        );
        assert!(resolve_on_call(&s, &[l], &[], t("2026-06-01T12:00:00Z")).is_empty());
    }

    #[test]
    fn daily_rotation_cycles_participants_each_day() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T09:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        // Before the first handoff time on day 0 → still participant 0.
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-01T08:59:00Z")),
            vec![uid(1)]
        );
        // After handoff on day 0 → participant 0.
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-01T09:00:00Z")),
            vec![uid(1)]
        );
        // Day 1 → participant 1; day 2 → back to 0.
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-02T10:00:00Z")),
            vec![uid(2)]
        );
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-06-03T10:00:00Z")),
            vec![uid(1)]
        );
    }

    #[test]
    fn handoff_boundary_is_inclusive_of_the_new_shift() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T09:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        // One second before the day-1 boundary → still participant 0.
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-02T08:59:59Z")),
            vec![uid(1)]
        );
        // Exactly at the boundary → participant 1 takes over.
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-06-02T09:00:00Z")),
            vec![uid(2)]
        );
    }

    #[test]
    fn daily_handoff_holds_wall_clock_time_across_dst() {
        // US spring-forward 2026: 02:00 → 03:00 on 2026-03-08 in New York.
        let s = schedule("America/New_York");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-03-06T09:00:00-05:00", // 09:00 local, before DST
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        // 2026-03-09 is 3 local days after the anchor → index 3 → participant 1,
        // even though a fixed 3*86400s would drift an hour past the 09:00 handoff
        // because the clocks sprang forward on the 8th.
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-03-09T13:30:00Z")), // 09:30 EDT
            vec![uid(2)]
        );
        // Just before the local 09:00 handoff on the 9th → still the prior day
        // (index 2 → participant 0).
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-03-09T12:30:00Z")), // 08:30 EDT
            vec![uid(1)]
        );
    }

    #[test]
    fn daily_handoff_does_not_regress_during_fall_back_hour() {
        // US fall-back 2026: 02:00 EDT → 01:00 EST on 2026-11-01, so 01:00–02:00
        // local happens twice. A handoff time-of-day inside that window used to
        // regress the rotation by one when compared naively.
        let s = schedule("America/New_York");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-10-30T01:30:00-04:00", // 01:30 local, anchor before the change
            vec![
                participant(uid(1), 0),
                participant(uid(2), 1),
                participant(uid(3), 2),
            ],
        );
        // 01:00 EDT (first 1am, before that day's 01:30 handoff) → index 1.
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-11-01T05:00:00Z")),
            vec![uid(2)]
        );
        // 01:15 EST (the repeated hour, AFTER the 01:30 handoff already passed) →
        // index 2, and must not slip back to index 1.
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-11-01T06:15:00Z")),
            vec![uid(3)]
        );
    }

    #[test]
    fn weekly_rotation_counts_seven_day_periods() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Weekly,
            7 * 86_400,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-04T00:00:00Z")),
            vec![uid(1)]
        );
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-08T00:00:00Z")),
            vec![uid(2)]
        );
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-06-15T00:00:00Z")),
            vec![uid(1)]
        );
    }

    #[test]
    fn custom_rotation_uses_fixed_seconds() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Custom,
            3_600, // hourly
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-01T00:30:00Z")),
            vec![uid(1)]
        );
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-01T01:00:00Z")),
            vec![uid(2)]
        );
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-06-01T02:00:00Z")),
            vec![uid(1)]
        );
    }

    #[test]
    fn before_the_anchor_holds_the_first_participant() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-05-01T00:00:00Z")),
            vec![uid(1)]
        );
    }

    #[test]
    fn participants_rotate_in_position_order_not_input_order() {
        let s = schedule("UTC");
        // Hand the participants over out of order; position drives rotation.
        let l = layer(
            0,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(2), 1), participant(uid(1), 0)],
        );
        assert_eq!(
            resolve_on_call(&s, std::slice::from_ref(&l), &[], t("2026-06-01T00:30:00Z")),
            vec![uid(1)]
        );
        assert_eq!(
            resolve_on_call(&s, &[l], &[], t("2026-06-01T01:30:00Z")),
            vec![uid(2)]
        );
    }

    #[test]
    fn higher_layer_order_wins_when_stacked() {
        let s = schedule("UTC");
        let base = layer(
            0,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        let top = layer(
            5,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(2), 0)],
        );
        // Order of the slice should not matter — the top layer wins.
        assert_eq!(
            resolve_on_call(
                &s,
                &[base.clone(), top.clone()],
                &[],
                t("2026-06-01T00:30:00Z")
            ),
            vec![uid(2)]
        );
        assert_eq!(
            resolve_on_call(&s, &[top, base], &[], t("2026-06-01T00:30:00Z")),
            vec![uid(2)]
        );
    }

    #[test]
    fn override_beats_the_rotation_in_its_window() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        let ov = OnCallOverride {
            id: Uuid::now_v7(),
            user_id: uid(9),
            starts_at: t("2026-06-01T00:00:00Z"),
            ends_at: t("2026-06-01T12:00:00Z"),
            created_by: None,
            created_at: t("2026-05-30T00:00:00Z"),
        };
        // Inside the window → the override user, ignoring the rotation.
        assert_eq!(
            resolve_on_call(
                &s,
                std::slice::from_ref(&l),
                std::slice::from_ref(&ov),
                t("2026-06-01T03:00:00Z")
            ),
            vec![uid(9)]
        );
        // The end is exclusive → rotation resumes at exactly ends_at.
        assert_eq!(
            resolve_on_call(&s, &[l], &[ov], t("2026-06-01T12:00:00Z")),
            vec![uid(1)]
        );
    }

    #[test]
    fn overlapping_overrides_union_their_users() {
        let s = schedule("UTC");
        let a = OnCallOverride {
            id: Uuid::now_v7(),
            user_id: uid(7),
            starts_at: t("2026-06-01T00:00:00Z"),
            ends_at: t("2026-06-01T06:00:00Z"),
            created_by: None,
            created_at: t("2026-05-30T00:00:00Z"),
        };
        let b = OnCallOverride {
            id: Uuid::now_v7(),
            user_id: uid(8),
            starts_at: t("2026-06-01T02:00:00Z"),
            ends_at: t("2026-06-01T08:00:00Z"),
            created_by: None,
            created_at: t("2026-05-30T00:00:00Z"),
        };
        let mut got = resolve_on_call(&s, &[], &[b, a], t("2026-06-01T03:00:00Z"));
        got.sort_by_key(|u| u.0);
        assert_eq!(got, vec![uid(7), uid(8)]);
    }

    #[test]
    fn db_str_roundtrips() {
        for r in RotationType::ALL {
            assert_eq!(RotationType::from_db_str(r.as_db_str()), *r);
        }
    }
}
