//! A press on an Acknowledge or Resolve button whose press reaches our own
//! app, in a Telegram chat, a Slack channel or under a Discord webhook's
//! alert. The org comes from where the press arrived, confirmed by the
//! button's MAC. The person comes from the app, and is named only through an
//! account they linked there. Acknowledging takes anyone and names whom it
//! can; resolving takes a linked member, since it closes the incident for
//! everyone.

use chrono::Utc;

use crate::app::AppState;
use crate::app_accounts::{identify, offer_link};
use crate::domain::{
    AlertAction, ChannelKind, ExternalId, Linked, LinkedApp, NotificationReason, OrgId,
};
use crate::security::incident_ack::Button;
use crate::storage::{Acknowledged, Actor, AppPress, LifecycleOutcome};

pub(super) const GONE: &str = "This button no longer works.";
const UNNAMED: &str =
    "Nothing changed: resolving takes an account linked to a member of this organization.";

/// A press, as the app reports it.
pub(super) struct Pressed<'a> {
    pub app: LinkedApp,
    pub kind: ChannelKind,
    /// The chat or channel it came from, as the app names it.
    pub place: &'a str,
    pub sender: ExternalId,
    /// The button's signed value.
    pub data: &'a str,
}

pub(super) enum Taken {
    /// `listed` when the press added someone to the incident's list.
    Acknowledged {
        listed: bool,
        linked: Linked,
    },
    Resolved,
    /// A Resolve press from an account no member of the org linked.
    Unnamed(Linked),
    /// Nothing changed, for the reason the presser is told.
    Refused(&'static str),
}

fn failed(action: AlertAction) -> &'static str {
    match action {
        AlertAction::Acknowledge => "Could not acknowledge. Try again, or use the app.",
        AlertAction::Resolve => "Could not resolve. Try again, or use the app.",
    }
}

pub(super) async fn take(state: &AppState, press: Pressed<'_>) -> Taken {
    let app = press.app.as_db_str();
    let secret = state.incident_ack_secret.as_str();
    let Some(button) = Button::parse(press.data).filter(|_| !secret.is_empty()) else {
        return Taken::Refused(GONE);
    };
    let action = button.action;
    let channels = match state
        .notification_channel_store
        .taking_by_external_ref(press.kind, press.place, action)
        .await
    {
        Ok(channels) => channels,
        Err(err) => {
            tracing::warn!(
                ?err,
                app,
                place = press.place,
                "press channel lookup failed"
            );
            return Taken::Refused(failed(action));
        }
    };
    let Some((org, _)) = channels
        .into_iter()
        .find(|(org, channel)| button.minted_for(secret, *org, *channel))
    else {
        return Taken::Refused(GONE);
    };
    let linked = identify(
        state.linked_app_store.as_ref(),
        org,
        press.app,
        press.sender,
    )
    .await;
    match action {
        AlertAction::Acknowledge => acknowledge(state, org, &button, &press, linked).await,
        AlertAction::Resolve => resolve(state, org, &button, &press, linked).await,
    }
}

fn actor(press: &Pressed<'_>, linked: Linked) -> Actor {
    Actor::App(AppPress {
        app: press.app,
        sender: press.sender,
        member: linked.member(),
    })
}

async fn acknowledge(
    state: &AppState,
    org: OrgId,
    button: &Button,
    press: &Pressed<'_>,
    linked: Linked,
) -> Taken {
    let app = press.app.as_db_str();
    let acked = state
        .incident_ops_store
        .acknowledge(
            org,
            button.incident_id,
            actor(press, linked),
            None,
            Some(button.generation),
        )
        .await;
    match acked {
        Ok(Acknowledged {
            outcome: LifecycleOutcome::Updated(_),
            listed,
        }) => {
            tracing::info!(
                org_id = %org.0,
                incident_id = %button.incident_id,
                app,
                named = linked.member().is_some(),
                listed,
                "incident acknowledged from an app"
            );
            Taken::Acknowledged { listed, linked }
        }
        Ok(Acknowledged { outcome, .. }) => Taken::Refused(refusal(outcome)),
        Err(err) => {
            tracing::warn!(org_id = %org.0, ?err, app, "app acknowledge failed");
            Taken::Refused(failed(AlertAction::Acknowledge))
        }
    }
}

