//! Server-rendered on-call pages under `/settings/on-call`: a schedule list
//! with a live "who's on call" readout, a schedule builder (layers, ordered
//! participants, rotation), and an overrides calendar on the edit page.
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
use uuid::Uuid;

use crate::app::AppState;
use std::collections::{HashMap, HashSet};

use crate::domain::{OrgId, RotationType, UserId};
use crate::error::AppError;
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::templates::filters;
use crate::templates::format::exact_duration;
use crate::web::error::WebResult;
use crate::web::views::resolve_org;
use crate::web::views::team_lock::{TeamLock, plan_locked, shows_teaser, team_lock};

const TAB_ON_CALL: &str = "on-call";

/// One org member offered as a participant / override coverer.
#[derive(Clone)]
pub struct MemberChoice {
    pub id: String,
    pub email: String,
    /// Has a channel that can deliver a page. Without one, a member on shift
    /// resolves as on call and nothing reaches them.
    pub reachable: bool,
}

pub struct ScheduleRow {
    pub id: String,
    pub name: String,
    pub timezone: String,
    pub layer_count: i64,
    pub created: chrono::DateTime<chrono::Utc>,
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
    /// Member id → email, rendered hidden so the who-on-call widget can name
    /// the user ids the resolver returns without a second request.
    pub members: Vec<MemberChoice>,
    /// No add button above to point an empty list at.
    pub locked: bool,
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
    pub participants: Vec<MemberChoice>,
}

/// One existing override rendered onto the calendar.
pub struct OverrideModel {
    pub id: String,
    pub user_id: String,
    pub email: String,
    pub starts_at: chrono::DateTime<chrono::Utc>,
    pub ends_at: chrono::DateTime<chrono::Utc>,
}

