//! On-call schedules + the pure who-is-on-call resolver.
//!
//! A schedule is an ordered list of rotation layers, each on call at all
//! hours or only in its weekly windows; the first layer on call at an instant
//! determines the on-call user, and a one-off override beats the computed
//! rotation for its window. Who-is-on-call is never stored — the
//! escalation engine calls [`resolve_on_call`] at page time, and the calendar
//! walks [`on_call_shifts`] over the same rules. Both are referentially
//! transparent (no I/O) so rotation/DST/override math is exhaustively
//! unit-tested here; the store loads the rows and the engine maps the resolved
//! users to their contact channels.

use std::ops::Range;
use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Days, Duration as ChronoDuration, LocalResult, NaiveDate, NaiveDateTime,
    NaiveTime, Offset, TimeZone, Timelike, Utc,
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

/// A day of the week, as a window names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    pub const ALL: [Self; 7] = [
        Self::Mon,
        Self::Tue,
        Self::Wed,
        Self::Thu,
        Self::Fri,
        Self::Sat,
        Self::Sun,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mon => "mon",
            Self::Tue => "tue",
            Self::Wed => "wed",
            Self::Thu => "thu",
            Self::Fri => "fri",
            Self::Sat => "sat",
            Self::Sun => "sun",
        }
    }

    fn of(date: NaiveDate) -> Self {
        Self::ALL[date.weekday().num_days_from_monday() as usize]
    }
}

/// When a layer is on call: from `from` on each of `days` until `to`, in the
/// schedule's timezone. A `to` at or before `from` runs into the next day, so
/// the same time at both ends covers a whole day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OnCallWindow {
    /// One or more days, each named once.
    pub days: Vec<Weekday>,
    #[serde(with = "clock")]
    #[schema(value_type = String, example = "09:00")]
    pub from: NaiveTime,
    #[serde(with = "clock")]
    #[schema(value_type = String, example = "17:00")]
    pub to: NaiveTime,
}

impl OnCallWindow {
    /// How long it stays open on the wall clock: up to `to`, past midnight
    /// when that comes first, and a whole day when the two meet.
    fn length(&self) -> ChronoDuration {
        let len = self.to - self.from;
        if len > ChronoDuration::zero() {
            len
        } else {
            len + ChronoDuration::days(1)
        }
    }

    /// The stretch it opens on `date`, as instants. `None` when `date` is not
    /// one of its days, or a daylight-saving jump leaves nothing of it.
    fn on(&self, tz: Tz, date: NaiveDate) -> Option<Range<DateTime<Utc>>> {
        if !self.days.contains(&Weekday::of(date)) {
            return None;
        }
        let opens = date.and_time(self.from);
        let open = local_to_utc(tz, opens);
        let close = local_to_utc(tz, opens.checked_add_signed(self.length())?);
        (open < close).then_some(open..close)
    }
}

const WEEK_MINUTES: usize = 7 * 24 * 60;

/// The minutes of the week, Monday 00:00 first, that `windows` keep a layer
/// on call on the wall clock; no windows means every one.
fn week_minutes(windows: &[OnCallWindow]) -> Vec<bool> {
    if windows.is_empty() {
        return vec![true; WEEK_MINUTES];
    }
    let mut on = vec![false; WEEK_MINUTES];
    for w in windows {
        let from = (w.from.hour() * 60 + w.from.minute()) as usize;
        let len = w.length().num_minutes() as usize;
        for d in &w.days {
            let start = *d as usize * 24 * 60 + from;
            for m in start..start + len {
                on[m % WEEK_MINUTES] = true;
            }
        }
    }
    on
}

/// A layer that never pages, by its place in the order layers are asked.
#[derive(Debug, PartialEq, Eq)]
pub struct Shadowed {
    pub layer: usize,
    /// An earlier layer with no hours, so on call at all of them, when giving
    /// it hours could let this one page: the layers with hours before it do
    /// not already cover this one.
    pub behind: Option<usize>,
    /// The layers before it cover the whole week, so no hours of its own
    /// would let it page.
    pub whole_week: bool,
}

