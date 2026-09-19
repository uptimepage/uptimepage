//! "Add to Discord" connect flow (`/auth/discord/start` +
//! `/auth/discord/callback`).
//!
//! The callback body (state consume, authority check, delegate spend) lives
//! in `connect_oauth::run_callback`; this module only exchanges the code at
//! Discord and keeps the webhook, stored as a regular `discord` channel —
//! the access token is discarded.

use axum::extract::{Query, State};
use axum::response::Response;

use crate::app::AppState;
use crate::auth::discord;
use crate::domain::{ChannelConfig, DiscordConfig};
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
    mint_start_response(&state, &connect_oauth::DISCORD, q.wants_json(), org, None).await
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
            let webhook = discord::exchange_code(
                &state.outbound_http,
                &state.cfg.discord_oauth,
                &callback_uri(&state, &connect_oauth::DISCORD),
                &code,
            )
            .await?;
            let name = webhook
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or("Discord")
                .to_string();
            Ok((
                ChannelConfig::Discord(DiscordConfig {
                    webhook_url: webhook.url,
                    mention: None,
                }),
                name,
            ))
        }
    };
    run_callback(
        &state,
        &connect_oauth::DISCORD,
        user,
        &client_ip.to_string(),
        q,
        exchange,
    )
    .await
}