async fn resolve(
    state: &AppState,
    org: OrgId,
    button: &Button,
    press: &Pressed<'_>,
    linked: Linked,
) -> Taken {
    let app = press.app.as_db_str();
    match linked {
        Linked::Member(_) => {}
        // Not knowing is no reason to send a linked member off to link again.
        Linked::Unknown => return Taken::Refused(failed(AlertAction::Resolve)),
        Linked::Outsider | Linked::Unlinked => return Taken::Unnamed(linked),
    }
    let resolved = state
        .incident_ops_store
        .resolve_episode(
            org,
            button.incident_id,
            actor(press, linked),
            button.generation,
        )
        .await;
    match resolved {
        Ok(LifecycleOutcome::Updated(_)) => {
            tracing::info!(
                org_id = %org.0,
                incident_id = %button.incident_id,
                app,
                "incident resolved from an app"
            );
            state.signal_incident(org, button.incident_id, NotificationReason::Resolved);
            Taken::Resolved
        }
        Ok(outcome) => Taken::Refused(refusal(outcome)),
        Err(err) => {
            tracing::warn!(org_id = %org.0, ?err, app, "app resolve failed");
            Taken::Refused(failed(AlertAction::Resolve))
        }
    }
}

/// Why a press that changed nothing is refused. A responder who lost a race
/// with the recovery is not an error.
fn refusal(outcome: LifecycleOutcome) -> &'static str {
    match outcome {
        LifecycleOutcome::IllegalTransition(_) => "This incident is already resolved.",
        LifecycleOutcome::Stale => "This alert is from an earlier outage, so nothing changed.",
        LifecycleOutcome::NotFound | LifecycleOutcome::Updated(_) => GONE,
    }
}

/// What the presser is told about a press in Slack or Discord, and what the
/// channel then hears about: someone joining the list, or the incident
/// closing. A presser nobody linked is offered a one-time link naming them on
/// their next presses, written by `link` into a message only they see.
pub(super) async fn answer_offering_link(
    state: &AppState,
    press: Pressed<'_>,
    username: Option<&str>,
    link: impl Fn(&str) -> String,
) -> (String, Option<AlertAction>) {
    let (app, sender) = (press.app, press.sender);
    let hint = |linked: Linked| async move {
        match linked.invites_link() {
            true => offered_link(state, app, sender, username)
                .await
                .map(|url| link(&url))
                .unwrap_or_default(),
            false => String::new(),
        }
    };
    match take(state, press).await {
        Taken::Acknowledged { listed, linked } => (
            acknowledged_notice(listed, linked, &hint(linked).await),
            listed.then_some(AlertAction::Acknowledge),
        ),
        Taken::Resolved => ("Resolved.".to_string(), Some(AlertAction::Resolve)),
        Taken::Unnamed(linked) => (unnamed_notice(linked, &hint(linked).await), None),
        Taken::Refused(notice) => (notice.to_string(), None),
    }
}

async fn offered_link(
    state: &AppState,
    app: LinkedApp,
    sender: ExternalId,
    username: Option<&str>,
) -> Option<String> {
    offer_link(
        state.linked_app_store.as_ref(),
        &state.cfg.auth.public_base_url,
        app,
        sender,
        username,
        Utc::now(),
    )
    .await
    .map(|o| o.url)
}