pub struct ScheduleFormModel {
    pub mode: &'static str,
    pub action: String,
    pub submit_method: &'static str,
    pub schedule_id: String,
    pub name: String,
    pub timezone: String,
    /// Zone names offered as the timezone field's suggestions.
    pub timezones: Vec<&'static str>,
    pub members: Vec<MemberChoice>,
    pub layers: Vec<LayerModel>,
    /// What "add layer" clones.
    pub blank_layer: LayerModel,
    pub overrides: Vec<OverrideModel>,
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
        .responders(org)
        .await?
        .iter()
        .any(|(_, u)| *u == user);
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
    let (members, responders, pagers, summaries) = tokio::try_join!(
        org_members(&state, org),
        async { Ok(state.on_call_store.responders(org).await?) },
        async { Ok(state.escalation_policy_store.schedule_pagers(org).await?) },
        async { Ok(state.on_call_store.list(org).await?) },
    )?;
    let by_id: HashMap<String, &MemberChoice> = members.iter().map(|m| (m.id.clone(), m)).collect();
    let mut unreachable: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (sid, user) in &responders {
        if let Some(m) = by_id.get(&user.to_string()).filter(|m| !m.reachable) {
            unreachable.entry(*sid).or_default().push(m.email.clone());
        }
    }
    let mut paged_by: HashMap<Uuid, Vec<String>> = HashMap::new();
    for (sid, name) in pagers {
        paged_by.entry(sid).or_default().push(name);
    }
    let schedules: Vec<ScheduleRow> = summaries
        .into_iter()
        .map(|s| ScheduleRow {
            unreachable: unreachable.remove(&s.id).unwrap_or_default(),
            paged_by: paged_by.remove(&s.id).unwrap_or_default(),
            id: s.id.to_string(),
            name: s.name,
            timezone: s.timezone,
            layer_count: s.layer_count,
            created: s.created_at,
        })
        .collect();
    let locked = schedules.is_empty() && plan_locked(&state, org).await?;
    Ok(OnCallPartial {
        schedules,
        members,
        locked,
    }
    .into_response())
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
            id: m.membership.user_id.to_string(),
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
        timezone: "UTC".into(),
        timezones: timezone_names(),
        layers: vec![empty_layer()],
        blank_layer: empty_layer(),
        no_members: members.is_empty(),
        members,
        overrides: vec![],
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

/// An instant as wall-clock time in `timezone`, for a `datetime-local` input.
/// An unknown zone reads as UTC, the same fallback the resolver uses.
fn local_input(at: chrono::DateTime<chrono::Utc>, timezone: &str) -> String {
    let tz: chrono_tz::Tz = timezone.parse().unwrap_or(chrono_tz::UTC);
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
    let detail =
        state.on_call_store.get(org, id).await?.ok_or_else(|| {
            AppError::not_found("ON_CALL_SCHEDULE_NOT_FOUND", "schedule not found")
        })?;
    let members = org_members(&state, org).await?;
    let by_id: HashMap<String, &MemberChoice> = members.iter().map(|m| (m.id.clone(), m)).collect();
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
                handoff_local: local_input(l.handoff_at, &detail.schedule.timezone),
                handoff_at: l.handoff_at.to_rfc3339(),
                participants: participants
                    .iter()
                    .filter_map(|p| by_id.get(&p.user_id.to_string()).map(|m| (*m).clone()))
                    .collect(),
            }
        })
        .collect();
    if layers.is_empty() {
        layers.push(empty_layer());
    }
    let overrides = detail
        .overrides
        .iter()
        .filter_map(|o| {
            let m = by_id.get(&o.user_id.to_string())?;
            Some(OverrideModel {
                id: o.id.to_string(),
                user_id: m.id.clone(),
                email: m.email.clone(),
                starts_at: o.starts_at,
                ends_at: o.ends_at,
            })
        })
        .collect();
    let form = ScheduleFormModel {
        mode: "edit",
        action: format!("/api/v1/on-call/schedules/{}", detail.schedule.id),
        submit_method: "PATCH",
        schedule_id: detail.schedule.id.to_string(),
        name: detail.schedule.name,
        timezone: detail.schedule.timezone,
        timezones: timezone_names(),
        no_members: members.is_empty(),
        members,
        layers,
        blank_layer: empty_layer(),
        overrides,
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

    fn member(id: &str, email: &str, reachable: bool) -> MemberChoice {
        MemberChoice {
            id: id.into(),
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
            created: "2026-06-04T00:00:00Z".parse().unwrap(),
            unreachable: unreachable.iter().map(|e| (*e).to_owned()).collect(),
            paged_by: paged_by.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn list(rows: Vec<ScheduleRow>) -> String {
        OnCallPartial {
            schedules: rows,
            members: vec![member("uid", "a@b.c", true)],
            locked: false,
        }
        .render()
        .unwrap()
    }

    #[test]
    fn list_renders_who_widget_and_member_map() {
        let html = list(vec![row(&[], &["Production"])]);
        assert!(html.contains("Primary"));
        assert!(html.contains(r#"data-who-on-call"#));
        assert!(html.contains(r#"data-schedule-id="sid""#));
        assert!(html.contains(r#"data-member-email="a@b.c""#));
        assert!(!html.contains("no paging channels"));
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
            members: vec![],
            locked: false,
        }
        .render()
        .unwrap();
        assert!(html.contains("No on-call schedules yet"));
        assert!(html.contains("add one above"));
    }

    #[test]
    fn empty_locked_list_points_at_the_plan_not_a_missing_button() {
        let html = OnCallPartial {
            schedules: vec![],
            members: vec![],
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
        assert!(!html.contains("Team plan"));
        assert!(!html.contains("reaches no one"));
    }

    #[test]
    fn a_member_on_shift_without_channels_is_told() {
        let mut p = page(None);
        p.on_duty = true;
        p.unpageable = true;
        let html = p.render().unwrap();
        assert!(html.contains("reaches no one"));
        assert!(
            !html.contains("data-unpageable class=\"alert-card alert-card--warn text-sm\" hidden")
        );
    }

    #[test]
    fn the_notice_is_kept_hidden_for_the_page_to_show_when_a_channel_is_dropped() {
        let mut p = page(None);
        p.on_duty = true;
        let html = p.render().unwrap();
        assert!(
            html.contains("data-unpageable class=\"alert-card alert-card--warn text-sm\" hidden")
        );
        assert!(html.contains(r#"data-contact value="cid" data-delivers checked"#));
    }

    #[test]
    fn locked_org_with_nothing_built_sees_the_pitch_not_the_page() {
        let html = page(locked(true)).render().unwrap();
        assert!(html.contains("Team plan"));
        assert!(html.contains("Ask the account owner"));
        assert!(!html.contains(r#"href="/settings/on-call/new""#));
        assert!(!html.contains("hx-get=\"/web/partials/settings/on-call\""));
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
            timezones: vec!["UTC", "Europe/Kyiv"],
            members: vec![
                member("uid", "a@b.c", true),
                member("u2", "taras@example.com", false),
            ],
            layers: vec![LayerModel {
                name: String::new(),
                rotation_type: "daily",
                rotation_length: "1".into(),
                handoff_local: "2026-09-28T09:00".into(),
                handoff_at: "2026-09-28T06:00:00+00:00".into(),
                participants: vec![
                    member("u2", "taras@example.com", false),
                    member("uid", "a@b.c", true),
                ],
            }],
            blank_layer: empty_layer(),
            overrides: vec![],
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
        // The overrides calendar is edit-only.
        assert!(!html.contains("data-cal-grid"));
    }

    #[test]
    fn participants_render_in_rotation_order_with_reachability() {
        let html = ScheduleFormPage {
            active_tab: TAB_ON_CALL,
            form: form("edit"),
        }
        .render()
        .unwrap();
        let first = html.find(r#"data-participant="u2""#).unwrap();
        let second = html.find(r#"data-participant="uid""#).unwrap();
        assert!(first < second, "stored position order is kept");
        assert!(html.contains(r#"value="2026-09-28T09:00" data-iso="2026-09-28T06:00:00+00:00""#));
        assert!(html.contains(r#"<option value="Europe/Kyiv">"#));
        assert!(html.contains(r#"value="u2" data-email="taras@example.com" data-unreachable"#));
        assert!(html.contains(r#"id="participant-template""#));
    }

    #[test]
    fn lengths_show_in_the_unit_the_rotation_implies() {
        assert_eq!(length_in_unit(RotationType::Weekly, 1_209_600), "2");
        assert_eq!(length_in_unit(RotationType::Custom, 43_200), "12h");
        assert_eq!(length_in_unit(RotationType::Custom, 5_400), "90m");
    }

    #[test]
    fn handoff_prefills_in_the_schedule_zone() {
        let at = "2026-09-28T06:00:00Z".parse().unwrap();
        assert_eq!(local_input(at, "Europe/Kyiv"), "2026-09-28T09:00");
        assert_eq!(local_input(at, "not/a-zone"), "2026-09-28T06:00");
    }

    #[test]
    fn edit_form_renders_overrides_calendar() {
        let mut f = form("edit");
        f.submit_method = "PATCH";
        f.overrides = vec![OverrideModel {
            id: "oid".into(),
            user_id: "uid".into(),
            email: "a@b.c".into(),
            starts_at: "2026-06-01T00:00:00Z".parse().unwrap(),
            ends_at: "2026-06-02T00:00:00Z".parse().unwrap(),
        }];
        let html = ScheduleFormPage {
            active_tab: TAB_ON_CALL,
            form: f,
        }
        .render()
        .unwrap();
        assert!(html.contains("data-cal-grid"));
        assert!(html.contains(r#"data-override-id="oid""#));
    }
}
