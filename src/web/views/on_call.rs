//! Server-rendered on-call pages under `/settings/on-call`: a schedule list
//! with a live "who's on call" readout, a schedule builder (layers, ordered
//! participants, rotation), and an overrides calendar on the edit page.
//!
//! Mutations run from the page against the JSON API (`/api/v1/on-call/...`), so
//! this module renders chrome, the member/channel choices, and prefills the
//! builder. Editing is owner-only — the API enforces it; a non-owner sees the
//! page but a mutation returns 403, mirroring the escalation surface.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{OrgId, RotationType};
use crate::error::AppError;
use crate::request::CurrentOrg;
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::resolve_org;

const TAB_ON_CALL: &str = "on-call";

/// One org member offered as a participant / override coverer.
pub struct MemberChoice {
    pub id: String,
    pub email: String,
}

/// A member checkbox within a layer, pre-checked when already a participant.
pub struct ParticipantChoice {
    pub id: String,
    pub email: String,
    pub checked: bool,
}

fn participant_choices(members: &[MemberChoice], selected: &[String]) -> Vec<ParticipantChoice> {
    members
        .iter()
        .map(|m| ParticipantChoice {
            id: m.id.clone(),
            email: m.email.clone(),
            checked: selected.contains(&m.id),
        })
        .collect()
}

pub struct ScheduleRow {
    pub id: String,
    pub name: String,
    pub timezone: String,
    pub layer_count: i64,
    pub created: chrono::DateTime<chrono::Utc>,
}

