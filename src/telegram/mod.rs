//! Central operator-owned Telegram bot: one bot links chats for every org via
//! a deep link. The bring-your-own `telegram` channel transport is separate
//! and unaffected.

mod budget;
mod client;
mod update;

pub use budget::{RateDeferred, TelegramSendBudget};
pub use client::{BotIdentity, TelegramClient};
pub use update::{ChatRef, Person, Press, Update, WebhookAction, classify_update};

use subtle::ConstantTimeEq;

use crate::config::AppConfig;
use crate::http_outbound::OutboundHttpClient;

/// Path the bot delivers updates to, appended to `auth.public_base_url`.
pub const WEBHOOK_PATH: &str = "/hooks/telegram";

/// Updates we ask Telegram to deliver: link messages, add/remove events and
/// button presses.
const ALLOWED_UPDATES: &[&str] = &["message", "my_chat_member", "callback_query"];

/// Constant-time match of the `X-Telegram-Bot-Api-Secret-Token` header against
/// the configured secret. The only thing authenticating the receiver.
pub fn webhook_secret_matches(provided: &str, expected: &str) -> bool {
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Opens a private chat with the bot and sends it `/start <payload>`.
pub fn start_link(bot_username: &str, payload: &str) -> String {
    format!(
        "https://t.me/{}?start={payload}",
        bot_username.trim_start_matches('@')
    )
}

/// Adds the bot to a group and sends it `/start <payload>` there.
pub fn start_group_link(bot_username: &str, payload: &str) -> String {
    format!(
        "https://t.me/{}?startgroup={payload}",
        bot_username.trim_start_matches('@')
    )
}

fn normalize_username(name: &str) -> String {
    name.trim().trim_start_matches('@').to_ascii_lowercase()
}

/// Confirm the configured bot, then register the webhook. Any failure is
/// logged and swallowed — a Telegram outage must not block a deploy, and the
/// next boot re-asserts. Idempotent: Telegram replaces an existing webhook.
pub async fn ensure_webhook(cfg: &AppConfig, http: OutboundHttpClient) {
    use secrecy::ExposeSecret;

    let tg = &cfg.telegram;
    let client = TelegramClient::new(http, tg.bot_token.expose_secret());

    let identity = match client.get_me().await {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(
                ?err,
                "telegram getMe failed — central bot disabled this boot"
            );
            return;
        }
    };
    let reported = identity.username.as_deref().unwrap_or_default();
    if normalize_username(reported) != normalize_username(&tg.bot_username) {
        tracing::warn!(
            configured = %tg.bot_username,
            reported = %reported,
            "telegram bot_username mismatch — central bot disabled this boot"
        );
        return;
    }

    let webhook_url = format!(
        "{}{WEBHOOK_PATH}",
        cfg.auth.public_base_url.trim_end_matches('/')
    );
    match client
        .set_webhook(
            &webhook_url,
            tg.webhook_secret.expose_secret(),
            ALLOWED_UPDATES,
        )
        .await
    {
        Ok(()) => tracing::info!(bot = %reported, "telegram webhook registered"),
        Err(err) => tracing::warn!(
            ?err,
            "telegram setWebhook failed — central bot disabled this boot"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_match_is_exact() {
        assert!(webhook_secret_matches("s3cret", "s3cret"));
        assert!(!webhook_secret_matches("s3cret", "s3creT"));
        assert!(!webhook_secret_matches("s3cret", "s3cret-extra"));
        assert!(!webhook_secret_matches("", "s3cret"));
    }

    #[test]
    fn username_normalizes_at_and_case() {
        assert_eq!(normalize_username("@UptimePageBot"), "uptimepagebot");
        assert_eq!(normalize_username(" uptimepagebot "), "uptimepagebot");
    }
}
