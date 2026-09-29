//! A press on the central bot's Acknowledge button, taken through
//! [`super::app_ack`]. Telegram answers the presser with a toast and the chat
//! with a reply to the alert.

use crate::app::AppState;
use crate::domain::{ChannelKind, Linked, LinkedApp};
use crate::security::app_link::external_id;
use crate::telegram::Press;

use super::app_ack::{GONE, Pressed, Taken, acknowledged_notice, announcement, take};
use super::telegram::{bot, spawn_send};

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
        spawn_send(
            state,
            press.chat_id,
            Some(press.message_id),
            announcement(press.person.display()),
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
    let place = press.chat_id.to_string();
    let pressed = Pressed {
        app: LinkedApp::Telegram,
        kind: ChannelKind::TelegramApp,
        place: &place,
        sender: external_id(&state.app_link_secret, &press.person.id.to_string()),
        data: &press.data,
    };
    match take(state, pressed).await {
        Taken::Acknowledged { listed, linked } => acknowledged(listed, linked, press.group),
        Taken::Refused(notice) => Outcome::quiet(notice),
    }
}

/// A press that adds someone to the list is news to a group; in a private chat
/// with the bot the presser is the only reader and already has the notice.
fn acknowledged(listed: bool, linked: Linked, group: bool) -> Outcome {
    Outcome {
        notice: acknowledged_notice(listed, linked, LINK_HINT),
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
