//! `/hooks/slack/interactions`, our Slack app's Interactivity Request URL.
//! Slack's signature is the only authentication. A signed request is answered
//! 200 at once, since Slack shows the presser an error after 3 seconds, and
//! the presses acted on, Acknowledge and Resolve, are taken off the request through
//! [`super::app_ack`]. Every other click on our messages, such as a link
//! button, is reported here too and gets nothing more.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use secrecy::ExposeSecret;

use crate::app::AppState;
use crate::domain::{ChannelKind, LinkedApp};
use crate::notifier::slack::escape;
use crate::security::app_link::external_id;
use crate::slack::{Press, Reply, alert_press, respond, signed_by_slack};

use super::app_ack::{Pressed, announcement, answer_offering_link};

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
    if let Some(press) = alert_press(&body) {
        tokio::spawn(async move { handle_press(&state, press).await });
    }
    StatusCode::OK
}

async fn handle_press(state: &AppState, press: Press) {
    let pressed = Pressed {
        app: LinkedApp::Slack,
        kind: ChannelKind::SlackApp,
        place: &press.channel_id,
        sender: external_id(&state.app_link_secret, &press.person),
        data: &press.value,
    };
    let (notice, done) = answer_offering_link(state, pressed, press.username.as_deref(), |url| {
        format!("<{url}|Link your Slack account> so your next presses carry your name.")
    })
    .await;
    reply(state, &press, &Reply::to_presser(&notice)).await;
    if let Some(action) = done.filter(|_| !press.in_direct_message()) {
        let text = announcement(action, press.username.as_deref().map(escape));
        reply(
            state,
            &press,
            &Reply::to_channel(&text, press.thread_ts.as_deref()),
        )
        .await;
    }
}

async fn reply(state: &AppState, press: &Press, reply: &Reply<'_>) {
    if let Err(err) = respond(&state.outbound_http, &press.response_url, reply).await {
        tracing::warn!(error = %err, "slack press reply failed");
    }
}
