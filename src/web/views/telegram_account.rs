//! `/start me-<code>`, opened from the account settings page. The Telegram
//! account that pressed Start is linked to the person who asked for the code,
//! so their presses on an Acknowledge button carry their name. A code links
//! once, and never takes a Telegram account from whoever already holds it.

use crate::app::AppState;
use crate::domain::{LinkedApp, UserId};
use crate::security::app_link::external_id;
use crate::security::sha256_hex;
use crate::storage::linked_apps::{Claimant, LinkOutcome};
use crate::telegram::Person;

use super::telegram_send::spawn_send;

pub(super) async fn handle_link(state: &AppState, code: &str, person: &Person, chat_id: i64) {
    let text = link(state, code, person).await;
    spawn_send(state, chat_id, None, text);
}

async fn link(state: &AppState, code: &str, person: &Person) -> String {
    let claimed = state
        .linked_app_store
        .claim(
            LinkedApp::Telegram,
            &sha256_hex(code),
            Claimant::Account {
                id: external_id(&state.app_link_secret, &person.id.to_string()),
                label: person.label().as_deref(),
            },
        )
        .await;
    let outcome = match claimed {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::warn!(?err, "telegram account link failed");
            return "Something went wrong while linking. Try the link again in a minute."
                .to_string();
        }
    };
    let email = match outcome {
        LinkOutcome::Linked(user) | LinkOutcome::AlreadyYours(user) => {
            account_email(state, user).await
        }
        LinkOutcome::Taken | LinkOutcome::Invalid => None,
    };
    if let LinkOutcome::Linked(user) = outcome {
        tracing::info!(user_id = %user.0, "telegram account linked");
    }
    reply(outcome, email.as_deref())
}

pub(super) async fn handle_unlink(state: &AppState, person: &Person, chat_id: i64) {
    let released = state
        .linked_app_store
        .release(
            LinkedApp::Telegram,
            external_id(&state.app_link_secret, &person.id.to_string()),
        )
        .await;
    let text = match released {
        Ok(true) => {
            tracing::info!("telegram account unlinked from the telegram side");
            "Unlinked. Acknowledge buttons you press in Telegram no longer carry anyone's name."
        }
        Ok(false) => "This Telegram account is not linked to anyone.",
        Err(err) => {
            tracing::warn!(?err, "telegram account unlink failed");
            "Something went wrong while unlinking. Try again in a minute."
        }
    };
    spawn_send(state, chat_id, None, text.to_string());
}

async fn account_email(state: &AppState, user: UserId) -> Option<String> {
    let pool = state.db.as_ref()?;
    crate::storage::users::live_email(pool, user)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(?err, "telegram link account lookup failed");
            None
        })
}

/// A link names the account, so someone who pressed Start on a code a
/// teammate sent them sees whose name their presses now carry. A refusal
/// never names whoever holds the Telegram account.
fn reply(outcome: LinkOutcome, email: Option<&str>) -> String {
    let account = email.map_or_else(|| "your Uptimepage account".to_string(), str::to_string);
    match outcome {
        LinkOutcome::Linked(_) => format!(
            "Linked to {account}. Acknowledge buttons you press in Telegram now carry that \
             name. Not your account? Send /unlink here."
        ),
        LinkOutcome::AlreadyYours(_) => {
            format!("This Telegram account is already linked to {account}.")
        }
        LinkOutcome::Taken => "This Telegram account is linked to another Uptimepage account. \
                               Send /unlink here to free it, then press this link again."
            .to_string(),
        LinkOutcome::Invalid => "This link has expired or was already used. Open your account \
                                 settings in Uptimepage and press link Telegram again."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn the_reply_names_only_the_account_this_code_linked() {
        let olena = UserId(Uuid::now_v7());
        assert!(
            reply(LinkOutcome::Linked(olena), Some("olena@example.com"))
                .starts_with("Linked to olena@example.com.")
        );
        assert!(
            reply(LinkOutcome::Linked(olena), None)
                .starts_with("Linked to your Uptimepage account.")
        );
        assert!(
            reply(LinkOutcome::AlreadyYours(olena), Some("olena@example.com"))
                .contains("already linked to olena@example.com")
        );
        assert!(!reply(LinkOutcome::Taken, None).contains('@'));
    }
}
