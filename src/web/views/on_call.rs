//! Server-rendered on-call pages under `/settings/on-call`: a schedule list
//! naming who is on call now, until when and who is next, a schedule builder
//! (layers, ordered participants, rotation), and the schedule calendar on the
//! edit page.
//!
//! Mutations run from the page against the JSON API (`/api/v1/on-call/...`), so
//! this module renders chrome, the member/channel choices, and prefills the
//! builder. Editing is owner-only — the API enforces it; a non-owner sees the
//! page but a mutation returns 403, mirroring the escalation surface.
//!
//! On a plan without on-call, an org that has built nothing sees what the
//! feature does and how to get it; one that has built schedules keeps the
//! working page with a notice, since what exists keeps paging.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::app::AppState;
use std::collections::{HashMap, HashSet};

use crate::domain::{
    OnCallScheduleDetail, OnCallWindow, OrgId, RotationType, UserId, Weekday, on_call_shifts,
};
use crate::error::{AppError, codes};
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::templates::filters;
use crate::templates::format::exact_duration;
use crate::web::error::WebResult;
use crate::web::views::on_call_calendar::{CalendarModel, calendar, overrides_from};
use crate::web::views::resolve_org;
use crate::web::views::team_lock::{TeamLock, plan_locked, shows_teaser, team_lock};

const TAB_ON_CALL: &str = "on-call";

/// How far ahead the schedule list looks for the next handoff.
const LOOKAHEAD_DAYS: i64 = 366;

/// One org member offered as a participant / override coverer.
#[derive(Clone)]
pub struct MemberChoice {
    pub id: UserId,
    pub email: String,
    /// Has a channel that can deliver a page. Without one, a member on shift
    /// resolves as on call and nothing reaches them.
    pub reachable: bool,
}

/// Members by user id, to name the users a schedule resolves to.
pub struct Roster<'a> {
    by_id: HashMap<UserId, &'a MemberChoice>,
    /// Email local parts, lowercased, that more than one member has.
    shared: HashSet<String>,
}

impl<'a> Roster<'a> {
    pub fn new(members: &'a [MemberChoice]) -> Self {
        let mut seen = HashSet::new();
        Self {
            by_id: members.iter().map(|m| (m.id, m)).collect(),
            shared: members
                .iter()
                .map(|m| local_part(&m.email).to_lowercase())
                .filter(|local| !seen.insert(local.clone()))
                .collect(),
        }
    }

    fn get(&self, user: &UserId) -> Option<&'a MemberChoice> {
        self.by_id.get(user).copied()
    }

    /// Someone who left came off every schedule with their membership, so a
    /// miss is only the moment between the two reads.
    pub fn email(&self, user: &UserId) -> &'a str {
        self.get(user).map_or("former member", |m| m.email.as_str())
    }

    /// The email's local part, or the whole email when another member's
    /// reads the same in any case.
    pub fn short(&self, user: &UserId) -> &'a str {
        let email = self.email(user);
        let local = local_part(email);
        if self.shared.contains(&local.to_lowercase()) {
            email
        } else {
            local
        }
    }

    /// No page reaches someone no longer a member.
    pub fn reachable(&self, user: &UserId) -> bool {
        self.get(user).is_some_and(|m| m.reachable)
    }
}

fn local_part(email: &str) -> &str {
    email.split_once('@').map_or(email, |(local, _)| local)
}

