//! Inbound webhook receiver for the central Telegram bot (`/hooks/telegram`).
//!
//! Telegram has no signature scheme — the secret-token header, compared in
//! constant time, is the only auth. Every accepted update is answered 200
//! fast (Telegram retries non-2xx aggressively), and the org a code links
//! to comes exclusively from the consumed code row, never the update body.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use secrecy::ExposeSecret;

use crate::api::error::codes;
use crate::app::AppState;
use crate::auth::sha256_hex;
use crate::domain::{ChannelConfig, ChannelKind, TelegramAppConfig};
use crate::error::{AppError, Result};
use crate::storage::LinkPurpose;
use crate::telegram::{
    ChatRef, TelegramClient, Update, WebhookAction, classify_update, webhook_secret_matches,
};
use crate::web::views::notification_channels::{QuotaBlockLog, create_channel_deduped};

const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";

pub async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let expected = state.cfg.telegram.webhook_secret.expose_secret();
    let provided = headers
        .get(SECRET_HEADER)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if expected.is_empty() || !webhook_secret_matches(provided, expected) {
        tracing::warn!("telegram webhook rejected: secret token mismatch");
        return StatusCode::FORBIDDEN;
    }

    let update = match serde_json::from_slice::<Update>(&body) {
        Ok(update) => update,
        Err(err) => {
            tracing::warn!(?err, "telegram webhook: unparseable update body");
            return StatusCode::OK;
        }
    };
    match classify_update(&update) {
        WebhookAction::LinkPrivate { code, chat } | WebhookAction::LinkGroup { code, chat } => {
            // Off the request: a slow ack makes Telegram re-deliver, racing
            // the first attempt's consume.
            tokio::spawn(async move { handle_link(&state, &code, chat).await });
        }
        WebhookAction::Stop { chat_id } => {
            tokio::spawn(async move { handle_stop(&state, chat_id).await });
        }
        WebhookAction::Removed { chat_id } => {
            tokio::spawn(async move { handle_removed(&state, chat_id).await });
        }
        WebhookAction::Migrated { from, to } => {
            tokio::spawn(async move { handle_migrated(&state, from, to).await });
        }
        WebhookAction::Ignore => {}
    }
    StatusCode::OK
}

const UNLINKED_NOTE: &str = "unlinked from the Telegram side";

/// Cross-org on purpose: several orgs can link one chat and a kick severs
/// all of them.
async fn unlink_chat(state: &AppState, chat_id: i64) -> u64 {
    match state
        .notification_channel_store
        .disable_by_external_ref(
            ChannelKind::TelegramApp,
            &chat_id.to_string(),
            UNLINKED_NOTE,
        )
        .await
    {
        Ok(n) => {
            if n > 0 {
                tracing::info!(chat_id, channels = n, "telegram chat unlinked");
            }
            n
        }
        Err(err) => {
            tracing::warn!(?err, chat_id, "telegram unlink failed");
            0
        }
    }
}

/// Unlike a kick, the bot can still confirm in-chat after `/stop`.
async fn handle_stop(state: &AppState, chat_id: i64) {
    let n = unlink_chat(state, chat_id).await;
    let text = if n > 0 {
        "Alerts stopped — this chat is unlinked. Link it again any time from the dashboard."
    } else {
        "Nothing is linked to this chat."
    };
    spawn_reply(state, chat_id, text.to_string());
}

/// Kicked/left/blocked: nobody left to reply to.
async fn handle_removed(state: &AppState, chat_id: i64) {
    unlink_chat(state, chat_id).await;
}

async fn handle_migrated(state: &AppState, from: i64, to: i64) {
    match state
        .notification_channel_store
        .follow_linked_chat_migration(&from.to_string(), &to.to_string())
        .await
    {
        Ok(n) if n > 0 => tracing::info!(from, to, channels = n, "telegram chat migrated"),
        Ok(_) => {}
        Err(err) => tracing::warn!(?err, from, to, "telegram chat migration failed"),
    }
}