/// The first layer in `stack`, each layer's windows in the order layers are
/// asked, whose hours the layers before it already cover, so it never pages.
pub fn never_pages(stack: &[&[OnCallWindow]]) -> Option<Shadowed> {
    let mut covered = vec![false; WEEK_MINUTES];
    let mut by_hours = vec![false; WEEK_MINUTES];
    let mut all_hours = None;
    let within = |mine: &[bool], cover: &[bool]| mine.iter().zip(cover).all(|(m, c)| !m || *c);
    for (i, windows) in stack.iter().enumerate() {
        let mine = week_minutes(windows);
        if within(&mine, &covered) {
            return Some(Shadowed {
                layer: i,
                behind: all_hours.filter(|_| !within(&mine, &by_hours)),
                whole_week: covered.iter().all(|c| *c),
            });
        }
        if windows.is_empty() {
            all_hours = Some(i);
        } else {
            by_hours.iter_mut().zip(&mine).for_each(|(c, m)| *c |= *m);
        }
        covered.iter_mut().zip(mine).for_each(|(c, m)| *c |= m);
    }
    None
}

/// `09:00`: a time of day to the minute.
mod clock {
    use chrono::NaiveTime;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(at: &NaiveTime, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&at.format("%H:%M"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveTime, D::Error> {
        let raw = String::deserialize(d)?;
        NaiveTime::parse_from_str(&raw, "%H:%M")
            .map_err(|_| D::Error::custom(format!("expected a time like 09:00, got {raw:?}")))
    }
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
    /// Its place in the schedule, unique within it; lower is asked first.
    pub layer_order: i32,
    /// When it is on call; empty means at all hours. Its rotation keeps
    /// counting outside them.
    pub windows: Vec<OnCallWindow>,
    pub created_at: DateTime<Utc>,
    pub participants: Vec<OnCallParticipant>,
}

impl OnCallLayer {
    /// When it is on call over `[from, to]`, merged and in order; `None` at
    /// all hours. A daylight-saving jump only ever pushes a window later, so
    /// one opened two days before `from` is the oldest that can still be open.
    fn hours(
        &self,
        tz: Tz,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Option<Vec<Range<DateTime<Utc>>>> {
        if self.windows.is_empty() {
            return None;
        }
        let first = from.with_timezone(&tz).date_naive();
        let first = first.checked_sub_days(Days::new(2)).unwrap_or(first);
        let last = to.with_timezone(&tz).date_naive();
        let mut spans: Vec<Range<DateTime<Utc>>> = first
            .iter_days()
            .take_while(|d| *d <= last)
            .flat_map(|d| self.windows.iter().filter_map(move |w| w.on(tz, d)))
            .collect();
        spans.sort_unstable_by_key(|r| r.start);
        let mut merged: Vec<Range<DateTime<Utc>>> = Vec::with_capacity(spans.len());
        for r in spans {
            match merged.last_mut() {
                Some(prev) if r.start <= prev.end => prev.end = prev.end.max(r.end),
                _ => merged.push(r),
            }
        }
        Some(merged)
    }
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
    /// every layer's participants and whoever covers an override. May repeat
    /// a person.
    pub fn responders(&self) -> impl Iterator<Item = UserId> + '_ {
        self.layers
            .iter()
            .flat_map(|l| l.participants.iter().map(|p| p.user_id))
            .chain(self.overrides.iter().map(|o| o.user_id))
    }
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
    /// Its place in the schedule; lower is asked first, and no two layers
    /// may share one.
    #[serde(default)]
    pub layer_order: i32,
    /// When it is on call, up to 14 windows; empty or left out means at all
    /// hours. A layer whose hours the layers before it already cover is
    /// refused, since it would never page.
    #[serde(default)]
    pub windows: Vec<OnCallWindow>,
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
/// (their users, deduped, soonest start first); otherwise the first layer,
/// by `layer_order`, that has participants and is on call at the instant
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
    Rota::new(schedule, layers, overrides, at, at).at(at).0
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
    let rota = Rota::new(schedule, layers, overrides, from, to);
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

/// The stretches of `[from, to)` that `user` is on call for, each without a
/// break: who else is on alongside them, and why, does not split one.
pub fn shifts_held_by<'a>(
    schedule: &OnCallSchedule,
    layers: &'a [OnCallLayer],
    overrides: &'a [OnCallOverride],
    user: UserId,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> impl Iterator<Item = Range<DateTime<Utc>>> + 'a {
    let mut held = on_call_shifts(schedule, layers, overrides, from, to)
        .filter(move |s| s.user_ids.contains(&user))
        .map(|s| s.starts_at..s.ends_at)
        .peekable();
    std::iter::from_fn(move || {
        let mut stretch = held.next()?;
        while let Some(next) = held.next_if(|n| n.start == stretch.end) {
            stretch.end = next.end;
        }
        Some(stretch)
    })
}

/// A layer with participants, ready to answer.
struct Staffed<'a> {
    layer: &'a OnCallLayer,
    /// Its participants in rotation order.
    people: Vec<UserId>,
    /// [`OnCallLayer::hours`] over the stretch asked about.
    hours: Option<Vec<Range<DateTime<Utc>>>>,
}

impl Staffed<'_> {
    fn on_call_at(&self, at: DateTime<Utc>) -> bool {
        self.hours.as_ref().is_none_or(|h| {
            h.get(h.partition_point(|r| r.end <= at))
                .is_some_and(|r| r.start <= at)
        })
    }

