//! A press on an Acknowledge button whose press reaches our own app, in a
//! Telegram chat or a Slack channel. The org comes from the chat or channel the
//! press arrived in, confirmed by the button's MAC. The person comes from the
//! app, and is named only through an account they linked there.

use crate::app::AppState;
use crate::domain::{ChannelKind, ExternalId, LinkedApp};
use crate::security::incident_ack::Button;
use crate::storage::linked_apps::{Linked, identify};
use crate::storage::{Acknowledged, Actor, AppPress, LifecycleOutcome};

pub(super) const GONE: &str = "This button no longer works.";
pub(super) const FAILED: &str = "Could not acknowledge. Try again, or use the app.";

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
    Acknowledged { listed: bool, linked: Linked },
    /// Nothing changed, for the reason the presser is told.
    Refused(&'static str),
}

pub(super) async fn take(state: &AppState, press: Pressed<'_>) -> Taken {
    let app = press.app.as_db_str();
    let secret = state.incident_ack_secret.as_str();
    let Some(button) = Button::parse(press.data).filter(|_| !secret.is_empty()) else {
        return Taken::Refused(GONE);
    };
    let channels = match state
        .notification_channel_store
        .acknowledging_by_external_ref(press.kind, press.place)
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
            return Taken::Refused(FAILED);
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
    let acked = state
        .incident_ops_store
        .acknowledge(
            org,
            button.incident_id,
            Actor::App(AppPress {
                app: press.app,
                sender: press.sender,
                member: linked.member(),
            }),
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
        // The responder lost a race with the recovery. Not an error.
        Ok(Acknowledged {
            outcome: LifecycleOutcome::IllegalTransition(_),
            ..
        }) => Taken::Refused("This incident is already resolved."),
        Ok(Acknowledged {
            outcome: LifecycleOutcome::Stale,
            ..
        }) => Taken::Refused("This alert is from an earlier outage, so nothing changed."),
        Ok(Acknowledged {
            outcome: LifecycleOutcome::NotFound,
            ..
        }) => Taken::Refused(GONE),
        Err(err) => {
            tracing::warn!(org_id = %org.0, ?err, app, "app acknowledge failed");
            Taken::Refused(FAILED)
        }
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
}