/// The schedule's responders no page can reach, by email.
fn unreachable(detail: &OnCallScheduleDetail, roster: &Roster) -> Vec<String> {
    let mut out: Vec<String> = detail
        .responders()
        .filter(|u| !roster.reachable(u))
        .map(|u| roster.email(&u).to_owned())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Who holds a schedule now, until when, and who takes over then.
pub struct Coverage {
    pub now: Vec<String>,
    pub overridden: bool,
    /// `None` when no one takes over within [`LOOKAHEAD_DAYS`].
    pub until: Option<DateTime<Utc>>,
    pub next: Vec<String>,
}

fn coverage(detail: &OnCallScheduleDetail, roster: &Roster, now: DateTime<Utc>) -> Coverage {
    let mut shifts = on_call_shifts(
        &detail.schedule,
        &detail.layers,
        &detail.overrides,
        now,
        now + TimeDelta::days(LOOKAHEAD_DAYS),
    );
    let names = |users: &[UserId]| -> Vec<String> {
        users.iter().map(|u| roster.email(u).to_owned()).collect()
    };
    let current = shifts.next();
    // Only the override mark changing is no handover.
    let after = current
        .as_ref()
        .and_then(|c| shifts.find(|s| !s.same_people(c)));
    Coverage {
        now: current
            .as_ref()
            .map(|s| names(&s.user_ids))
            .unwrap_or_default(),
        overridden: current.is_some_and(|s| s.overridden),
        until: after.as_ref().map(|s| s.starts_at),
        next: after.map(|s| names(&s.user_ids)).unwrap_or_default(),
    }
}

pub struct ScheduleRow {
    pub id: String,
    pub name: String,
    pub timezone: String,
    pub layer_count: usize,
    pub coverage: Coverage,
    /// Participants no page can reach, by email.
    pub unreachable: Vec<String>,
    /// Names of the policies that page this schedule.
    pub paged_by: Vec<String>,
}

/// A notification channel the signed-in member may opt into being paged on.
pub struct ContactChoice {
    pub id: String,
    pub name: String,
    pub checked: bool,
    /// Why it cannot carry a page now; `None` when it can. One that cannot
    /// stays a choice, marked with this.
    pub blocked: Option<&'static str>,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/on_call.html")]
pub struct OnCallPage {
    pub active_tab: &'static str,
    /// The member's own paging contacts, every org channel offered as a toggle.
    pub contacts: Vec<ContactChoice>,
    pub no_channels: bool,
    /// The viewer is on a rotation or covers an override.
    pub on_duty: bool,
    /// On duty with no channel that can deliver a page. The page toggles the
    /// notice as channels are ticked, so it is rendered whenever `on_duty`.
    pub unpageable: bool,
    pub lock: Option<TeamLock>,
}

impl OnCallPage {
    fn teaser(&self) -> bool {
        shows_teaser(&self.lock)
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/on_call_partial.html")]
pub struct OnCallPartial {
    pub schedules: Vec<ScheduleRow>,
    /// No add button above to point an empty list at.
    pub locked: bool,
}

/// One weekday toggle on a window row.
pub struct DayToggle {
    pub value: &'static str,
    pub label: String,
    pub on: bool,
}

/// One window prefilled in the builder, as its inputs take it.
pub struct WindowModel {
    /// Monday first.
    pub days: Vec<DayToggle>,
    pub from: String,
    pub to: String,
}

impl WindowModel {
    fn new(days: &[Weekday], from: String, to: String) -> Self {
        Self {
            days: Weekday::ALL
                .iter()
                .map(|d| {
                    let value = d.as_str();
                    DayToggle {
                        value,
                        label: value[..1].to_uppercase() + &value[1..],
                        on: days.contains(d),
                    }
                })
                .collect(),
            from,
            to,
        }
    }

    fn from_window(w: &OnCallWindow) -> Self {
        Self::new(
            &w.days,
            w.from.format("%H:%M").to_string(),
            w.to.format("%H:%M").to_string(),
        )
    }

    /// What "add hours" starts from: weekday working hours.
    fn working_hours() -> Self {
        Self::new(&Weekday::ALL[..5], "09:00".into(), "17:00".into())
    }
}

/// One layer prefilled in the builder. The JS serialises participants in the
/// order they are listed, which is the rotation order.
pub struct LayerModel {
    pub name: String,
    pub rotation_type: &'static str,
    /// Length in the unit `rotation_type` implies: days, weeks, or hours.
    pub rotation_length: String,
    /// First handoff as wall-clock time in the schedule's timezone, the value
    /// a `datetime-local` input takes.
    pub handoff_local: String,
    /// The stored instant, sent back unchanged when neither the handoff nor
    /// the timezone was edited, so a save never re-resolves it.
    pub handoff_at: String,
    /// Empty when it is on call at all hours.
    pub windows: Vec<WindowModel>,
    pub participants: Vec<MemberChoice>,
}

/// A zone in the picker, labelled with its offset from UTC right now.
pub struct ZoneChoice {
    pub name: String,
    pub label: String,
}

pub struct ScheduleFormModel {
    pub mode: &'static str,
    pub action: String,
    pub submit_method: &'static str,
    pub schedule_id: String,
    pub name: String,
    pub timezone: String,
    pub timezones: Vec<ZoneChoice>,
    pub members: Vec<MemberChoice>,
    pub layers: Vec<LayerModel>,
    /// What "add layer" clones.
    pub blank_layer: LayerModel,
    /// What "add hours" clones.
    pub blank_window: WindowModel,
    /// The saved schedule's month; `None` while creating one.
    pub calendar: Option<CalendarModel>,
    /// True when the org has no members to staff a rotation (cannot happen for
    /// a real org — there is always an owner — but guards the empty dev case).
    pub no_members: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/on_call_form.html")]
pub struct ScheduleFormPage {
    pub active_tab: &'static str,
    pub form: ScheduleFormModel,
}

pub async fn index(
    _auth: AuthedBrowser,
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/on-call") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let mine = state.contact_store.for_user(org, user).await?;
    // A member still wired to be paged keeps the page, so they can unwire.
    let lock = team_lock(&state, org, user, async {
        Ok(!mine.is_empty() || !state.on_call_store.list(org).await?.is_empty())
    })
    .await?;
    if shows_teaser(&lock) {
        return Ok(OnCallPage {
            active_tab: TAB_ON_CALL,
            contacts: Vec::new(),
            no_channels: true,
            on_duty: false,
            unpageable: false,
            lock,
        }
        .into_response());
    }
    let channels = state.notification_channel_store.list(org).await?;
    let on_duty = state
        .on_call_store
        .current(org, Utc::now())
        .await?
        .iter()
        .any(|d| d.responders().any(|u| u == user));
    let unpageable = on_duty
        && !channels
            .iter()
            .any(|c| c.can_deliver() && mine.contains(&c.id));
    let contacts = channels
        .into_iter()
        .map(|c| ContactChoice {
            checked: mine.contains(&c.id),
            blocked: c.delivery_block(),
            id: c.id.to_string(),
            name: c.name,
        })
        .collect::<Vec<_>>();
    Ok(OnCallPage {
        active_tab: TAB_ON_CALL,
        no_channels: contacts.is_empty(),
        contacts,
        on_duty,
        unpageable,
        lock,
    }
    .into_response())
}

pub async fn list_partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/on-call") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let now = Utc::now();
    let (members, pagers, current) = tokio::try_join!(
        org_members(&state, org),
        async { Ok(state.escalation_policy_store.schedule_pagers(org).await?) },
        async { Ok(state.on_call_store.current(org, now).await?) },
    )?;
    let roster = Roster::new(&members);
    let mut paged_by: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (sid, name) in pagers {
        paged_by.entry(sid).or_default().push(name);
    }
    let schedules: Vec<ScheduleRow> = current
        .into_iter()
        .map(|d| ScheduleRow {
            coverage: coverage(&d, &roster, now),
            unreachable: unreachable(&d, &roster),
            paged_by: paged_by.remove(&d.schedule.id).unwrap_or_default(),
            id: d.schedule.id.to_string(),
            layer_count: d.layers.len(),
            name: d.schedule.name,
            timezone: d.schedule.timezone,
        })
        .collect();
    let locked = schedules.is_empty() && plan_locked(&state, org).await?;
    Ok(OnCallPartial { schedules, locked }.into_response())
}

/// Org members as builder choices. Empty without a DB (single-tenant dev).
pub(crate) async fn org_members(state: &AppState, org: OrgId) -> WebResult<Vec<MemberChoice>> {
    let Some(pool) = &state.db else {
        return Ok(vec![]);
    };
    // Only a warning rides on this, so a channel that fails to load leaves
    // everyone unflagged rather than taking the page down.
    let reachable = reachable_users(state, org)
        .await
        .inspect_err(
            |e| tracing::warn!(error = %e, org = %org.0, "on-call reachability unavailable"),
        )
        .ok();
    Ok(crate::storage::orgs::list_members(pool, org)
        .await?
        .into_iter()
        .map(|m| MemberChoice {
            reachable: reachable
                .as_ref()
                .is_none_or(|r| r.contains(&m.membership.user_id)),
            id: m.membership.user_id,
            email: m.email,
        })
        .collect())
}

/// Members with a channel that can deliver a page.
async fn reachable_users(state: &AppState, org: OrgId) -> crate::error::Result<HashSet<UserId>> {
    let delivering: HashSet<Uuid> = state
        .notification_channel_store
        .list(org)
        .await?
        .into_iter()
        .filter(|c| c.can_deliver())
        .map(|c| c.id)
        .collect();
    Ok(state
        .contact_store
        .for_org(org)
        .await?
        .into_iter()
        .filter(|(_, c)| delivering.contains(c))
        .map(|(u, _)| u)
        .collect())
}

/// One empty daily layer to seed a fresh builder.
fn empty_layer() -> LayerModel {
    LayerModel {
        name: String::new(),
        rotation_type: "daily",
        rotation_length: "1".into(),
        handoff_local: String::new(),
        handoff_at: String::new(),
        windows: Vec::new(),
        participants: Vec::new(),
    }
}

pub async fn new_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/on-call/new") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    // The list page says why and how to get it; a blank builder would only
    // fail on save.
    if plan_locked(&state, org).await? {
        return Ok(Redirect::to("/settings/on-call").into_response());
    }
    let members = org_members(&state, org).await?;
    let form = ScheduleFormModel {
        mode: "create",
        action: "/api/v1/on-call/schedules".into(),
        submit_method: "POST",
        schedule_id: String::new(),
        name: String::new(),
        timezones: zone_choices("UTC", Utc::now()),
        timezone: "UTC".into(),
        layers: vec![empty_layer()],
        blank_layer: empty_layer(),
        blank_window: WindowModel::working_hours(),
        no_members: members.is_empty(),
        members,
        calendar: None,
    };
    Ok(ScheduleFormPage {
        active_tab: TAB_ON_CALL,
        form,
    }
    .into_response())
}

/// A layer's stored second-count in the form its rotation type takes: a count
/// of days or weeks, or a duration such as `12h` for a custom rotation.
fn length_in_unit(rotation: RotationType, secs: i32) -> String {
    let secs = i64::from(secs);
    match rotation {
        RotationType::Daily => (secs / 86_400).to_string(),
        RotationType::Weekly => (secs / 604_800).to_string(),
        RotationType::Custom => exact_duration(secs.unsigned_abs()),
    }
}

/// The picker's zones: `current` first when the list leaves it out, since the
/// API takes any IANA name, then [`timezone_names`], each with its offset at
/// `now`.
fn zone_choices(current: &str, now: DateTime<Utc>) -> Vec<ZoneChoice> {
    let names = timezone_names();
    let stored = (!names.contains(&current)).then_some(current);
    stored
        .into_iter()
        .chain(names)
        .map(|name| {
            let tz: chrono_tz::Tz = name.parse().unwrap_or(chrono_tz::UTC);
            let offset = now.with_timezone(&tz).format("%:z").to_string();
            ZoneChoice {
                label: if name == "UTC" {
                    name.to_owned()
                } else {
                    format!("{name} (UTC{offset})")
                },
                name: name.to_owned(),
            }
        })
        .collect()
}

/// Region-named zones and UTC, the ones a person picks from. Leaves out
/// country and legacy names such as `US/Eastern` and `GB`, and the
/// sign-inverted `Etc/GMT+N`; the API still accepts any IANA name.
fn timezone_names() -> Vec<&'static str> {
    const REGIONS: [&str; 10] = [
        "Africa",
        "America",
        "Antarctica",
        "Arctic",
        "Asia",
        "Atlantic",
        "Australia",
        "Europe",
        "Indian",
        "Pacific",
    ];
    static NAMES: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
        std::iter::once("UTC")
            .chain(
                chrono_tz::TZ_VARIANTS
                    .iter()
                    .map(|tz| tz.name())
                    .filter(|name| {
                        name.split_once('/')
                            .is_some_and(|(r, _)| REGIONS.contains(&r))
                    }),
            )
            .collect()
    });
    NAMES.clone()
}