    /// The next instant after `after` at which it comes on or goes off;
    /// `None` when it does not within the stretch asked about.
    fn next_window_edge(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let h = self.hours.as_ref()?;
        let r = h.get(h.partition_point(|r| r.end <= after))?;
        Some(if r.start > after { r.start } else { r.end })
    }
}

/// A schedule made ready to resolve: its overrides, and the layers that
/// staff it outside them.
struct Rota<'a> {
    tz: Tz,
    /// Layers with participants, first asked first.
    layers: Vec<Staffed<'a>>,
    /// Those not over by the first instant asked about, soonest start first:
    /// the order their people are paged in.
    overrides: Vec<&'a OnCallOverride>,
}

impl<'a> Rota<'a> {
    /// Ready to answer for `[from, to]`.
    fn new(
        schedule: &OnCallSchedule,
        layers: &'a [OnCallLayer],
        overrides: &'a [OnCallOverride],
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Self {
        let tz = schedule.tz();
        let mut staffed: Vec<&OnCallLayer> = layers
            .iter()
            .filter(|l| !l.participants.is_empty())
            .collect();
        staffed.sort_by_key(|l| (l.layer_order, l.id));
        let layers = staffed
            .into_iter()
            .map(|layer| {
                let mut ps: Vec<&OnCallParticipant> = layer.participants.iter().collect();
                ps.sort_by_key(|p| p.position);
                Staffed {
                    layer,
                    people: ps.into_iter().map(|p| p.user_id).collect(),
                    hours: layer.hours(tz, from, to),
                }
            })
            .collect();
        let mut overrides: Vec<&OnCallOverride> =
            overrides.iter().filter(|o| o.ends_at > from).collect();
        overrides.sort_by_key(|o| (o.starts_at, o.created_at, o.id));
        Self {
            tz,
            layers,
            overrides,
        }
    }

    /// Every override start and end, sorted and deduped: where a walk may
    /// change hands besides a handoff or a window.
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

    /// Where in [`Self::layers`] the layer on call at `at` sits.
    fn answering(&self, at: DateTime<Utc>) -> Option<usize> {
        self.layers.iter().position(|l| l.on_call_at(at))
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
        let rotation = self.answering(at).map(|i| {
            let Staffed { layer, people, .. } = &self.layers[i];
            let slot = rotation_index(layer, self.tz, at).rem_euclid(people.len() as i64) as usize;
            vec![people[slot]]
        });
        (rotation.unwrap_or_default(), false)
    }

    /// The first instant after `after` at which [`Self::at`] may answer
    /// differently, given [`Self::edges`]; `None` when it never will.
    fn next_change(&self, edges: &[DateTime<Utc>], after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let edge = edges.get(edges.partition_point(|e| *e <= after)).copied();
        // Layers hidden under an override change nothing until it ends.
        if self.covering(after).next().is_some() {
            return edge;
        }
        // Only the answering layer's windows and those of the layers asked
        // before it can hand the instant to another; a rotation of one
        // person, however often listed, never hands off.
        let answering = self.answering(after);
        let asked = answering.map_or(self.layers.len(), |i| i + 1);
        let windows = self.layers[..asked]
            .iter()
            .filter_map(|l| l.next_window_edge(after));
        let handoff = answering
            .map(|i| &self.layers[i])
            .filter(|l| l.people.iter().any(|u| *u != l.people[0]))
            .and_then(|l| next_handoff(l.layer, self.tz, after));
        edge.into_iter()
            .chain(windows)
            .chain(handoff)
            .filter(|t| *t > after)
            .min()
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
            windows: vec![],
            created_at: t("2026-01-01T00:00:00Z"),
            participants,
        }
    }