/// What the chat hears when a press adds someone to the list or closes the
/// incident. The caller escapes `who` for its app's markup; Telegram replies
/// are plain text. A name that already ends a sentence, like `Olena K.`, gets
/// no second stop.
pub(super) fn announcement(action: AlertAction, who: Option<String>) -> String {
    let who = who.as_deref().map_or("someone", str::trim_end);
    let stop = if who.ends_with(['.', '!', '?', '…']) {
        ""
    } else {
        "."
    };
    let did = match action {
        AlertAction::Acknowledge => "Acknowledged",
        AlertAction::Resolve => "Resolved",
    };
    format!("{did} by {who}{stop}")
}

/// What a Resolve press from an account nobody linked is told. `hint`
/// follows when nobody linked the account at all.
pub(super) fn unnamed_notice(linked: Linked, hint: &str) -> String {
    if linked.invites_link() && !hint.is_empty() {
        format!("{UNNAMED} {hint}")
    } else {
        UNNAMED.to_string()
    }
}

/// What the presser is told about a press that landed. `hint` follows when
/// nobody linked the account that pressed.
pub(super) fn acknowledged_notice(listed: bool, linked: Linked, hint: &str) -> String {
    let hint = if linked.invites_link() && !hint.is_empty() {
        format!(" {hint}")
    } else {
        String::new()
    };
    if listed {
        format!("Acknowledged.{hint}")
    } else {
        format!("You already acknowledged this.{hint}")
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::domain::UserId;

    #[test]
    fn only_a_presser_nobody_linked_is_told_how_to_be_named() {
        let olena = Linked::Member(UserId(Uuid::now_v7()));
        assert_eq!(
            acknowledged_notice(true, Linked::Unlinked, "Link it."),
            "Acknowledged. Link it."
        );
        assert_eq!(
            acknowledged_notice(false, Linked::Unlinked, "Link it."),
            "You already acknowledged this. Link it."
        );
        for linked in [olena, Linked::Outsider, Linked::Unknown] {
            for listed in [true, false] {
                assert!(
                    !acknowledged_notice(listed, linked, "Link it.").contains("Link"),
                    "{linked:?}"
                );
            }
        }
        assert_eq!(
            acknowledged_notice(true, Linked::Unlinked, ""),
            "Acknowledged."
        );
    }

    #[test]
    fn an_announcement_names_the_presser_or_someone() {
        assert_eq!(
            announcement(AlertAction::Acknowledge, Some("Olena".into())),
            "Acknowledged by Olena."
        );
        assert_eq!(
            announcement(AlertAction::Acknowledge, None),
            "Acknowledged by someone."
        );
        assert_eq!(
            announcement(AlertAction::Acknowledge, Some("Olena (SRE)".into())),
            "Acknowledged by Olena (SRE)."
        );
    }

    #[test]
    fn a_name_that_ends_a_sentence_gets_no_second_stop() {
        let said = |name: &str| announcement(AlertAction::Acknowledge, Some(name.into()));
        assert_eq!(said("Olena K."), "Acknowledged by Olena K.");
        assert_eq!(said("Taras!"), "Acknowledged by Taras!");
        assert_eq!(said("Olena…"), "Acknowledged by Olena…");
        assert_eq!(said("?"), "Acknowledged by ?");
        assert_eq!(said("Olena K. "), "Acknowledged by Olena K.");
    }

    #[test]
    fn closing_an_incident_is_announced_as_resolved() {
        assert_eq!(
            announcement(AlertAction::Resolve, Some("Olena".into())),
            "Resolved by Olena."
        );
        assert_eq!(
            announcement(AlertAction::Resolve, None),
            "Resolved by someone."
        );
    }

    #[test]
    fn only_an_account_nobody_linked_is_offered_a_link_to_resolve() {
        assert!(unnamed_notice(Linked::Unlinked, "Link it.").ends_with("Link it."));
        for linked in [Linked::Outsider, Linked::Unknown] {
            assert_eq!(unnamed_notice(linked, "Link it."), UNNAMED, "{linked:?}");
        }
        assert_eq!(unnamed_notice(Linked::Unlinked, ""), UNNAMED);
    }
}