async fn handle_link(state: &AppState, code: &str, chat: ChatRef) {
    let chat_id = chat.id;
    let text = match link_chat(state, code, chat).await {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(?err, chat_id, "telegram link consume failed");
            "Something went wrong while linking. Please mint a fresh link and try again."
                .to_string()
        }
    };
    spawn_reply(state, chat_id, text);
}

async fn link_chat(state: &AppState, code: &str, chat: ChatRef) -> Result<String> {
    let hash = sha256_hex(code);
    // A delegation code doubles as a t.me start payload, so the /c/ page
    // needs no second code: a successful link spends the delegate's
    // one-channel budget.
    let (link, delegated) = match state
        .channel_link_code_store
        .consume(LinkPurpose::Telegram, &hash)
        .await?
    {
        Some(l) => (l, false),
        None => match state
            .channel_link_code_store
            .consume(LinkPurpose::Delegate, &hash)
            .await?
        {
            Some(l) => (l, true),
            None => {
                return Ok(
                    "This link is invalid, expired, or already used. Mint a fresh link from \
                     the notification settings and try again."
                        .to_string(),
                );
            }
        },
    };
    let org = link.org_id;

    let base_name = link
        .channel_name
        .as_deref()
        .or(chat.title.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Telegram");
    let limit = i64::from(
        state
            .quotas
            .limit_for_org(org)
            .await?
            .max_notification_channels,
    );
    let config = ChannelConfig::TelegramApp(TelegramAppConfig {
        chat_id: chat.id.to_string(),
        chat_title: chat.title.clone(),
    });
    let channel = match create_channel_deduped(
        state.notification_channel_store.as_ref(),
        org,
        base_name,
        config,
        limit,
        QuotaBlockLog {
            db: state.db.clone(),
            user: None,
            flow: if delegated {
                "telegram_delegate"
            } else {
                "telegram_link"
            },
        },
    )
    .await
    {
        Ok(ch) => ch,
        Err(err) => {
            // A failed create must not burn a 7-day delegate invite; the
            // 15-minute telegram codes keep their burn-on-failure shape.
            if delegated {
                state.channel_link_code_store.restore(link.id).await?;
            }
            if let AppError::Unprocessable { code, .. } = &err
                && *code == codes::CHANNEL_QUOTA_EXCEEDED
            {
                return Ok(
                    "Couldn't link: this workspace is at its notification-channel limit. \
                     Remove an unused channel and mint a fresh link."
                        .to_string(),
                );
            }
            return Err(err);
        }
    };
    if let Err(err) = state
        .channel_link_code_store
        .attach_channel(link.id, channel.id)
        .await
    {
        // Channel exists and works; only the form's poll misses out.
        tracing::warn!(?err, channel_id = %channel.id, "telegram link attach failed");
    }
    if delegated {
        crate::web::views::delegate_connect::audit_delegated_create(state, org, &channel, "").await;
    }

    let org_name = match &state.db {
        Some(pool) => crate::storage::get_org(pool, org)
            .await
            .ok()
            .flatten()
            .map(|o| o.name),
        None => None,
    };
    Ok(match org_name {
        Some(name) => format!("Linked to {name} — alerts will arrive in this chat."),
        None => "Linked — alerts will arrive in this chat.".to_string(),
    })
}

fn spawn_reply(state: &AppState, chat_id: i64, text: String) {
    let client = TelegramClient::new(
        state.outbound_http.clone(),
        state.cfg.telegram.bot_token.expose_secret(),
    );
    let budget = state.telegram_send_budget.clone();
    tokio::spawn(async move {
        // A reply deferred past the budget's wait ceiling is dropped — a
        // late link confirmation is noise, and alerts keep their slots.
        if let Err(deferred) = budget.acquire(chat_id).await {
            tracing::warn!(chat_id, ?deferred, "telegram reply dropped by send budget");
            return;
        }
        if let Err(err) = client.send_message(chat_id, &text).await {
            tracing::warn!(?err, chat_id, "telegram link reply failed");
        }
    });
}