    fn window(days: &[Weekday], from: &str, to: &str) -> OnCallWindow {
        let at = |s: &str| NaiveTime::parse_from_str(s, "%H:%M").unwrap();
        OnCallWindow {
            days: days.to_vec(),
            from: at(from),
            to: at(to),
        }
    }

    const WEEKDAYS: [Weekday; 5] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
    ];

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
    fn the_first_layer_on_call_wins() {
        let s = schedule("UTC");
        let first = layer(
            0,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        let second = layer(
            5,
            RotationType::Custom,
            3_600,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(2), 0)],
        );
        // Order of the slice should not matter, only layer_order.
        assert_eq!(
            resolve_on_call(
                &s,
                &[first.clone(), second.clone()],
                &[],
                t("2026-06-01T00:30:00Z")
            ),
            vec![uid(1)]
        );
        assert_eq!(
            resolve_on_call(&s, &[second, first], &[], t("2026-06-01T00:30:00Z")),
            vec![uid(1)]
        );
    }

    /// Weekday working hours on the first layer, everyone else's time on the
    /// second.
    fn days_and_nights() -> Vec<OnCallLayer> {
        let mut days = layer(
            0,
            RotationType::Weekly,
            7 * 86_400,
            "2026-06-01T09:00:00+03:00",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        days.windows = vec![window(&WEEKDAYS, "09:00", "17:00")];
        let rest = layer(
            1,
            RotationType::Daily,
            86_400,
            "2026-06-01T17:00:00+03:00",
            vec![participant(uid(3), 0), participant(uid(4), 1)],
        );
        vec![rest, days]
    }

    #[test]
    fn a_layer_steps_aside_outside_its_windows() {
        let s = schedule("Europe/Kyiv");
        let layers = days_and_nights();
        let at = |i: &str| resolve_on_call(&s, &layers, &[], t(i));
        // Mon 2026-06-01 10:00 local: the working-hours layer.
        assert_eq!(at("2026-06-01T07:00:00Z"), vec![uid(1)]);
        // Its window ends at 17:00 local, exclusive.
        assert_eq!(at("2026-06-01T13:59:59Z"), vec![uid(1)]);
        assert_eq!(at("2026-06-01T14:00:00Z"), vec![uid(3)]);
        // Before 09:00 local on Tuesday: still the evening's person.
        assert_eq!(at("2026-06-02T05:59:00Z"), vec![uid(3)]);
        // Saturday evening falls through, the rest layer's rotation having
        // kept counting.
        assert_eq!(at("2026-06-06T15:00:00Z"), vec![uid(4)]);
        // The next week the working-hours rotation has moved on.
        assert_eq!(at("2026-06-08T07:00:00Z"), vec![uid(2)]);
    }

    #[test]
    fn hours_no_layer_covers_are_no_one_s() {
        let s = schedule("UTC");
        let mut only = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        only.windows = vec![window(&[Weekday::Sat, Weekday::Sun], "00:00", "00:00")];
        let got = shifts(
            &s,
            &[only],
            &[],
            "2026-06-05T00:00:00Z",
            "2026-06-09T00:00:00Z",
        );
        let brief: Vec<_> = got
            .iter()
            .map(|x| (x.starts_at, x.user_ids.clone()))
            .collect();
        assert_eq!(
            brief,
            vec![
                (t("2026-06-05T00:00:00Z"), vec![]),
                (t("2026-06-06T00:00:00Z"), vec![uid(1)]),
                (t("2026-06-08T00:00:00Z"), vec![]),
            ]
        );
    }

    #[test]
    fn a_window_ending_before_it_starts_runs_overnight() {
        let s = schedule("UTC");
        let mut nights = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        nights.windows = vec![window(&[Weekday::Fri], "22:00", "06:00")];
        let at = |i: &str| resolve_on_call(&s, std::slice::from_ref(&nights), &[], t(i));
        assert!(at("2026-06-05T21:59:00Z").is_empty());
        assert_eq!(at("2026-06-05T22:00:00Z"), vec![uid(1)]);
        // Saturday morning belongs to Friday's window.
        assert_eq!(at("2026-06-06T05:59:00Z"), vec![uid(1)]);
        assert!(at("2026-06-06T06:00:00Z").is_empty());
    }

    #[test]
    fn a_layer_whose_hours_earlier_layers_cover_never_pages() {
        let all: &[OnCallWindow] = &[];
        let days: &[OnCallWindow] = &[window(&WEEKDAYS, "09:00", "17:00")];
        let nights: &[OnCallWindow] = &[window(&Weekday::ALL, "17:00", "09:00")];
        let weekends: &[OnCallWindow] = &[window(&[Weekday::Sat, Weekday::Sun], "00:00", "00:00")];
        let shadowed = |layer, behind| {
            Some(Shadowed {
                layer,
                behind,
                whole_week: true,
            })
        };
        assert_eq!(never_pages(&[days, all]), None);
        assert_eq!(never_pages(&[all, days]), shadowed(1, Some(0)));
        assert_eq!(
            never_pages(&[days, days]),
            Some(Shadowed {
                layer: 1,
                behind: None,
                whole_week: false,
            })
        );
        // Sunday night runs past the end of the week into Monday morning.
        assert_eq!(
            never_pages(&[days, nights, weekends, all]),
            shadowed(3, None)
        );
        assert_eq!(never_pages(&[days, nights, all]), None);
        let almost: &[OnCallWindow] = &[window(&Weekday::ALL, "17:01", "09:00")];
        assert_eq!(never_pages(&[days, almost, weekends, all]), None);
        // Every day from a time back to itself is the whole week, but more
        // hours on it would not help.
        let whole: &[OnCallWindow] = &[window(&Weekday::ALL, "09:00", "09:00")];
        assert_eq!(never_pages(&[days, whole, days]), shadowed(2, None));
        // Layer 1 alone covers the last one, so hours on layer 2 cannot help.
        assert_eq!(never_pages(&[days, all, days]), shadowed(2, None));
        assert_eq!(never_pages(&[days, all, nights]), shadowed(2, Some(1)));
    }

    #[test]
    fn day_names_are_the_ones_the_api_takes() {
        for d in Weekday::ALL {
            assert_eq!(
                serde_json::to_string(&d).unwrap(),
                format!("\"{}\"", d.as_str())
            );
        }
    }

    #[test]
    fn windows_read_as_times_to_the_minute() {
        let w: OnCallWindow =
            serde_json::from_str(r#"{"days":["mon","fri"],"from":"09:00","to":"17:30"}"#).unwrap();
        assert_eq!(w, window(&[Weekday::Mon, Weekday::Fri], "09:00", "17:30"));
        assert_eq!(
            serde_json::to_string(&w).unwrap(),
            r#"{"days":["mon","fri"],"from":"09:00","to":"17:30"}"#
        );
        for bad in ["9", "09:00:00", "24:00", "noon"] {
            let raw = format!(r#"{{"days":["mon"],"from":"{bad}","to":"17:00"}}"#);
            assert!(serde_json::from_str::<OnCallWindow>(&raw).is_err(), "{bad}");
        }
        assert!(
            serde_json::from_str::<OnCallWindow>(
                r#"{"days":["Monday"],"from":"09:00","to":"17:00"}"#
            )
            .is_err()
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
        let rota = Rota::new(
            &s,
            std::slice::from_ref(&l),
            &ov,
            from,
            t("2027-01-01T00:00:00Z"),
        );
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

    #[test]
    fn a_person_holds_their_turns_in_the_rotation() {
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0), participant(uid(2), 1)],
        );
        let got = shifts_held_by(
            &schedule("UTC"),
            &[l],
            &[],
            uid(2),
            t("2026-06-01T00:00:00Z"),
            t("2026-06-05T00:00:00Z"),
        )
        .collect::<Vec<_>>();
        assert_eq!(
            got,
            vec![
                t("2026-06-02T00:00:00Z")..t("2026-06-03T00:00:00Z"),
                t("2026-06-04T00:00:00Z")..t("2026-06-05T00:00:00Z"),
            ]
        );
    }

    #[test]
    fn a_person_s_shift_breaks_only_where_they_go_off() {
        let l = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-06-01T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        let overrides = [
            cover(uid(1), "2026-06-02T06:00:00Z", "2026-06-02T18:00:00Z"),
            cover(uid(7), "2026-06-02T16:00:00Z", "2026-06-02T20:00:00Z"),
            cover(uid(9), "2026-06-02T20:00:00Z", "2026-06-02T22:00:00Z"),
        ];
        let got = shifts_held_by(
            &schedule("UTC"),
            &[l],
            &overrides,
            uid(1),
            t("2026-06-02T00:00:00Z"),
            t("2026-06-03T00:00:00Z"),
        )
        .collect::<Vec<_>>();
        assert_eq!(
            got,
            vec![
                t("2026-06-02T00:00:00Z")..t("2026-06-02T18:00:00Z"),
                t("2026-06-02T22:00:00Z")..t("2026-06-03T00:00:00Z"),
            ]
        );
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
            ("Europe/Kyiv", days_and_nights(), vec![]),
            (
                "America/New_York",
                {
                    // Windows opening and closing inside both of the
                    // season's clock changes, with no layer at all hours.
                    let mut early = layer(
                        0,
                        RotationType::Custom,
                        5 * 3_600,
                        "2026-10-30T00:00:00Z",
                        vec![participant(uid(1), 0), participant(uid(2), 1)],
                    );
                    early.windows = vec![
                        window(&Weekday::ALL, "01:30", "02:30"),
                        window(&[Weekday::Sat, Weekday::Sun], "23:00", "01:15"),
                    ];
                    let mut late = layer(
                        1,
                        RotationType::Daily,
                        86_400,
                        "2026-10-30T01:45:00-04:00",
                        vec![participant(uid(3), 0), participant(uid(4), 1)],
                    );
                    late.windows = vec![window(&Weekday::ALL, "01:00", "12:00")];
                    vec![early, late]
                },
                vec![cover(
                    uid(9),
                    "2026-11-01T05:10:00Z",
                    "2026-11-01T06:20:00Z",
                )],
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

    #[test]
    fn a_window_closing_in_a_gap_runs_into_the_next_day() {
        // Saturday's 23:30 close falls in Nuuk's skipped hour, so it lands
        // at 00:30 on Sunday.
        let mut late = layer(
            0,
            RotationType::Daily,
            86_400,
            "2026-03-20T00:00:00Z",
            vec![participant(uid(1), 0)],
        );
        late.windows = vec![window(&[Weekday::Sat], "22:00", "23:30")];
        let rest = layer(
            1,
            RotationType::Daily,
            86_400,
            "2026-03-20T00:00:00Z",
            vec![participant(uid(2), 0)],
        );
        let s = schedule("America/Nuuk");
        let layers = [late, rest];
        assert_eq!(
            resolve_on_call(&s, &layers, &[], t("2026-03-29T01:15:00Z")),
            vec![uid(1)]
        );
        assert_eq!(
            resolve_on_call(&s, &layers, &[], t("2026-03-29T01:30:00Z")),
            vec![uid(2)]
        );
        assert_shifts_agree(
            "America/Nuuk",
            &layers,
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
