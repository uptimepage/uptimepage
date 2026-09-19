//! "Add to Slack" connect flow (`/auth/slack/start` + `/auth/slack/callback`).
//!
//! The callback body (state consume, authority check, delegate spend) lives
//! in `connect_oauth::run_callback`; this module only exchanges the code at
//! Slack and keeps the incoming webhook, stored as a regular `slack` channel
//! — the access token is discarded.

use axum::extract::{Query, State};
use axum::response::Response;

use crate::app::AppState;
use crate::auth::slack;
use crate::domain::{ChannelConfig, SlackConfig};
use crate::error::{AppError, Result};
use crate::request::client_ip::ClientIp;
use crate::request::{Authorized, ChannelsWrite, CurrentUser};
use crate::web::views::connect_oauth::{
    self, CallbackQuery, StartQuery, callback_uri, mint_start_response, run_callback,
};

pub async fn start(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    Query(q): Query<StartQuery>,
) -> Result<Response> {
    mint_start_response(&state, &connect_oauth::SLACK, q.wants_json(), org, None).await
}

pub async fn callback(
    State(state): State<AppState>,
    user: Result<CurrentUser, AppError>,
    ClientIp(client_ip): ClientIp,
    Query(q): Query<CallbackQuery>,
) -> Result<Response> {
    let exchange = {
        let state = state.clone();
        async move |code: String| {
            let webhook = slack::exchange_code(
                &state.outbound_http,
                &state.cfg.slack_oauth,
                &callback_uri(&state, &connect_oauth::SLACK),
                &code,
            )
            .await?;
            let name = webhook.channel.trim();
            let name = if name.is_empty() { "Slack" } else { name }.to_string();
            Ok((
                ChannelConfig::Slack(SlackConfig {
                    webhook_url: webhook.url,
                    mention: None,
                }),
                name,
            ))
        }
    };
    run_callback(
        &state,
        &connect_oauth::SLACK,
        user,
        &client_ip.to_string(),
        q,
        exchange,
    )
    .await
}