/// A notification channel the signed-in member may opt into being paged on.
pub struct ContactChoice {
    pub id: String,
    pub name: String,
    pub checked: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/on_call.html")]
pub struct OnCallPage {
    pub active_tab: &'static str,
    /// The member's own paging contacts, every org channel offered as a toggle.
    pub contacts: Vec<ContactChoice>,
    pub no_channels: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/on_call_partial.html")]
pub struct OnCallPartial {
    pub schedules: Vec<ScheduleRow>,
    /// Member id → email, rendered hidden so the who-on-call widget can name
    /// the user ids the resolver returns without a second request.
    pub members: Vec<MemberChoice>,
}

/// One layer prefilled in the builder. Participants carry the member id so the
/// editor can pre-check them; the JS serialises the ordered selection.
pub struct LayerModel {
    pub name: String,
    pub rotation_type: &'static str,
    /// Friendly length in the unit implied by `rotation_type` (days/weeks/secs).
    pub rotation_length: i64,
    pub handoff_at: String,
    pub participants: Vec<ParticipantChoice>,
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
    pub members: Vec<MemberChoice>,
    pub layers: Vec<LayerModel>,
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
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    user: crate::request::CurrentUser,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/on-call") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let channels = state.notification_channel_store.list(org).await?;
    let mine = state.contact_store.for_user(org, user.0).await?;
    let contacts = channels
        .into_iter()
        .map(|c| ContactChoice {
            checked: mine.contains(&c.id),
            id: c.id.to_string(),
            name: c.name,
        })
        .collect::<Vec<_>>();
    Ok(OnCallPage {
        active_tab: TAB_ON_CALL,
        no_channels: contacts.is_empty(),
        contacts,
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
    let schedules = state
        .on_call_store
        .list(org)
        .await?
        .into_iter()
        .map(|s| ScheduleRow {
            id: s.id.to_string(),
            name: s.name,
            timezone: s.timezone,
            layer_count: s.layer_count,
            created: s.created_at,
        })
        .collect();
    let members = org_members(&state, org).await?;
    Ok(OnCallPartial { schedules, members }.into_response())
}

/// Org members as builder choices. Empty without a DB (single-tenant dev).
async fn org_members(state: &AppState, org: OrgId) -> WebResult<Vec<MemberChoice>> {
    let Some(pool) = &state.db else {
        return Ok(vec![]);
    };
    Ok(crate::storage::orgs::list_members(pool, org)
        .await?
        .into_iter()
        .map(|m| MemberChoice {
            id: m.membership.user_id.to_string(),
            email: m.email,
        })
        .collect())
}

/// One empty daily layer to seed a fresh builder; every member offered unchecked.
fn empty_layer(members: &[MemberChoice]) -> LayerModel {
    LayerModel {
        name: String::new(),
        rotation_type: "daily",
        rotation_length: 7,
        handoff_at: String::new(),
        participants: participant_choices(members, &[]),
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
    let members = org_members(&state, org).await?;
    let form = ScheduleFormModel {
        mode: "create",
        action: "/api/v1/on-call/schedules".into(),
        submit_method: "POST",
        schedule_id: String::new(),
        name: String::new(),
        timezone: "UTC".into(),
        layers: vec![empty_layer(&members)],
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

/// Split a layer's stored second-count back into the unit its rotation type
/// implies, so the editor shows "every 1 week" not "every 604800 seconds".
fn length_in_unit(rotation: RotationType, secs: i32) -> i64 {
    let secs = i64::from(secs);
    match rotation {
        RotationType::Daily => secs / 86_400,
        RotationType::Weekly => secs / 604_800,
        RotationType::Custom => secs,
    }
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
    let email_of = |uid: &str| -> String {
        members
            .iter()
            .find(|m| m.id == uid)
            .map(|m| m.email.clone())
            .unwrap_or_else(|| "(removed member)".into())
    };
    let mut layers: Vec<LayerModel> = detail
        .layers
        .iter()
        .map(|l| {
            let mut participants: Vec<&crate::domain::OnCallParticipant> =
                l.participants.iter().collect();
            participants.sort_by_key(|p| p.position);
            let selected: Vec<String> =
                participants.iter().map(|p| p.user_id.to_string()).collect();
            LayerModel {
                name: l.name.clone().unwrap_or_default(),
                rotation_type: l.rotation_type.as_db_str(),
                rotation_length: length_in_unit(l.rotation_type, l.rotation_length_secs),
                handoff_at: l.handoff_at.to_rfc3339(),
                participants: participant_choices(&members, &selected),
            }
        })
        .collect();
    if layers.is_empty() {
        layers.push(empty_layer(&members));
    }
    let overrides = detail
        .overrides
        .iter()
        .map(|o| OverrideModel {
            id: o.id.to_string(),
            user_id: o.user_id.to_string(),
            email: email_of(&o.user_id.to_string()),
            starts_at: o.starts_at,
            ends_at: o.ends_at,
        })
        .collect();
    let form = ScheduleFormModel {
        mode: "edit",
        action: format!("/api/v1/on-call/schedules/{}", detail.schedule.id),
        submit_method: "PATCH",
        schedule_id: detail.schedule.id.to_string(),
        name: detail.schedule.name,
        timezone: detail.schedule.timezone,
        no_members: members.is_empty(),
        members,
        layers,
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

    #[test]
    fn list_renders_who_widget_and_member_map() {
        let html = OnCallPartial {
            schedules: vec![ScheduleRow {
                id: "sid".into(),
                name: "Primary".into(),
                timezone: "UTC".into(),
                layer_count: 2,
                created: "2026-06-04T00:00:00Z".parse().unwrap(),
            }],
            members: vec![MemberChoice {
                id: "uid".into(),
                email: "a@b.c".into(),
            }],
        }
        .render()
        .unwrap();
        assert!(html.contains("Primary"));
        assert!(html.contains(r#"data-who-on-call"#));
        assert!(html.contains(r#"data-schedule-id="sid""#));
        assert!(html.contains(r#"data-member-email="a@b.c""#));
    }

    #[test]
    fn empty_list_renders_onboarding() {
        let html = OnCallPartial {
            schedules: vec![],
            members: vec![],
        }
        .render()
        .unwrap();
        assert!(html.contains("No on-call schedules yet"));
    }

    #[test]
    fn page_renders_contact_card() {
        let html = OnCallPage {
            active_tab: TAB_ON_CALL,
            no_channels: false,
            contacts: vec![ContactChoice {
                id: "cid".into(),
                name: "Ops Slack".into(),
                checked: true,
            }],
        }
        .render()
        .unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("channels that page you"));
        assert!(html.contains(r#"data-contact value="cid" checked"#));
    }

    fn form(mode: &'static str) -> ScheduleFormModel {
        ScheduleFormModel {
            mode,
            action: "/api/v1/on-call/schedules".into(),
            submit_method: "POST",
            schedule_id: "sid".into(),
            name: "Primary".into(),
            timezone: "UTC".into(),
            members: vec![MemberChoice {
                id: "uid".into(),
                email: "a@b.c".into(),
            }],
            layers: vec![LayerModel {
                name: String::new(),
                rotation_type: "daily",
                rotation_length: 7,
                handoff_at: String::new(),
                participants: vec![ParticipantChoice {
                    id: "uid".into(),
                    email: "a@b.c".into(),
                    checked: true,
                }],
            }],
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
