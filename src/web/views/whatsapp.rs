//! Inbound webhook receiver for the operator WhatsApp number
//! (`/hooks/whatsapp`).
//!
//! Meta signs every POST with `X-Hub-Signature-256` (HMAC over the raw
//! body, app secret as key) — that signature is the only auth. The one-time
//! GET is Meta's subscribe handshake: echo `hub.challenge` when the verify
//! token matches. Accepted deliveries are answered 200 fast (Meta retries
//! non-2xx with backoff and disables flapping endpoints), and the org a
//! code links to comes exclusively from the consumed code row, never the
//! notification body.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use secrecy::ExposeSecret;
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::app::AppState;
use crate::auth::sha256_hex;
use crate::domain::{ChannelConfig, ChannelKind, WhatsAppAppConfig};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::storage::LinkPurpose;
use crate::web::views::notification_channels::{QuotaBlockLog, create_channel_deduped};
use crate::whatsapp::{InboundAction, Notification, classify, send_text, signature_matches};

const SIGNATURE_HEADER: &str = "x-hub-signature-256";

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    #[serde(rename = "hub.mode", default)]
    pub mode: String,
    #[serde(rename = "hub.verify_token", default)]
    pub verify_token: String,
    #[serde(rename = "hub.challenge", default)]
    pub challenge: String,
}

/// Meta's one-time subscribe handshake.
pub async fn verify(State(state): State<AppState>, Query(q): Query<VerifyQuery>) -> Response {
    let expected = state.cfg.whatsapp_app.verify_token.expose_secret();
    let ok = q.mode == "subscribe"
        && !expected.is_empty()
        && bool::from(q.verify_token.as_bytes().ct_eq(expected.as_bytes()));
    if !ok {
        tracing::warn!("whatsapp webhook subscribe rejected: verify token mismatch");
        return StatusCode::FORBIDDEN.into_response();
    }
    q.challenge.into_response()
}

pub async fn webhook(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let secret = state.cfg.whatsapp_app.app_secret.expose_secret();
    let provided = headers
        .get(SIGNATURE_HEADER)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if secret.trim().is_empty() || !signature_matches(secret, provided, &body) {
        tracing::warn!("whatsapp webhook rejected: bad signature");
        return StatusCode::FORBIDDEN;
    }
    let notification = match serde_json::from_slice::<Notification>(&body) {
        Ok(n) => n,
        Err(err) => {
            tracing::warn!(?err, "whatsapp webhook: unparseable notification body");
            return StatusCode::OK;
        }
    };
    for action in classify(&notification, &state.cfg.whatsapp_app.phone_number_id) {
        // Off the request: a slow ack makes Meta re-deliver, racing the
        // first attempt's consume.
        let state = state.clone();
        match action {
            InboundAction::Link {
                code,
                from,
                profile_name,
            } => {
                tokio::spawn(async move { handle_link(&state, &code, &from, profile_name).await });
            }
            InboundAction::Stop { from } => {
                tokio::spawn(async move { handle_stop(&state, &from).await });
            }
        }
    }
    StatusCode::OK
}

const OPTED_OUT_NOTE: &str = "the recipient sent stop on WhatsApp";

/// `stop` severs every org's channel bound to the number (Meta opt-out
/// policy) — same cross-org shape as a Telegram kick.
async fn handle_stop(state: &AppState, from: &str) {
    match state
        .notification_channel_store
        .disable_by_external_ref(ChannelKind::WhatsAppApp, from, OPTED_OUT_NOTE)
        .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!(disabled = n, "whatsapp channels severed by stop"),
        Err(err) => tracing::warn!(?err, "whatsapp stop disable failed"),
    }
}

async fn handle_link(state: &AppState, code: &str, from: &str, profile_name: Option<String>) {
    let text = match link_phone(state, code, from, profile_name).await {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(?err, "whatsapp link consume failed");
            "Something went wrong while linking. Please mint a fresh link and try again."
                .to_string()
        }
    };
    spawn_reply(state, from.to_string(), text);
}

async fn link_phone(
    state: &AppState,
    code: &str,
    from: &str,
    profile_name: Option<String>,
) -> Result<String> {
    let hash = sha256_hex(code);
    let Some(link) = state
        .channel_link_code_store
        .consume(LinkPurpose::Whatsapp, &hash)
        .await?
    else {
        return Ok(
            "This link is invalid, expired, or already used. Mint a fresh link from the \
             notification settings and try again."
                .to_string(),
        );
    };
    let org = link.org_id;

    let base_name = link
        .channel_name
        .as_deref()
        .or(profile_name.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("WhatsApp")
        .to_string();
    let limit = i64::from(
        state
            .quotas
            .limit_for_org(org)
            .await?
            .max_notification_channels,
    );
    let config = ChannelConfig::WhatsAppApp(WhatsAppAppConfig {
        phone: from.to_string(),
        profile_name,
    });
    let channel = match create_channel_deduped(
        state.notification_channel_store.as_ref(),
        org,
        &base_name,
        config,
        limit,
        QuotaBlockLog {
            db: state.db.clone(),
            user: None,
            flow: "whatsapp_link",
        },
    )
    .await
    {
        Ok(ch) => ch,
        Err(err) => {
            // 15-minute codes keep their burn-on-failure shape (telegram
            // precedent).
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
        tracing::warn!(?err, channel_id = %channel.id, "whatsapp link attach failed");
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

fn spawn_reply(state: &AppState, to: String, text: String) {
    let http = state.outbound_http.clone();
    let token = state.cfg.whatsapp_app.access_token.clone();
    let phone_number_id = state.cfg.whatsapp_app.phone_number_id.clone();
    tokio::spawn(async move {
        if let Err(err) =
            send_text(&http, token.expose_secret(), &phone_number_id, &to, &text).await
        {
            tracing::warn!(?err, "whatsapp link reply failed");
        }
    });
}
