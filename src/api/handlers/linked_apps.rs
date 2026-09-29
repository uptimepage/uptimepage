//! App accounts on the caller's own account: starting a Telegram link,
//! spending the offer a Pushover or Slack account was sent, and unlinking any
//! of them.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::json::Json;
use crate::app::AppState;
use crate::domain::{LinkedApp, UserId};
use crate::error::{AppError, Result, codes};
use crate::request::{BrowserUser, CurrentUser};
use crate::security::app_link::{TELEGRAM_LINK_TTL, telegram_start_payload};
use crate::security::sha256_hex;
use crate::security::token_hash::generate_raw_token;
use crate::storage::linked_apps::{Claimant, LinkOutcome};

#[derive(Debug, Serialize, ToSchema)]
pub struct TelegramAccountLink {
    /// Opens a chat with the bot; pressing Start there links that Telegram
    /// account. Works once, for an hour.
    pub url: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/me/linked-apps/telegram",
    tag = "account",
    summary = "Start linking a Telegram account to the caller",
    description = "Returns a one-time link to the bot. The Telegram account that \
                   presses Start with it is linked to the caller, so its presses \
                   on an Acknowledge button name them in every organization they \
                   belong to. Asking again voids the previous link. A Telegram \
                   account linked to someone else is refused, not moved.",
    responses(
        (status = 200, body = TelegramAccountLink),
        (status = 404, body = crate::error::ApiError, description = "No Telegram bot on this deployment"),
    ),
)]
pub async fn start_telegram(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
) -> Result<Json<TelegramAccountLink>> {
    if !state.cfg.telegram.enabled() {
        return Err(AppError::not_found(
            codes::TELEGRAM_LINK_NOT_FOUND,
            "telegram linking is not available on this deployment",
        ));
    }
    let code = generate_raw_token();
    state
        .linked_app_store
        .mint_telegram(user_id, &sha256_hex(&code), Utc::now() + TELEGRAM_LINK_TTL)
        .await?;
    Ok(Json(TelegramAccountLink {
        url: crate::telegram::start_link(
            &state.cfg.telegram.bot_username,
            &telegram_start_payload(&code),
        ),
    }))
}

/// The code from the offer an app account received after it acknowledged
/// without a name.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkOfferRequest {
    pub code: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/me/linked-apps/pushover",
    tag = "account",
    summary = "Link the Pushover account a link offer was sent to",
    description = "Only the Pushover account that acknowledged an emergency page \
                   receives the offer, so presenting its code proves the caller \
                   holds that account. Its acknowledgements then name the caller \
                   in every organization they belong to. A code links once; a \
                   Pushover account linked to someone else is refused, not moved.",
    request_body = LinkOfferRequest,
    responses(
        (status = 204, description = "Linked, or it was the caller's already"),
        (status = 400, body = crate::error::ApiError, description = "Unknown, used or expired code"),
        (status = 409, body = crate::error::ApiError, description = "Linked to someone else"),
    ),
)]
pub async fn link_pushover(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    Json(req): Json<LinkOfferRequest>,
) -> Result<StatusCode> {
    claim_offer(&state, user_id, LinkedApp::Pushover, &req.code).await
}

#[utoipa::path(
    post,
    path = "/api/v1/me/linked-apps/slack",
    tag = "account",
    summary = "Link the Slack account a link offer was sent to",
    description = "Only the Slack account that pressed Acknowledge sees the offer, \
                   so presenting its code proves the caller holds that account. \
                   Its presses then name the caller in every organization they \
                   belong to. A code links once, within an hour; a Slack account \
                   linked to someone else is refused, not moved.",
    request_body = LinkOfferRequest,
    responses(
        (status = 204, description = "Linked, or it was the caller's already"),
        (status = 400, body = crate::error::ApiError, description = "Unknown, used or expired code"),
        (status = 409, body = crate::error::ApiError, description = "Linked to someone else"),
    ),
)]
pub async fn link_slack(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    Json(req): Json<LinkOfferRequest>,
) -> Result<StatusCode> {
    claim_offer(&state, user_id, LinkedApp::Slack, &req.code).await
}

async fn claim_offer(
    state: &AppState,
    user_id: UserId,
    app: LinkedApp,
    code: &str,
) -> Result<StatusCode> {
    let outcome = state
        .linked_app_store
        .claim(app, &sha256_hex(code.trim()), Claimant::Person(user_id))
        .await?;
    match outcome {
        LinkOutcome::Linked(_) | LinkOutcome::AlreadyYours(_) => {
            tracing::info!(user_id = %user_id.0, app = app.as_db_str(), "app account linked");
            Ok(StatusCode::NO_CONTENT)
        }
        LinkOutcome::Taken => Err(AppError::conflict(
            codes::APP_ACCOUNT_TAKEN,
            format!(
                "this {} account is linked to another Uptimepage account; unlink it there first",
                app.label()
            ),
        )),
        LinkOutcome::Invalid => Err(AppError::bad_request(
            codes::APP_LINK_INVALID,
            "this link is not valid, was already used, or has expired",
        )),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/me/linked-apps/{id}",
    tag = "account",
    summary = "Unlink one app account from the caller's account",
    description = "Acknowledgements made in that app account stop carrying the \
                   caller's name. Ones already recorded keep it.",
    params(("id" = Uuid, Path, description = "Linked app account id")),
    responses(
        (status = 204, description = "Unlinked"),
        (status = 404, body = crate::error::ApiError, description = "No such app account on this account"),
    ),
)]
pub async fn unlink(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if state.linked_app_store.unlink(user_id, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::LINKED_APP_NOT_FOUND,
            "no such app account on this account",
        ))
    }
}
