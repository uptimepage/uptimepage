//! `/hooks/discord/interactions`, our Discord app's Interactions Endpoint URL.
//! Discord's signature is the only authentication, and Discord checks now and
//! then that a bad one is refused. A press on the Acknowledge button is
//! answered at once with a private "thinking…", since Discord drops the
//! interaction after 3 seconds, and taken off the request through
//! [`super::app_ack`]; the outcome then replaces the "thinking…". Every other
//! interaction gets a valid answer that does nothing.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;

use crate::app::AppState;
use crate::discord::{
    Press, Received, Response as Answer, announce, answer_privately, read, signed_by_discord,
};
use crate::domain::{ChannelKind, LinkedApp};
use crate::notifier::discord::escape;
use crate::security::app_link::external_id;

use super::app_ack::{Pressed, announcement, answer_offering_link};

const SIGNATURE_HEADER: &str = "x-signature-ed25519";
const TIMESTAMP_HEADER: &str = "x-signature-timestamp";

pub async fn interactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let header = |name| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
    };
    let signed = state
        .cfg
        .discord_interactions
        .public_key
        .as_ref()
        .is_some_and(|key| {
            signed_by_discord(
                key,
                header(TIMESTAMP_HEADER),
                header(SIGNATURE_HEADER),
                &body,
                Utc::now().timestamp(),
            )
        });
    if !signed {
        // Discord itself sends bad signatures now and then, to check that
        // they are refused, so one is routine rather than an attack.
        tracing::debug!("discord interaction rejected: bad signature");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(received) = read(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let answer = match received {
        Received::Ping => Answer::pong(),
        Received::Press(press) => {
            tokio::spawn(async move { handle_press(&state, press).await });
            Answer::deferred_private()
        }
        Received::Other(kind) => Answer::nothing_for(kind),
    };
    Json(answer).into_response()
}

async fn handle_press(state: &AppState, press: Press) {
    let pressed = Pressed {
        app: LinkedApp::Discord,
        kind: ChannelKind::DiscordApp,
        place: &press.webhook_id,
        sender: external_id(&state.app_link_secret, &press.person),
        data: &press.custom_id,
    };
    let (notice, listed) = answer_offering_link(state, pressed, press.username.as_deref(), |url| {
        format!("[Link your Discord account](<{url}>) so your next presses carry your name.")
    })
    .await;
    let http = &state.outbound_http;
    if let Err(err) = answer_privately(http, &press.reply, &notice).await {
        // Posted now, the announcement would take the place of the private
        // answer, where only the presser sees it.
        tracing::warn!(error = %err, "discord press answer failed");
        return;
    }
    if listed {
        let text = announcement(press.display_name.as_deref().map(escape));
        if let Err(err) = announce(http, &press.reply, &text).await {
            tracing::warn!(error = %err, "discord acknowledgement announcement failed");
        }
    }
}