/// An instant as wall-clock time in `tz`, for a `datetime-local` input.
fn local_input(at: DateTime<Utc>, tz: chrono_tz::Tz) -> String {
    at.with_timezone(&tz).format("%Y-%m-%dT%H:%M").to_string()
}

pub async fn edit_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
) -> WebResult<Response> {
    let org = match resolve_org(org, &format!("/settings/on-call/{id}/edit")) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let now = Utc::now();
    let (detail, members) = tokio::try_join!(
        async {
            Ok(state
                .on_call_store
                .get_from(org, id, overrides_from(None, now))
                .await?)
        },
        org_members(&state, org),
    )?;
    let detail = detail.ok_or_else(|| {
        AppError::not_found(codes::ON_CALL_SCHEDULE_NOT_FOUND, "schedule not found")
    })?;
    let roster = Roster::new(&members);
    let mut layers: Vec<LayerModel> = detail
        .layers
        .iter()
        .map(|l| {
            let mut participants: Vec<&crate::domain::OnCallParticipant> =
                l.participants.iter().collect();
            participants.sort_by_key(|p| p.position);
            LayerModel {
                name: l.name.clone().unwrap_or_default(),
                rotation_type: l.rotation_type.as_db_str(),
                rotation_length: length_in_unit(l.rotation_type, l.rotation_length_secs),
                handoff_local: local_input(l.handoff_at, detail.schedule.tz()),
                handoff_at: l.handoff_at.to_rfc3339(),
                windows: l.windows.iter().map(WindowModel::from_window).collect(),
                participants: participants
                    .iter()
                    .filter_map(|p| roster.get(&p.user_id).cloned())
                    .collect(),
            }
        })
        .collect();
    if layers.is_empty() {
        layers.push(empty_layer());
    }
    let calendar = calendar(&detail, &roster, None, now);
    let form = ScheduleFormModel {
        mode: "edit",
        action: format!("/api/v1/on-call/schedules/{}", detail.schedule.id),
        submit_method: "PATCH",
        schedule_id: detail.schedule.id.to_string(),
        name: detail.schedule.name,
        timezones: zone_choices(&detail.schedule.timezone, now),
        timezone: detail.schedule.timezone,
        no_members: members.is_empty(),
        members,
        layers,
        blank_layer: empty_layer(),
        blank_window: WindowModel::working_hours(),
        calendar: Some(calendar),
    };
    Ok(ScheduleFormPage {
        active_tab: TAB_ON_CALL,
        form,
    }
    .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: UserId, email: &str, reachable: bool) -> MemberChoice {
        MemberChoice {
            id,
            email: email.into(),
            reachable,
        }
    }

    fn row(unreachable: &[&str], paged_by: &[&str]) -> ScheduleRow {
        ScheduleRow {
            id: "sid".into(),
            name: "Primary".into(),
            timezone: "UTC".into(),
            layer_count: 2,
            coverage: Coverage {
                now: vec!["olena@example.com".into()],
                overridden: false,
                until: None,
                next: vec![],
            },
            unreachable: unreachable.iter().map(|e| (*e).to_owned()).collect(),
            paged_by: paged_by.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn list(rows: Vec<ScheduleRow>) -> String {
        OnCallPartial {
            schedules: rows,
            locked: false,
        }
        .render()
        .unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn uid(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    fn rotation(overrides: Vec<crate::domain::OnCallOverride>) -> OnCallScheduleDetail {
        use crate::domain::{OnCallLayer, OnCallParticipant, OnCallSchedule};
        let participant = |n: u128, position| OnCallParticipant {
            id: Uuid::now_v7(),
            user_id: uid(n),
            position,
        };
        OnCallScheduleDetail {
            schedule: OnCallSchedule {
                id: Uuid::nil(),
                name: "Primary".into(),
                timezone: "UTC".into(),
                created_at: t("2026-01-01T00:00:00Z"),
                updated_at: t("2026-01-01T00:00:00Z"),
            },
            layers: vec![OnCallLayer {
                id: Uuid::now_v7(),
                name: None,
                rotation_type: RotationType::Daily,
                rotation_length_secs: 86_400,
                handoff_at: t("2026-09-01T09:00:00Z"),
                layer_order: 0,
                windows: vec![],
                created_at: t("2026-01-01T00:00:00Z"),
                participants: vec![participant(1, 0), participant(2, 1)],
            }],
            overrides,
        }
    }

    fn roster_members() -> Vec<MemberChoice> {
        vec![
            member(uid(1), "olena@example.com", true),
            member(uid(2), "taras@example.com", true),
            member(uid(3), "iryna@example.com", true),
        ]
    }

    #[test]
    fn unreachable_names_each_person_no_page_reaches_once() {
        let members = vec![
            member(uid(1), "olena@example.com", true),
            member(uid(2), "taras@example.com", false),
            member(uid(3), "iryna@example.com", false),
            member(uid(4), "petro@example.com", false),
        ];
        let cover = |n| crate::domain::OnCallOverride {
            id: Uuid::now_v7(),
            user_id: uid(n),
            starts_at: t("2026-09-26T00:00:00Z"),
            ends_at: t("2026-09-27T00:00:00Z"),
            created_by: None,
            created_at: t("2026-09-25T00:00:00Z"),
        };
        let mut d = rotation(vec![cover(3), cover(2), cover(1)]);
        // A later layer pages in the hours the first one leaves.
        d.layers[0].windows = vec![
            serde_json::from_str(
                r#"{"days":["mon","tue","wed","thu","fri"],"from":"09:00","to":"17:00"}"#,
            )
            .unwrap(),
        ];
        let mut later = d.layers[0].clone();
        later.layer_order = 1;
        later.windows.clear();
        later.participants[0].user_id = uid(4);
        d.layers.push(later);
        assert_eq!(
            unreachable(&d, &Roster::new(&members)),
            vec![
                "iryna@example.com",
                "petro@example.com",
                "taras@example.com"
            ]
        );
    }

    #[test]
    fn coverage_names_who_is_on_until_when_and_who_is_next() {
        let members = roster_members();
        let c = coverage(
            &rotation(vec![]),
            &Roster::new(&members),
            t("2026-09-26T12:00:00Z"),
        );
        assert_eq!(c.now, vec!["taras@example.com"]);
        assert!(!c.overridden);
        assert_eq!(c.until, Some(t("2026-09-27T09:00:00Z")));
        assert_eq!(c.next, vec!["olena@example.com"]);
    }

    #[test]
    fn coverage_follows_an_override_and_what_comes_after_it() {
        let members = roster_members();
        let ov = crate::domain::OnCallOverride {
            id: Uuid::now_v7(),
            user_id: uid(3),
            starts_at: t("2026-09-26T00:00:00Z"),
            ends_at: t("2026-09-26T18:00:00Z"),
            created_by: None,
            created_at: t("2026-09-20T00:00:00Z"),
        };
        let c = coverage(
            &rotation(vec![ov]),
            &Roster::new(&members),
            t("2026-09-26T12:00:00Z"),
        );
        assert_eq!(c.now, vec!["iryna@example.com"]);
        assert!(c.overridden);
        assert_eq!(c.until, Some(t("2026-09-26T18:00:00Z")));
        assert_eq!(c.next, vec!["taras@example.com"]);
    }

    #[test]
    fn an_override_by_the_person_on_shift_is_no_handover() {
        let members = roster_members();
        let ov = crate::domain::OnCallOverride {
            id: Uuid::now_v7(),
            user_id: uid(2),
            starts_at: t("2026-09-26T10:00:00Z"),
            ends_at: t("2026-09-26T18:00:00Z"),
            created_by: None,
            created_at: t("2026-09-20T00:00:00Z"),
        };
        let c = coverage(
            &rotation(vec![ov]),
            &Roster::new(&members),
            t("2026-09-26T12:00:00Z"),
        );
        assert_eq!(c.now, vec!["taras@example.com"]);
        assert!(c.overridden);
        assert_eq!(c.until, Some(t("2026-09-27T09:00:00Z")));
        assert_eq!(c.next, vec!["olena@example.com"]);
    }

    #[test]
    fn short_names_keep_the_domain_when_two_members_share_a_local_part() {
        let members = vec![
            member(uid(1), "ops@acme.com", true),
            member(uid(2), "ops@contractor.io", true),
            member(uid(3), "olena@acme.com", true),
        ];
        let roster = Roster::new(&members);
        assert_eq!(roster.short(&uid(1)), "ops@acme.com");
        assert_eq!(roster.short(&uid(2)), "ops@contractor.io");
        assert_eq!(roster.short(&uid(3)), "olena");
    }

    #[test]
    fn short_names_keep_the_domain_when_local_parts_differ_only_in_case() {
        let members = vec![
            member(uid(1), "Ops@acme.com", true),
            member(uid(2), "ops@contractor.io", true),
        ];
        let roster = Roster::new(&members);
        assert_eq!(roster.short(&uid(1)), "Ops@acme.com");
        assert_eq!(roster.short(&uid(2)), "ops@contractor.io");
    }

    #[test]
    fn a_rotation_of_one_has_no_handoff_ahead() {
        let members = roster_members();
        let mut d = rotation(vec![]);
        d.layers[0].participants.truncate(1);
        let c = coverage(&d, &Roster::new(&members), t("2026-09-26T12:00:00Z"));
        assert_eq!(c.now, vec!["olena@example.com"]);
        assert_eq!(c.until, None);
    }

    #[test]
    fn list_renders_who_is_on_call_until_and_next() {
        let mut r = row(&[], &["Production"]);
        r.coverage = Coverage {
            now: vec!["olena@example.com".into()],
            overridden: true,
            until: Some(t("2026-09-27T09:00:00Z")),
            next: vec![],
        };
        let html = list(vec![r]);
        assert!(html.contains("Primary"));
        assert!(html.contains("olena@example.com"));
        assert!(html.contains(">override<"));
        assert!(html.contains(
            r#"<time data-tz="at" datetime="2026-09-27T09:00:00Z">2026-09-27 09:00 UTC</time>"#
        ));
        assert!(html.contains(
            r#"then
            <span class="flash-text--warn">no one</span>"#
        ));
        assert!(!html.contains("no paging channels"));
    }

    #[test]
    fn list_warns_when_no_one_is_on_call() {
        let mut r = row(&[], &["Production"]);
        r.coverage = Coverage {
            now: vec![],
            overridden: false,
            until: None,
            next: vec![],
        };
        let html = list(vec![r]);
        assert!(html.contains(r#"<span class="flash-text--warn">no one</span>"#));
        assert!(!html.contains("<time"));
    }

    #[test]
    fn list_flags_participants_no_page_can_reach() {
        let html = list(vec![row(&["taras@example.com"], &["Production"])]);
        assert!(html.contains("no paging channels: taras@example.com"));
    }

    #[test]
    fn delete_names_the_policies_that_page_the_schedule() {
        let html = list(vec![row(&[], &["Production", "Nights"])]);
        assert!(html.contains("Production, Nights page Primary and will reach no one"));
        let html = list(vec![row(&[], &[])]);
        assert!(html.contains("No escalation policy pages Primary."));
        assert!(html.contains("no escalation policy pages it"));
    }

    #[test]
    fn empty_list_renders_onboarding() {
        let html = OnCallPartial {
            schedules: vec![],
            locked: false,
        }
        .render()
        .unwrap();
        assert!(html.contains("# no on-call schedules yet"));
        assert!(html.contains("add one above"));
    }

    #[test]
    fn empty_locked_list_points_at_the_plan_not_a_missing_button() {
        let html = OnCallPartial {
            schedules: vec![],
            locked: true,
        }
        .render()
        .unwrap();
        assert!(!html.contains("add one above"));
        assert!(html.contains("Team plan"));
    }

    fn page(lock: Option<TeamLock>) -> OnCallPage {
        OnCallPage {
            active_tab: TAB_ON_CALL,
            no_channels: false,
            contacts: vec![ContactChoice {
                id: "cid".into(),
                name: "Ops Slack".into(),
                checked: true,
                blocked: None,
            }],
            on_duty: false,
            unpageable: false,
            lock,
        }
    }

    fn locked(teaser: bool) -> Option<TeamLock> {
        Some(TeamLock {
            payer: false,
            teaser,
        })
    }

    #[test]
    fn page_renders_contact_card() {
        let html = page(None).render().unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("channels that page you"));
        assert!(html.contains(r#"data-contact value="cid" data-delivers checked"#));
        assert!(html.contains(r#"href="/settings/on-call/new""#));
        let (_, mine) = html
            .split_once(r#"hx-get="/web/partials/settings/on-call/mine""#)
            .unwrap();
        assert!(!mine.split_once('>').unwrap().0.contains("oncall:feed"));
        let (_, feed) = html
            .split_once(r#"hx-get="/web/partials/settings/on-call/my-calendar""#)
            .unwrap();
        assert!(
            feed.split_once('>')
                .unwrap()
                .0
                .contains("oncall:feed from:body")
        );
        assert!(!html.contains("Team plan"));
        assert!(!html.contains("reaches no one"));
    }

    #[test]
    fn a_member_on_shift_without_channels_is_told() {
        let mut p = page(None);
        p.on_duty = true;
        p.unpageable = true;
        let html = p.render().unwrap();
        assert!(html.contains(r#"<div data-unpageable class="p-5">"#));
        assert!(html.contains("reaches no one"));
    }

    #[test]
    fn the_notice_is_kept_hidden_for_the_page_to_show_when_a_channel_is_dropped() {
        let mut p = page(None);
        p.on_duty = true;
        let html = p.render().unwrap();
        assert!(html.contains(r#"data-unpageable class="p-5" hidden"#));
        assert!(html.contains(r#"data-contact value="cid" data-delivers checked"#));
    }

    #[test]
    fn locked_org_with_nothing_built_sees_the_pitch_not_the_page() {
        let html = page(locked(true)).render().unwrap();
        assert!(html.contains("Team plan"));
        assert!(html.contains("Ask the account owner"));
        assert!(!html.contains(r#"href="/settings/on-call/new""#));
        assert!(!html.contains("hx-get=\"/web/partials/settings/on-call\""));
        assert!(!html.contains("your on-call"));
        assert!(!html.contains("channels that page you"));
    }

    #[test]
    fn downgraded_org_keeps_its_schedules_under_a_notice() {
        let html = page(locked(false)).render().unwrap();
        assert!(html.contains("keeps paging"));
        assert!(html.contains("hx-get=\"/web/partials/settings/on-call\""));
        assert!(html.contains("channels that page you"));
        assert!(!html.contains(r#"href="/settings/on-call/new""#));
    }

    fn form(mode: &'static str) -> ScheduleFormModel {
        ScheduleFormModel {
            mode,
            action: "/api/v1/on-call/schedules".into(),
            submit_method: "POST",
            schedule_id: "sid".into(),
            name: "Primary".into(),
            timezone: "UTC".into(),
            timezones: zone_choices("UTC", t("2026-09-26T12:00:00Z"))
                .into_iter()
                .filter(|z| z.name == "UTC" || z.name == "Europe/Kyiv")
                .collect(),
            members: vec![
                member(uid(1), "a@b.c", true),
                member(uid(2), "taras@example.com", false),
            ],
            layers: vec![LayerModel {
                name: String::new(),
                rotation_type: "daily",
                rotation_length: "1".into(),
                handoff_local: "2026-09-28T09:00".into(),
                handoff_at: "2026-09-28T06:00:00+00:00".into(),
                windows: vec![],
                participants: vec![
                    member(uid(2), "taras@example.com", false),
                    member(uid(1), "a@b.c", true),
                ],
            }],
            blank_layer: empty_layer(),
            blank_window: WindowModel::working_hours(),
            calendar: None,
            no_members: false,
        }
    }

    #[test]
    fn new_form_renders_one_layer_no_calendar() {
        let html = ScheduleFormPage {
            active_tab: TAB_ON_CALL,
            form: form("create"),
        }
        .render()
        .unwrap();
        assert!(html.contains("data-layer-row"));
        assert!(html.contains(r#"data-rotation-type"#));
        // The calendar shows a saved schedule, so the create form has none.
        assert!(!html.contains("data-cal-grid"));
        assert!(!html.contains("on_call_calendar"));
    }

    #[test]
    fn participants_render_in_rotation_order_with_reachability() {
        let html = ScheduleFormPage {
            active_tab: TAB_ON_CALL,
            form: form("edit"),
        }
        .render()
        .unwrap();
        let first = html
            .find(&format!(r#"data-participant="{}""#, uid(2)))
            .unwrap();
        let second = html
            .find(&format!(r#"data-participant="{}""#, uid(1)))
            .unwrap();
        assert!(first < second, "stored position order is kept");
        assert!(html.contains(r#"value="2026-09-28T09:00" data-iso="2026-09-28T06:00:00+00:00""#));
        assert!(html.contains(r#"<option value="Europe/Kyiv">Europe/Kyiv (UTC+03:00)</option>"#));
        assert!(html.contains(&format!(
            r#"value="{}" data-email="taras@example.com" data-unreachable"#,
            uid(2)
        )));
        assert!(html.contains(r#"id="participant-template""#));
    }

    #[test]
    fn lengths_show_in_the_unit_the_rotation_implies() {
        assert_eq!(length_in_unit(RotationType::Weekly, 1_209_600), "2");
        assert_eq!(length_in_unit(RotationType::Custom, 43_200), "12h");
        assert_eq!(length_in_unit(RotationType::Custom, 5_400), "90m");
    }

    #[test]
    fn a_stored_zone_the_list_leaves_out_is_still_offered() {
        let now = t("2026-01-15T12:00:00Z");
        let zones = zone_choices("US/Eastern", now);
        assert_eq!(zones[0].name, "US/Eastern");
        assert_eq!(zones[0].label, "US/Eastern (UTC-05:00)");
        assert_eq!(zones[1].label, "UTC");
        assert_eq!(zone_choices("UTC", now)[0].name, "UTC");
    }

    #[test]
    fn handoff_prefills_in_the_schedule_zone() {
        let at = "2026-09-28T06:00:00Z".parse().unwrap();
        assert_eq!(local_input(at, chrono_tz::Europe::Kyiv), "2026-09-28T09:00");
    }

    #[test]
    fn edit_form_renders_the_calendar() {
        let members = roster_members();
        let mut f = form("edit");
        f.submit_method = "PATCH";
        f.calendar = Some(calendar(
            &rotation(vec![]),
            &Roster::new(&members),
            None,
            t("2026-09-26T12:00:00Z"),
        ));
        let html = ScheduleFormPage {
            active_tab: TAB_ON_CALL,
            form: f,
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"id="on-call-calendar""#));
        assert!(html.contains(r#"data-month="2026-09""#));
        assert!(html.contains("data-cal-grid"));
        assert!(html.contains("on_call_calendar"));
    }
}
