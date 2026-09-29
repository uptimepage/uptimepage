//! `/hooks/slack/interactions`, our Slack app's Interactivity Request URL.
//! Slack's signature is the only authentication. A signed request is answered
//! 200 at once, since Slack shows the presser an error after 3 seconds, and
//! the one press acted on, Acknowledge, is taken off the request through
//! [`super::app_ack`]. Every other click on our messages, such as a link
//! button, is reported here too and gets nothing more.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use secrecy::ExposeSecret;

use crate::app::AppState;
use crate::domain::{ChannelKind, ExternalId, LinkedApp};
use crate::notifier::slack::escape;
use crate::security::app_link::external_id;
use crate::slack::{Press, Reply, acknowledge_press, respond, signed_by_slack};
use crate::storage::linked_apps::offer_link;

use super::app_ack::{Pressed, Taken, acknowledged_notice, take};

const TIMESTAMP_HEADER: &str = "x-slack-request-timestamp";
const SIGNATURE_HEADER: &str = "x-slack-signature";

pub async fn interactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let secret = state.cfg.slack_interactivity.signing_secret.expose_secret();
    let header = |name| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
    };
    let signed = !secret.trim().is_empty()
        && signed_by_slack(
            secret,
            header(TIMESTAMP_HEADER),
            header(SIGNATURE_HEADER),
            &body,
            Utc::now().timestamp(),
        );
    if !signed {
        tracing::warn!("slack interaction rejected: bad signature");
        return StatusCode::UNAUTHORIZED;
    }
    if let Some(press) = acknowledge_press(&body) {
        tokio::spawn(async move { handle_press(&state, press).await });
    }
    StatusCode::OK
}

async fn handle_press(state: &AppState, press: Press) {
    let sender = external_id(&state.app_link_secret, &press.person);
    let pressed = Pressed {
        app: LinkedApp::Slack,
        kind: ChannelKind::SlackApp,
        place: &press.channel_id,
        sender,
        data: &press.value,
    };
    let (notice, announce) = match take(state, pressed).await {
        Taken::Acknowledged { listed, linked } => {
            let hint = match linked.invites_link() {
                true => link_hint(state, sender, press.username.as_deref()).await,
                false => None,
            };
            (
                acknowledged_notice(listed, linked, hint.as_deref().unwrap_or_default()),
                listed && !press.in_direct_message(),
            )
        }
        Taken::Refused(notice) => (notice.to_string(), false),
    };
    reply(state, &press, &Reply::to_presser(&notice)).await;
    if announce {
        let who = press
            .username
            .as_deref()
            .map_or_else(|| "someone".to_string(), escape);
        let text = format!("Acknowledged by {who}.");
        reply(
            state,
            &press,
            &Reply::to_channel(&text, press.thread_ts.as_deref()),
        )
        .await;
    }
}

/// A one-time link naming the presser on their next presses, once whoever
/// opens it signs in. Only the presser sees the message it rides in.
async fn link_hint(state: &AppState, sender: ExternalId, username: Option<&str>) -> Option<String> {
    let offer = offer_link(
        state.linked_app_store.as_ref(),
        &state.cfg.auth.public_base_url,
        LinkedApp::Slack,
        sender,
        username,
        Utc::now(),
    )
    .await;
    match offer {
        Ok(offer) => offer.map(|o| {
            format!(
                "<{}|Link your Slack account> so your next presses carry your name.",
                o.url
            )
        }),
        Err(err) => {
            tracing::warn!(error = %err, "slack link offer failed");
            None
        }
    }
}

async fn reply(state: &AppState, press: &Press, reply: &Reply<'_>) {
    if let Err(err) = respond(&state.outbound_http, &press.response_url, reply).await {
        tracing::warn!(error = %err, "slack press reply failed");
    }
}
