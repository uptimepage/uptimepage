//! On-call schedules + the pure who-is-on-call resolver.
//!
//! A schedule is a stack of rotation layers; the highest-order layer that has
//! participants determines the on-call user, and a one-off override beats the
//! computed rotation for its window. Who-is-on-call is never stored — the
//! escalation engine calls [`resolve_on_call`] at page time, and the calendar
//! walks [`on_call_shifts`] over the same rules. Both are referentially
//! transparent (no I/O) so rotation/DST/override math is exhaustively
//! unit-tested here; the store loads the rows and the engine maps the resolved
//! users to their contact channels.

use std::str::FromStr;

use chrono::{
    DateTime, Days, Duration as ChronoDuration, LocalResult, NaiveDateTime, Offset, TimeZone, Utc,
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
    /// Higher wins when layers are stacked; unique within the schedule.
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

impl OnCallSchedule {
    /// The zone its handoffs and days are read in. The API refuses an
    /// unknown name, so the UTC fallback only covers a zone the tz database
    /// has since dropped.
    pub fn tz(&self) -> Tz {
        Tz::from_str(&self.timezone).unwrap_or(Tz::UTC)
    }
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

impl OnCallScheduleDetail {
    /// Everyone the schedule can put on call, given the overrides it holds:
    /// the paging layer's participants and whoever covers an override. May
    /// repeat a person.
    pub fn responders(&self) -> impl Iterator<Item = UserId> + '_ {
        paging_layer(&self.layers)
            .into_iter()
            .flat_map(|l| l.participants.iter().map(|p| p.user_id))
            .chain(self.overrides.iter().map(|o| o.user_id))
    }
}

/// The layer that staffs a schedule outside overrides: the highest-order one
/// with participants. Every layer covers every instant, so the layers below
/// it never answer.
fn paging_layer(layers: &[OnCallLayer]) -> Option<&OnCallLayer> {
    layers
        .iter()
        .filter(|l| !l.participants.is_empty())
        .max_by_key(|l| l.layer_order)
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
    /// Higher wins when layers are stacked; no two layers may share one.
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
    /// A start already past is stored as the moment the override is added.
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

/// The years an on-call instant may name, well inside what the database
/// stores.
pub const FIRST_YEAR: i32 = 1970;
pub const LAST_YEAR: i32 = 9999;

// ── The pure resolver ────────────────────────────────────────────────────

/// Who is on call for `schedule` at `at`. Overrides covering the instant win
/// (their users, deduped, soonest start first); otherwise the highest-order
/// layer with participants supplies its current rotation slot. Empty when
/// nothing covers the instant.
///
/// Pure: no I/O. `layers`/`overrides` need not be pre-sorted — the resolver
/// orders them itself so callers can hand over rows in any order.
pub fn resolve_on_call(
    schedule: &OnCallSchedule,
    layers: &[OnCallLayer],
    overrides: &[OnCallOverride],
    at: DateTime<Utc>,
) -> Vec<UserId> {
    Rota::new(schedule, layers, overrides, at).at(at).0
}

/// A stretch of time over which [`resolve_on_call`] gives one answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnCallShift {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    /// Empty when no one covers the stretch.
    pub user_ids: Vec<UserId>,
    /// Overrides put `user_ids` there, not the rotation.
    pub overridden: bool,
}

impl OnCallShift {
    /// Whether `other` is held by the same people, in whatever order.
    pub fn same_people(&self, other: &Self) -> bool {
        same_people(&self.user_ids, &other.user_ids)
    }
}

/// Both sides are deduped, so matching lengths and containment is equality.
fn same_people(a: &[UserId], b: &[UserId]) -> bool {
    a.len() == b.len() && a.iter().all(|u| b.contains(u))
}

/// The shifts that tile `[from, to)` in order, each as long as the same
/// people hold it the same way, so consecutive shifts always differ.
pub fn on_call_shifts<'a>(
    schedule: &OnCallSchedule,
    layers: &'a [OnCallLayer],
    overrides: &'a [OnCallOverride],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> impl Iterator<Item = OnCallShift> + 'a {
    let rota = Rota::new(schedule, layers, overrides, from);
    let edges = rota.edges();
    let mut t = from;
    // The answer at `t` when the previous shift already resolved it.
    let mut ahead = None;
    std::iter::from_fn(move || {
        if t >= to {
            return None;
        }
        let starts_at = t;
        let answer = ahead.take().unwrap_or_else(|| rota.at(t));
        loop {
            t = rota.next_change(&edges, t).map_or(to, |next| next.min(to));
            if t >= to {
                break;
            }
            let next = rota.at(t);
            if next.1 != answer.1 || !same_people(&next.0, &answer.0) {
                ahead = Some(next);
                break;
            }
        }
        let (user_ids, overridden) = answer;
        Some(OnCallShift {
            starts_at,
            ends_at: t,
            user_ids,
            overridden,
        })
    })
}

/// A schedule made ready to resolve: its overrides, and the one layer that
/// staffs it outside them.
struct Rota<'a> {
    tz: Tz,
    /// The paging layer, and its participants in rotation order.
    top: Option<(&'a OnCallLayer, Vec<UserId>)>,
    /// Those not over by the first instant asked about, soonest start first:
    /// the order their people are paged in.
    overrides: Vec<&'a OnCallOverride>,
}

impl<'a> Rota<'a> {
    /// Ready to answer for `from` on.
    fn new(
        schedule: &OnCallSchedule,
        layers: &'a [OnCallLayer],
        overrides: &'a [OnCallOverride],
        from: DateTime<Utc>,
    ) -> Self {
        let tz = schedule.tz();
        let top = paging_layer(layers).map(|l| {
            let mut ps: Vec<&OnCallParticipant> = l.participants.iter().collect();
            ps.sort_by_key(|p| p.position);
            (l, ps.into_iter().map(|p| p.user_id).collect())
        });
        let mut overrides: Vec<&OnCallOverride> =
            overrides.iter().filter(|o| o.ends_at > from).collect();
        overrides.sort_by_key(|o| (o.starts_at, o.created_at, o.id));
        Self { tz, top, overrides }
    }

    /// Every override start and end, sorted and deduped: where a walk may
    /// change hands besides a handoff.
    fn edges(&self) -> Vec<DateTime<Utc>> {
        let mut edges: Vec<DateTime<Utc>> = self
            .overrides
            .iter()
            .flat_map(|o| [o.starts_at, o.ends_at])
            .collect();
        edges.sort_unstable();
        edges.dedup();
        edges
    }

    fn covering(&self, at: DateTime<Utc>) -> impl Iterator<Item = &'a OnCallOverride> + '_ {
        let started = self.overrides.partition_point(|o| o.starts_at <= at);
        self.overrides[..started]
            .iter()
            .copied()
            .filter(move |o| at < o.ends_at)
    }

    /// Who is on call at `at`, and whether overrides put them there.
    fn at(&self, at: DateTime<Utc>) -> (Vec<UserId>, bool) {
        let mut out: Vec<UserId> = Vec::new();
        for o in self.covering(at) {
            if !out.contains(&o.user_id) {
                out.push(o.user_id);
            }
        }
        if !out.is_empty() {
            return (out, true);
        }
        let rotation = self.top.as_ref().map(|(layer, ps)| {
            let slot = rotation_index(layer, self.tz, at).rem_euclid(ps.len() as i64) as usize;
            vec![ps[slot]]
        });
        (rotation.unwrap_or_default(), false)
    }

    /// The first instant after `after` at which [`Self::at`] may answer
    /// differently, given [`Self::edges`]; `None` when it never will.
    fn next_change(&self, edges: &[DateTime<Utc>], after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // A rotation of one person, however often listed, never hands off,
        // and one hidden under an override changes nothing until it ends.
        let handoff = self
            .top
            .as_ref()
            .filter(|(_, ps)| ps.iter().any(|u| *u != ps[0]))
            .filter(|_| self.covering(after).next().is_none())
            .and_then(|(layer, _)| next_handoff(layer, self.tz, after));
        let edge = edges.get(edges.partition_point(|e| *e <= after)).copied();
        edge.into_iter().chain(handoff).filter(|t| *t > after).min()
    }
}

/// How many handoffs have elapsed since the anchor at `at` (0 before the
/// anchor). `daily`/`weekly` count whole local-calendar periods so a handoff
/// stays at the same wall-clock time across a DST transition; `custom` is a
/// fixed second interval.
fn rotation_index(layer: &OnCallLayer, tz: Tz, at: DateTime<Utc>) -> i64 {
    match layer.rotation_type {
        RotationType::Custom => {
            let secs = (at - layer.handoff_at).num_seconds();
            if secs < 0 {
                0
            } else {
                secs / custom_secs(layer)
            }
        }
        RotationType::Daily | RotationType::Weekly => {
            let anchor = layer.handoff_at.with_timezone(&tz);
            let anchor_date = anchor.date_naive();
            // A day's handoff has landed once its real instant has passed:
            // the anchor's time of day on that local date, resolved as the
            // walk resolves it. Naive wall-clock compares misrank the repeated
            // fall-back hour, and a spring-forward gap can push a late
            // handoff past midnight, so step back until one is behind `at`.
            let handoff = |days: i64| {
                anchor_date
                    .checked_add_days(Days::new(days.unsigned_abs()))
                    .map(|d| local_to_utc(tz, d.and_time(anchor.time())))
            };
            let mut days = (at.with_timezone(&tz).date_naive() - anchor_date).num_days();
            while days >= 0 && handoff(days).is_none_or(|h| at < h) {
                days -= 1;
            }
            if days < 0 {
                0
            } else {
                days / period_days(layer)
            }
        }
    }
}

/// The handoff that ends the rotation slot holding `after`, resolved the same
/// way [`rotation_index`] counts them. `None` past the representable range.
fn next_handoff(layer: &OnCallLayer, tz: Tz, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let next = rotation_index(layer, tz, after) + 1;
    match layer.rotation_type {
        RotationType::Custom => {
            let secs = next.checked_mul(custom_secs(layer))?;
            layer
                .handoff_at
                .checked_add_signed(ChronoDuration::try_seconds(secs)?)
        }
        RotationType::Daily | RotationType::Weekly => {
            let anchor = layer.handoff_at.with_timezone(&tz);
            let days = u64::try_from(next.checked_mul(period_days(layer))?).ok()?;
            let date = anchor.date_naive().checked_add_days(Days::new(days))?;
            Some(local_to_utc(tz, date.and_time(anchor.time())))
        }
    }
}

fn custom_secs(layer: &OnCallLayer) -> i64 {
    i64::from(layer.rotation_length_secs).max(1)
}

fn period_days(layer: &OnCallLayer) -> i64 {
    (i64::from(layer.rotation_length_secs) / 86_400).max(1)
}

/// Resolve a local wall-clock time to a concrete UTC instant, DST-safe: on the
/// ambiguous fall-back hour take the earliest occurrence; in the spring-forward
/// gap (the wall time never happens) read it with the offset in force before
/// the jump, so it lands as far past the jump as it fell into the gap.
pub fn local_to_utc(tz: Tz, naive: NaiveDateTime) -> DateTime<Utc> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt.to_utc(),
        LocalResult::Ambiguous(earliest, _) => earliest.to_utc(),
        LocalResult::None => {
            let before = tz
                .offset_from_utc_datetime(&(naive - ChronoDuration::days(1)))
                .fix();
            (naive - ChronoDuration::seconds(i64::from(before.local_minus_utc()))).and_utc()
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
        assert_eq!(
            resolve_on_call(&s, &[], &[b, a], t("2026-06-01T03:00:00Z")),
            vec![uid(7), uid(8)]
        );
    }

    #[test]
    fn override_people_are_paged_soonest_start_first() {
        let s = schedule("UTC");
        let first = cover(uid(8), "2026-06-01T00:00:00Z", "2026-06-01T06:00:00Z");
        let second = cover(uid(7), "2026-06-01T02:00:00Z", "2026-06-01T08:00:00Z");
        let at = t("2026-06-01T03:00:00Z");
        assert_eq!(
            resolve_on_call(&s, &[], &[second.clone(), first.clone()], at),
            vec![uid(8), uid(7)]
        );
        assert_eq!(
            resolve_on_call(&s, &[], &[first, second], at),
            vec![uid(8), uid(7)]
        );
    }

    #[test]
    fn the_same_people_by_other_overrides_are_one_shift() {
        let s = schedule("UTC");
        let got = shifts(
            &s,
            &[],
            &[
                cover(uid(8), "2026-06-01T00:00:00Z", "2026-06-03T00:00:00Z"),
                cover(uid(8), "2026-06-03T00:00:00Z", "2026-06-05T00:00:00Z"),
                cover(uid(7), "2026-06-02T00:00:00Z", "2026-06-04T00:00:00Z"),
            ],
            "2026-06-01T00:00:00Z",
            "2026-06-05T00:00:00Z",
        );
        let people: Vec<_> = got.iter().map(|x| x.user_ids.clone()).collect();
        assert_eq!(
            people,
            vec![vec![uid(8)], vec![uid(8), uid(7)], vec![uid(8)]]
        );
    }

    #[test]
    fn a_rotation_under_an_override_is_not_walked() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        let ov = [cover(
            uid(9),
            "2026-06-01T00:00:00Z",
            "2026-12-01T00:00:00Z",
        )];
        let from = t("2026-06-01T00:30:00Z");
        let rota = Rota::new(&s, std::slice::from_ref(&l), &ov, from);
        assert_eq!(
            rota.next_change(&rota.edges(), from),
            Some(t("2026-12-01T00:00:00Z"))
        );
    }

    fn shifts(
        s: &OnCallSchedule,
        layers: &[OnCallLayer],
        overrides: &[OnCallOverride],
        from: &str,
        to: &str,
    ) -> Vec<OnCallShift> {
        on_call_shifts(s, layers, overrides, t(from), t(to)).collect()
    }

    fn cover(user: UserId, from: &str, to: &str) -> OnCallOverride {
        OnCallOverride {
            id: Uuid::now_v7(),
            user_id: user,
            starts_at: t(from),
            ends_at: t(to),
            created_by: None,
            created_at: t("2026-01-01T00:00:00Z"),
        }
    }

    #[test]
    fn shifts_hand_off_at_the_local_time_across_dst() {
        let s = schedule("America/New_York");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-03-06T09:00:00-05:00",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        let got = shifts(
            &s,
            &[l],
            &[],
            "2026-03-07T00:00:00Z",
            "2026-03-10T00:00:00Z",
        );
        let starts: Vec<_> = got.iter().map(|x| x.starts_at).collect();
        assert_eq!(
            starts,
            vec![
                t("2026-03-07T00:00:00Z"),
                t("2026-03-07T14:00:00Z"), // 09:00 EST
                t("2026-03-08T13:00:00Z"), // 09:00 EDT, the clocks sprang forward
                t("2026-03-09T13:00:00Z"),
            ]
        );
        assert_eq!(got[0].user_ids, vec![uid(1)]);
        assert_eq!(got[1].user_ids, vec![uid(2)]);
        assert_eq!(got[3].ends_at, t("2026-03-10T00:00:00Z"));
    }

    #[test]
    fn a_lone_participant_is_one_shift() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T09:00:00Z",
            vec![participant(uid(1), 0)],
        );
        let got = shifts(
            &s,
            &[l],
            &[],
            "2026-06-01T00:00:00Z",
            "2027-06-01T00:00:00Z",
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].ends_at, t("2027-06-01T00:00:00Z"));
    }

    #[test]
    fn one_person_listed_twice_is_one_shift() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0), participant(uid(1), 1)],
        );
        let got = shifts(
            &s,
            &[l],
            &[],
            "2026-06-01T00:00:00Z",
            "2027-06-01T00:00:00Z",
        );
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn an_unknown_zone_reads_as_utc() {
        assert_eq!(schedule("Mars/Phobos").tz(), Tz::UTC);
        assert_eq!(schedule("Europe/Kyiv").tz(), chrono_tz::Europe::Kyiv);
    }

    #[test]
    fn an_override_splits_the_rotation_and_is_marked() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        let ov = cover(uid(9), "2026-06-02T06:00:00Z", "2026-06-02T18:00:00Z");
        let got = shifts(
            &s,
            &[l],
            &[ov],
            "2026-06-02T00:00:00Z",
            "2026-06-03T00:00:00Z",
        );
        let brief: Vec<_> = got
            .iter()
            .map(|x| (x.starts_at, x.user_ids.clone(), x.overridden))
            .collect();
        assert_eq!(
            brief,
            vec![
                (t("2026-06-02T00:00:00Z"), vec![uid(1)], false),
                (t("2026-06-02T06:00:00Z"), vec![uid(9)], true),
                (t("2026-06-02T18:00:00Z"), vec![uid(1)], false),
            ]
        );
    }

    #[test]
    fn an_override_by_the_person_on_shift_still_shows() {
        let s = schedule("UTC");
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        let ov = cover(uid(1), "2026-06-02T06:00:00Z", "2026-06-02T18:00:00Z");
        let got = shifts(
            &s,
            &[l],
            &[ov],
            "2026-06-02T00:00:00Z",
            "2026-06-03T00:00:00Z",
        );
        assert_eq!(got.len(), 3);
        assert!(got[1].overridden);
    }

    #[test]
    fn an_empty_schedule_is_one_uncovered_shift() {
        let got = shifts(
            &schedule("UTC"),
            &[],
            &[],
            "2026-06-01T00:00:00Z",
            "2026-06-08T00:00:00Z",
        );
        assert_eq!(got.len(), 1);
        assert!(got[0].user_ids.is_empty());
        assert!(!got[0].overridden);
    }

    /// Every instant of every shift resolves to that shift's users, the shifts
    /// tile the window, and neighbours differ.
    #[test]
    fn shifts_agree_with_the_resolver_everywhere() {
        let rotas: Vec<(&str, Vec<OnCallLayer>, Vec<OnCallOverride>)> = vec![
            (
                "America/New_York",
                vec![layer(
                    0,
                    RotationType::Daily,
                    86_400,
                    "2026-10-30T01:30:00-04:00",
                    vec![
                        participant(uid(1), 0),
                        participant(uid(2), 1),
                        participant(uid(3), 2),
                    ],
                )],
                vec![cover(
                    uid(9),
                    "2026-11-01T04:00:00Z",
                    "2026-11-01T07:00:00Z",
                )],
            ),
            (
                "Europe/Kyiv",
                vec![
                    layer(
                        0,
                        RotationType::Custom,
                        43_200,
                        "2026-10-20T08:00:00Z",
                        vec![participant(uid(1), 0), participant(uid(2), 1)],
                    ),
                    layer(
                        3,
                        RotationType::Weekly,
                        14 * 86_400,
                        "2026-10-19T09:00:00+03:00",
                        vec![participant(uid(4), 0), participant(uid(5), 1)],
                    ),
                ],
                vec![
                    cover(uid(7), "2026-10-26T00:00:00Z", "2026-10-28T00:00:00Z"),
                    cover(uid(8), "2026-10-27T00:00:00Z", "2026-10-29T00:00:00Z"),
                ],
            ),
            (
                "Australia/Lord_Howe",
                vec![layer(
                    0,
                    RotationType::Daily,
                    2 * 86_400,
                    "2026-09-30T02:15:00+10:30",
                    vec![participant(uid(1), 0), participant(uid(2), 1)],
                )],
                vec![],
            ),
        ];
        let from = t("2026-09-28T00:00:00Z");
        let to = t("2026-11-10T00:00:00Z");
        for (tz, layers, overrides) in rotas {
            assert_shifts_agree(tz, &layers, &overrides, from, to);
        }
    }

    #[test]
    fn a_handoff_in_a_gap_across_midnight_lands_where_the_walk_says() {
        // Nuuk skips 23:00-23:59 on Mar 28, so that night's 23:30 handoff
        // happens at 00:30 on the 29th.
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-03-20T23:30:00-02:00",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        assert_shifts_agree(
            "America/Nuuk",
            &[l],
            &[],
            t("2026-03-27T00:00:00Z"),
            t("2026-03-31T00:00:00Z"),
        );
    }

    /// The shifts tile `[from, to)`, neighbours differ, and every instant in
    /// a shift resolves to its people.
    fn assert_shifts_agree(
        tz: &str,
        layers: &[OnCallLayer],
        overrides: &[OnCallOverride],
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) {
        let s = schedule(tz);
        let got: Vec<OnCallShift> = on_call_shifts(&s, layers, overrides, from, to).collect();
        assert_eq!(got.first().unwrap().starts_at, from, "{tz}");
        assert_eq!(got.last().unwrap().ends_at, to, "{tz}");
        for pair in got.windows(2) {
            assert_eq!(pair[0].ends_at, pair[1].starts_at, "{tz}");
            assert!(
                !pair[0].same_people(&pair[1]) || pair[0].overridden != pair[1].overridden,
                "{tz}: neighbours at {} are equal",
                pair[1].starts_at
            );
        }
        for shift in &got {
            let mut at = shift.starts_at;
            while at < shift.ends_at {
                assert!(
                    same_people(&resolve_on_call(&s, layers, overrides, at), &shift.user_ids),
                    "{tz} at {at}"
                );
                at += ChronoDuration::minutes(15);
            }
            let last = shift.ends_at - ChronoDuration::seconds(1);
            assert!(
                same_people(
                    &resolve_on_call(&s, layers, overrides, last),
                    &shift.user_ids
                ),
                "{tz} at {last}"
            );
        }
    }

    #[test]
    fn a_time_in_a_gap_lands_as_far_past_the_jump() {
        let at = |date: &str| NaiveDateTime::parse_from_str(date, "%Y-%m-%d %H:%M").unwrap();
        // New York jumps 02:00 to 03:00; 02:30 reads as 03:30.
        assert_eq!(
            local_to_utc(chrono_tz::America::New_York, at("2026-03-08 02:30")),
            t("2026-03-08T07:30:00Z")
        );
        // Lord Howe jumps 02:00 to 02:30; 02:15 reads as 02:45.
        assert_eq!(
            local_to_utc(chrono_tz::Australia::Lord_Howe, at("2026-10-04 02:15")),
            t("2026-10-03T15:45:00Z")
        );
    }

    #[test]
    fn db_str_roundtrips() {
        for r in RotationType::ALL {
            assert_eq!(RotationType::from_db_str(r.as_db_str()), *r);
        }
    }
}
