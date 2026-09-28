//! A press on the central bot's Acknowledge button. The org comes from the chat
//! the press arrived in, confirmed by the button's MAC. The person comes from
//! Telegram, and is named only through a Telegram account they linked.

use crate::app::AppState;
use crate::domain::{ChannelKind, LinkedApp};
use crate::security::app_link::external_id;
use crate::security::incident_ack::Button;
use crate::storage::linked_apps::{Linked, identify};
use crate::storage::{Acknowledged, Actor, AppPress, LifecycleOutcome};
use crate::telegram::Press;

use super::telegram::{bot, spawn_send};

const GONE: &str = "This button no longer works.";
const FAILED: &str = "Could not acknowledge. Try again, or use the app.";
const LINK_HINT: &str =
    "Link Telegram in your Uptimepage account settings so your next presses carry your name.";

/// What the presser sees, and whether the chat hears that someone took it.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    notice: String,
    announce: bool,
}

impl Outcome {
    fn quiet(notice: &str) -> Self {
        Self {
            notice: notice.to_string(),
            announce: false,
        }
    }
}

pub(super) async fn handle_press(state: &AppState, press: Press) {
    let outcome = acknowledge(state, &press).await;
    answer(state, &press.query_id, &outcome.notice).await;
    if outcome.announce {
        let who = press
            .person
            .display()
            .unwrap_or_else(|| "someone".to_string());
        spawn_send(
            state,
            press.chat_id,
            Some(press.message_id),
            format!("Acknowledged by {who}."),
        );
    }
}

pub(super) async fn answer_gone(state: &AppState, query_id: &str) {
    answer(state, query_id, GONE).await;
}

async fn answer(state: &AppState, query_id: &str, notice: &str) {
    if let Err(err) = bot(state).answer_callback_query(query_id, notice).await {
        tracing::warn!(?err, "telegram press answer failed");
    }
}

async fn acknowledge(state: &AppState, press: &Press) -> Outcome {
    let secret = state.incident_ack_secret.as_str();
    let Some(button) = Button::parse(&press.data).filter(|_| !secret.is_empty()) else {
        return Outcome::quiet(GONE);
    };
    let channels = match state
        .notification_channel_store
        .find_by_external_ref(ChannelKind::TelegramApp, &press.chat_id.to_string())
        .await
    {
        Ok(channels) => channels,
        Err(err) => {
            tracing::warn!(
                ?err,
                chat_id = press.chat_id,
                "telegram press channel lookup failed"
            );
            return Outcome::quiet(FAILED);
        }
    };
    let Some((org, _)) = channels
        .into_iter()
        .find(|(org, channel)| button.minted_for(secret, *org, *channel))
    else {
        return Outcome::quiet(GONE);
    };
    let sender = external_id(&state.app_link_secret, &press.person.id.to_string());
    let linked = identify(
        state.linked_app_store.as_ref(),
        org,
        LinkedApp::Telegram,
        sender,
    )
    .await;
    let acked = state
        .incident_ops_store
        .acknowledge(
            org,
            button.incident_id,
            Actor::App(AppPress {
                app: LinkedApp::Telegram,
                sender,
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
                named = linked.member().is_some(),
                listed,
                "incident acknowledged from telegram"
            );
            acknowledged(listed, linked, press.group)
        }
        // The responder lost a race with the recovery. Not an error.
        Ok(Acknowledged {
            outcome: LifecycleOutcome::IllegalTransition(_),
            ..
        }) => Outcome::quiet("This incident is already resolved."),
        Ok(Acknowledged {
            outcome: LifecycleOutcome::Stale,
            ..
        }) => Outcome::quiet("This alert is from an earlier outage, so nothing changed."),
        Ok(Acknowledged {
            outcome: LifecycleOutcome::NotFound,
            ..
        }) => Outcome::quiet(GONE),
        Err(err) => {
            tracing::warn!(org_id = %org.0, ?err, "telegram acknowledge failed");
            Outcome::quiet(FAILED)
        }
    }
}

/// A press that adds someone to the list is news to a group; in a private chat
/// with the bot the presser is the only reader and already has the notice.
fn acknowledged(listed: bool, linked: Linked, group: bool) -> Outcome {
    let hint = if linked.invites_link() {
        format!(" {LINK_HINT}")
    } else {
        String::new()
    };
    let notice = match (listed, linked.member().is_some()) {
        (true, true) => "Acknowledged.".to_string(),
        (true, false) => format!("Acknowledged.{hint}"),
        (false, true) => "You already acknowledged this.".to_string(),
        (false, false) => format!("You already acknowledged this.{hint}"),
    };
    Outcome {
        notice,
        announce: listed && group,
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::domain::UserId;

    #[test]
    fn only_a_press_that_joins_the_list_is_announced_and_only_to_a_group() {
        let olena = Linked::Member(UserId(Uuid::now_v7()));
        assert!(acknowledged(true, olena, true).announce);
        assert!(acknowledged(true, Linked::Unlinked, true).announce);
        assert!(!acknowledged(false, olena, true).announce);
        assert!(!acknowledged(false, Linked::Unlinked, true).announce);
        assert!(
            !acknowledged(true, olena, false).announce,
            "a private chat's only reader already has the notice"
        );
    }

    #[test]
    fn only_a_presser_nobody_linked_is_told_how_to_be_named() {
        let olena = Linked::Member(UserId(Uuid::now_v7()));
        assert!(
            acknowledged(true, Linked::Unlinked, true)
                .notice
                .contains("Link Telegram")
        );
        for linked in [olena, Linked::Outsider, Linked::Unknown] {
            for listed in [true, false] {
                assert!(
                    !acknowledged(listed, linked, true)
                        .notice
                        .contains("Link Telegram"),
                    "{linked:?}"
                );
            }
        }
        // Telegram cuts a callback answer at 200 characters.
        for linked in [olena, Linked::Unlinked] {
            for listed in [true, false] {
                assert!(acknowledged(listed, linked, true).notice.chars().count() <= 200);
            }
        }
    }
}
