//! `/incidents/{id}/acknowledge`, where the Acknowledge button on a chat or
//! mail alert lands. The person signs in, so the ack names them. GET changes
//! nothing, so a link scanner, a prefetch or a page that navigates here takes
//! nothing and moves nobody to another org; the POST acknowledges, pinned to
//! the episode the alert was about, while the channel it came through still
//! offers the button.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{OpsIncident, OrgId, UserId};
use crate::notifier::ack_page::{AlertLink, AlertLinkQuery};
use crate::request::auth::{Session, login_redirect};
use crate::storage::{Actor, LifecycleOutcome};
use crate::templates::filters;
use crate::web::error::WebResult;

use super::actors::{AckList, ack_list};
use super::{fmt_secs, incident_label, members_map, state_label};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    Confirm,
    /// The viewer already acknowledged this episode.
    Taken,
    Resolved,
    /// The incident reopened since the alert went out.
    Stale,
    /// The channel the alert came through no longer offers the button:
    /// switched off, disabled or deleted since.
    Withdrawn,
    /// No such incident, or not in an org the viewer belongs to. One answer
    /// for both, so the page confirms nothing to an outsider.
    #[default]
    Missing,
}

#[derive(Template, WebTemplate, Default)]
#[template(path = "incidents/acknowledge.html")]
pub struct AcknowledgePage {
    pub phase: Phase,
    pub id: String,
    pub label: String,
    pub severity: &'static str,
    pub state_label: &'static str,
    pub open_for: Option<String>,
    /// A monitor recovering closes the incident; a declared one waits for a
    /// person.
    pub monitored: bool,
    pub acks: AckList,
    /// Where the button posts: this page's own link, rebuilt from its parts.
    pub action: String,
    /// The alert's org when the viewer is working in another one: "view
    /// incident" switches there first, on the click rather than on this GET.
    pub switch_org: Option<String>,
}

fn missing() -> Response {
    (StatusCode::NOT_FOUND, AcknowledgePage::default()).into_response()
}

/// Stale before resolved, the order the store refuses in: an alert from an
/// earlier outage is out of date whatever the incident did since. A withdrawn
/// button only replaces the offer, so the page still says what became of the
/// incident.
fn phase(
    inc: &OpsIncident,
    episode: i64,
    link: &AlertLink,
    acks: &AckList,
    offered: bool,
) -> Phase {
    if episode != link.episode {
        Phase::Stale
    } else if !inc.state.is_open() {
        Phase::Resolved
    } else if acks.mine {
        Phase::Taken
    } else if !offered {
        Phase::Withdrawn
    } else {
        Phase::Confirm
    }
}

/// Whether the viewer belongs to the alert's org, which need not be the one
/// they are working in.
async fn is_member(
    state: &AppState,
    session: &Session,
    user: UserId,
    org: OrgId,
) -> WebResult<bool> {
    // In-memory stores carry no memberships; the injected session is the
    // test's word, as it is for `CurrentOrg`.
    let Some(pool) = state.db.as_ref() else {
        return Ok(session.active_org_id == Some(org));
    };
    Ok(crate::storage::orgs::is_active_member(pool, user, org).await?)
}

/// Whether the channel the alert came through still offers the button.
async fn still_offered(state: &AppState, link: &AlertLink) -> WebResult<bool> {
    Ok(state
        .notification_channel_store
        .takes_acknowledgements(link.org, link.channel)
        .await?)
}

