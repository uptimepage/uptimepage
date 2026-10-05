use secrecy::ExposeSecret;

use crate::app::AppState;
use crate::telegram::TelegramClient;

pub(super) fn bot(state: &AppState) -> TelegramClient {
    TelegramClient::new(
        state.outbound_http.clone(),
        state.cfg.telegram.bot_token.expose_secret(),
    )
}

/// Message a chat, as a reply to `reply_to` when given.
pub(super) fn spawn_send(state: &AppState, chat_id: i64, reply_to: Option<i64>, text: String) {
    let client = bot(state);
    let budget = state.telegram_send_budget.clone();
    tokio::spawn(async move {
        // A reply deferred past the budget's wait ceiling is dropped — a late
        // confirmation is noise, and alerts keep their slots.
        if let Err(deferred) = budget.acquire(chat_id).await {
            tracing::warn!(chat_id, ?deferred, "telegram reply dropped by send budget");
            return;
        }
        let sent = match reply_to {
            Some(message_id) => client.send_reply(chat_id, message_id, &text).await,
            None => client.send_message(chat_id, &text).await,
        };
        if let Err(err) = sent {
            tracing::warn!(?err, chat_id, "telegram reply failed");
        }
    });
}