pub async fn acknowledge_page(
    State(state): State<AppState>,
    session: Session,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<Uuid>,
    Query(q): Query<AlertLinkQuery>,
) -> WebResult<Response> {
    let Some(user) = session.user_id() else {
        let back = uri.path_and_query().map_or(uri.path(), |p| p.as_str());
        return Ok(login_redirect(back).into_response());
    };
    let Some(link) = q.link() else {
        return Ok(missing());
    };
    if !is_member(&state, &session, user, link.org).await? {
        return Ok(missing());
    }
    let org = link.org;
    let Some(inc) = state.incident_ops_store.get(org, id).await? else {
        return Ok(missing());
    };
    let (members, mut acks, episode, monitor_name, offered) = tokio::try_join!(
        members_map(&state, org),
        async {
            Ok(state
                .incident_ops_store
                .acknowledgements(org, &[id])
                .await?)
        },
        async { Ok(state.incident_ops_store.generation(org, id).await?) },
        async {
            Ok(match inc.target_id {
                Some(t) => state.target_store.get(org, t).await?.map(|x| x.name),
                None => None,
            })
        },
        still_offered(&state, &link),
    )?;
    let Some(episode) = episode else {
        return Ok(missing());
    };
    let acks = ack_list(&acks.remove(&id).unwrap_or_default(), user, &members);
    let ongoing = inc.state.is_open();
    Ok(AcknowledgePage {
        phase: phase(&inc, episode, &link, &acks, offered),
        id: id.to_string(),
        label: incident_label(inc.title.clone(), monitor_name),
        severity: inc.severity.as_db_str(),
        state_label: state_label(inc.state),
        open_for: ongoing
            .then(|| {
                fmt_secs(Some(
                    (Utc::now() - inc.started_at).num_seconds().max(0) as f64
                ))
            })
            .flatten(),
        monitored: inc.target_id.is_some(),
        acks,
        action: link.path(id),
        switch_org: (session.active_org_id != Some(org)).then(|| org.0.to_string()),
    }
    .into_response())
}

/// The status is the whole answer: the page reloads and explains it.
pub async fn acknowledge(
    State(state): State<AppState>,
    session: Session,
    Path(id): Path<Uuid>,
    Query(q): Query<AlertLinkQuery>,
) -> WebResult<StatusCode> {
    let Some(user) = session.user_id() else {
        return Ok(StatusCode::UNAUTHORIZED);
    };
    let Some(link) = q.link() else {
        return Ok(StatusCode::NOT_FOUND);
    };
    let (member, offered) = tokio::try_join!(
        is_member(&state, &session, user, link.org),
        still_offered(&state, &link),
    )?;
    if !member {
        return Ok(StatusCode::NOT_FOUND);
    }
    if !offered {
        return Ok(StatusCode::CONFLICT);
    }
    let outcome = state
        .incident_ops_store
        .acknowledge(link.org, id, Actor::User(user), None, Some(link.episode))
        .await?
        .outcome;
    Ok(match outcome {
        LifecycleOutcome::Updated(_) => {
            tracing::info!(
                org_id = %link.org.0,
                incident_id = %id,
                "incident acknowledged from an alert's acknowledge page"
            );
            StatusCode::NO_CONTENT
        }
        LifecycleOutcome::Stale | LifecycleOutcome::IllegalTransition(_) => StatusCode::CONFLICT,
        LifecycleOutcome::NotFound => StatusCode::NOT_FOUND,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::IncidentState;

    fn incident(state: IncidentState) -> OpsIncident {
        let mut inc = super::super::tests::ops(state);
        inc.title = Some("db unreachable".into());
        inc
    }

    fn link(episode: i64) -> AlertLink {
        AlertLink {
            org: OrgId(Uuid::nil()),
            channel: Uuid::nil(),
            episode,
        }
    }

    fn mine(mine: bool) -> AckList {
        AckList {
            ackers: Vec::new(),
            mine,
        }
    }

    #[test]
    fn an_alert_from_an_earlier_outage_takes_nothing_whatever_came_since() {
        for state in [IncidentState::Triggered, IncidentState::Resolved] {
            let p = phase(&incident(state), 2, &link(1), &mine(false), true);
            assert_eq!(p, Phase::Stale, "{state:?}");
        }
    }

    /// A withdrawn button only takes away the offer: what became of the
    /// incident still reads as it would.
    #[test]
    fn a_withdrawn_button_replaces_only_the_offer() {
        let open = incident(IncidentState::Triggered);
        assert_eq!(
            phase(&open, 1, &link(1), &mine(false), false),
            Phase::Withdrawn
        );
        assert_eq!(phase(&open, 1, &link(1), &mine(true), false), Phase::Taken);
        let closed = incident(IncidentState::Resolved);
        assert_eq!(
            phase(&closed, 1, &link(1), &mine(false), false),
            Phase::Resolved
        );
        assert_eq!(phase(&open, 2, &link(1), &mine(false), false), Phase::Stale);
    }

    #[test]
    fn the_current_episode_offers_the_button_until_the_viewer_took_it() {
        let inc = incident(IncidentState::Acknowledged);
        assert_eq!(phase(&inc, 1, &link(1), &mine(false), true), Phase::Confirm);
        assert_eq!(phase(&inc, 1, &link(1), &mine(true), true), Phase::Taken);
        let closed = incident(IncidentState::Resolved);
        assert_eq!(
            phase(&closed, 1, &link(1), &mine(false), true),
            Phase::Resolved
        );
    }

    fn render(page: AcknowledgePage) -> String {
        page.render().unwrap()
    }

    fn sample(phase: Phase) -> AcknowledgePage {
        AcknowledgePage {
            phase,
            id: "7".into(),
            label: "db unreachable".into(),
            severity: "major",
            state_label: "triggered",
            open_for: Some("12m 0s".into()),
            monitored: true,
            acks: mine(false),
            action: "/incidents/7/acknowledge?org=1&channel=2&episode=0".into(),
            switch_org: None,
        }
    }

    fn page(phase: Phase) -> String {
        render(sample(phase))
    }

    /// The page itself never acknowledges: only the button's POST does.
    #[test]
    fn only_the_confirm_phase_offers_the_button() {
        let confirm = page(Phase::Confirm);
        assert!(confirm.contains("data-incident-acknowledge"), "{confirm}");
        assert!(
            confirm.contains(
                r#"data-action="/incidents/7/acknowledge?org=1&#38;channel=2&#38;episode=0""#
            ),
            "{confirm}"
        );
        assert!(confirm.contains("incident_acknowledge"), "script loaded");
        for other in [
            Phase::Taken,
            Phase::Resolved,
            Phase::Stale,
            Phase::Withdrawn,
        ] {
            let html = page(other);
            assert!(!html.contains("data-incident-acknowledge"), "{other:?}");
            assert!(html.contains(r#"href="/incidents/7""#), "{other:?}");
        }
    }

    #[test]
    fn a_stale_alert_says_it_is_from_an_earlier_outage() {
        assert!(page(Phase::Stale).contains("earlier outage"));
    }

    #[test]
    fn a_withdrawn_alert_says_its_channel_no_longer_offers_the_button() {
        let html = page(Phase::Withdrawn);
        assert!(html.contains("no longer offers the Acknowledge"), "{html}");
    }

    /// Stopping reminders already happened, so a second responder is told
    /// they would join the list rather than halt anything.
    #[test]
    fn a_second_responder_is_told_someone_already_took_it() {
        let mut taken = sample(Phase::Confirm);
        taken.acks = AckList {
            ackers: vec![super::super::actors::AckerView {
                name: "ana@acme.test".into(),
                phrase: "by ana@acme.test".into(),
                avatar: None,
                at: Utc::now(),
            }],
            mine: false,
        };
        let html = render(taken);
        assert!(html.contains("Someone already took this"), "{html}");
        assert!(!html.contains("escalation ladder halts"), "{html}");
    }

    /// Their own acknowledgement is on the timeline, so "too late" would
    /// contradict it.
    #[test]
    fn a_responder_who_took_it_is_not_told_they_came_too_late() {
        assert!(page(Phase::Resolved).contains("before you got here"));
        let mut theirs = sample(Phase::Resolved);
        theirs.acks = mine(true);
        let html = render(theirs);
        assert!(html.contains("You acknowledged this"), "{html}");
        assert!(!html.contains("before you got here"), "{html}");
    }

    /// Nothing closes a declared incident but a person.
    #[test]
    fn a_declared_incident_is_not_promised_to_close_itself() {
        assert!(page(Phase::Taken).contains("leave it to close itself"));
        let mut declared = sample(Phase::Taken);
        declared.monitored = false;
        let html = render(declared);
        assert!(!html.contains("close itself"), "{html}");
        assert!(html.contains("until someone"), "{html}");
    }

    /// Opening the page moves nobody; the switch waits for the click.
    #[test]
    fn view_incident_switches_org_only_when_the_alert_is_from_another() {
        assert!(!page(Phase::Stale).contains("data-switch-org"));
        let mut elsewhere = sample(Phase::Stale);
        elsewhere.switch_org = Some("org-b".into());
        let html = render(elsewhere);
        assert!(html.contains(r#"data-switch-org="org-b""#), "{html}");
        assert!(html.contains("incident_acknowledge"), "script loaded");
    }
}
