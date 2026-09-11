//! Operator endpoints for notification-channel CRUD + a send-test action.
//!
//! Standard `ApiError` envelope. Mounted under `/api/v1/notification-channels`.
//! Every handler resolves the caller's tenant via the scope-gated
//! `Authorized<…>` extractor (which wraps `CurrentOrg`) and threads the org
//! into the store, so a channel is only ever visible to its owning org.
//! Secrets are sealed at rest by the store and are never echoed back: every
//! read path returns through [`Redacted`].

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::AppendHeaders;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::api::redaction::Redacted;
use crate::app::AppState;
use crate::auth::sha256_hex;
use crate::auth::token_hash::generate_raw_token;
use crate::auth::url::token_link;
use crate::domain::{
    ChannelConfig, IncidentOrigin, IncidentSeverity, IncidentUrgency, NewNotificationChannel,
    NotificationChannel, NotificationChannelUpdate, NotificationReason, validate_channel_name,
};
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::error::{AppError, Result};
use crate::notifier::build_notifier;
use crate::notifier::event::IncidentNotice;
use crate::storage::channel_verification;
use crate::storage::{LinkCodeStatus, LinkPurpose, MintOutcome};
use crate::web::{
    Authorized, ChannelsDelete, ChannelsExecute, ChannelsRead, ChannelsWrite, CurrentUser,
    RequestSource,
};

/// Result of `POST /{id}/test`. A `false` never reaches the client — a failed
/// delivery is a 422 (`CHANNEL_TEST_FAILED`) — but the explicit field keeps
/// the success body self-describing for API consumers.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TestNotificationResponse {
    pub delivered: bool,
}

/// Only where a plan is something the customer buys: a self-hosted operator
/// owns the `plans` row and their own SMS credentials. `already_sms` keeps an
/// existing channel editable, so a number can be corrected and a leaked token
/// rotated on a plan that would no longer grant a new one.
fn gate_sms(
    state: &AppState,
    config: &ChannelConfig,
    plan: &crate::domain::Plan,
    already_sms: bool,
) -> Result<()> {
    if state.cfg.marketing.enabled
        && !already_sms
        && matches!(config, ChannelConfig::Sms(_))
        && !plan.sms_alerts_enabled
    {
        return Err(AppError::forbidden_code(
            codes::SMS_ALERTS_DISABLED,
            "text-message alerts are not available on your plan",
        ));
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/api/v1/notification-channels",
    tag = "notification-channels",
    summary = "Create a notification channel",
    request_body(content = NewNotificationChannel, example = json!({
        "name": "Ops Slack",
        "config": {
            "type": "slack",
            "webhook_url": "https://hooks.slack.com/services/T000/B000/XXXX"
        },
        "enabled": true
    })),
    responses(
        (status = 201, body = NotificationChannel,
            headers(("Location" = String, description = "URL of the new channel"))),
        (status = 400, body = ApiError,
            description = "Invalid name, invalid transport config, or the redaction \
                           sentinel was re-submitted in place of a real secret"),
        (status = 422, body = ApiError,
            description = "Channel name already in use, or the plan's channel limit \
                           is reached"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Json(mut new): Json<NewNotificationChannel>,
) -> Result<(
    StatusCode,
    AppendHeaders<[(axum::http::HeaderName, HeaderValue); 1]>,
    Redacted<NotificationChannel>,
)> {
    validate_name(&new.name)?;
    new.auto_bind_tags = normalize_rule_tags(&new.auto_bind_tags)?;
    reject_managed_kind(&new.config)?;
    // The console trims in the browser, so without this the same paste that
    // works in the app is refused over the API.
    new.config.normalize();
    validate_config(&new.config)?;
    check_channel_abuse(&state, org, &new.config, false).await?;
    // Friendly pre-check; the store INSERT enforces the same cap atomically
    // under a per-org advisory lock.
    state
        .quotas
        .check_can_create_notification_channel(org, None)
        .await?;
    let plan = state.quotas.limit_for_org(org).await?;
    gate_sms(&state, &new.config, &plan, false)?;
    let limit = i64::from(plan.max_notification_channels);
    let ch = state
        .notification_channel_store
        .create(org, new, source, limit, Some(user))
        .await?;
    let ch = fast_verify_member_email(&state, org, ch).await?;
    spawn_send_verification(&state, org, &ch);
    let location = HeaderValue::from_str(&format!("/api/v1/notification-channels/{}", ch.id))
        .expect("uuid produces ascii-only path");
    Ok((
        StatusCode::CREATED,
        AppendHeaders([(header::LOCATION, location)]),
        Redacted::new(ch),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/notification-channels",
    tag = "notification-channels",
    summary = "List notification channels",
    responses(
        (status = 200, body = [NotificationChannel]),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsRead>,
) -> Result<Redacted<Vec<NotificationChannel>>> {
    Ok(Redacted::new(
        state.notification_channel_store.list(org).await?,
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/notification-channels/{id}",
    tag = "notification-channels",
    summary = "Get a notification channel",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = NotificationChannel),
        (status = 404, body = ApiError),
    ),
)]
pub async fn get(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsRead>,
    Path(id): Path<Uuid>,
) -> Result<Redacted<NotificationChannel>> {
    state
        .notification_channel_store
        .get(org, id)
        .await?
        .map(Redacted::new)
        .ok_or_else(channel_not_found)
}

#[utoipa::path(
    patch,
    path = "/api/v1/notification-channels/{id}",
    tag = "notification-channels",
    summary = "Edit a notification channel",
    description = "Omit fields you don't want to change. A `config` that still \
                   carries the `***` sentinel returns 400 — omit `config` to \
                   keep the stored secret unchanged. A `config` identical to \
                   the stored one keeps the verification state.",
    params(("id" = Uuid, Path)),
    request_body(content = NotificationChannelUpdate),
    responses(
        (status = 200, body = NotificationChannel),
        (status = 400, body = ApiError),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "Channel name already in use"),
    ),
)]
pub async fn update(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    CurrentUser(user): CurrentUser,
    RequestSource(source): RequestSource,
    Path(id): Path<Uuid>,
    Json(mut update): Json<NotificationChannelUpdate>,
) -> Result<Redacted<NotificationChannel>> {
    if let Some(name) = &update.name {
        validate_name(name)?;
    }
    if let Some(tags) = &update.auto_bind_tags {
        update.auto_bind_tags = Some(normalize_rule_tags(tags)?);
    }
    let config_replaced = update.config.is_some();
    if let Some(cfg) = &mut update.config {
        reject_managed_kind(cfg)?;
        cfg.normalize();
        validate_config(cfg)?;
        let stored = state.notification_channel_store.get(org, id).await?;
        let plan = state.quotas.limit_for_org(org).await?;
        // Gated only for a channel that is there: an unknown id is a 404, and
        // answering 403 would tell a caller the plan before the lookup.
        if let Some(held) = &stored {
            let already_sms = matches!(held.config, ChannelConfig::Sms(_));
            gate_sms(&state, cfg, &plan, already_sms)?;
        }
        // A different address is a new destination and gets the full gate.
        let established = match &*cfg {
            ChannelConfig::Email(new) => stored.is_some_and(|c| match &c.config {
                ChannelConfig::Email(old) => {
                    !c.awaiting_verification() && old.to.eq_ignore_ascii_case(&new.to)
                }
                _ => false,
            }),
            _ => false,
        };
        check_channel_abuse(&state, org, cfg, established).await?;
    }
    let updated = state
        .notification_channel_store
        .update(org, id, update, source, Some(user))
        .await?
        .ok_or_else(channel_not_found)?;
    // A replaced config resets the verification gate; re-prove the address,
    // short-circuiting for an org member's own confirmed email as create does.
    if config_replaced {
        let updated = fast_verify_member_email(&state, org, updated).await?;
        spawn_send_verification(&state, org, &updated);
        return Ok(Redacted::new(updated));
    }
    Ok(Redacted::new(updated))
}

#[utoipa::path(
    delete,
    path = "/api/v1/notification-channels/{id}",
    tag = "notification-channels",
    summary = "Delete a notification channel",
    description = "The channel's alert bindings are removed from every \
                   monitor; re-adding the channel later starts unbound.",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, body = ApiError),
    ),
)]
pub async fn delete(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsDelete>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    // The bot-leave decision below needs the transport + chat id, gone once
    // the row is.
    let channel = state.notification_channel_store.get(org, id).await?;
    // Scrub before delete: a stale {channel_id} left in targets.alerts
    // fails channel-existence validation on the next whole-array alerts
    // update of that monitor. This order keeps the operation retryable —
    // a scrub failure leaves the channel in place, so the client's DELETE
    // retry re-runs both steps. A PATCH racing the delete can still
    // re-introduce a binding; the escalation engine tolerates missing
    // channels at resolve time.
    state.target_store.unbind_channel(org, id).await?;
    if state
        .notification_channel_store
        .delete(org, id, Some(user))
        .await?
    {
        if let Some(ch) = channel {
            maybe_leave_telegram_group(&state, &ch);
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(channel_not_found())
    }
}

/// Best-effort: deleting the last channel linked to a Telegram group walks
/// the bot out; it stays while another org's channel still points there.
/// A re-link racing the count can lose the bot — the chat just re-invites it.
fn maybe_leave_telegram_group(state: &AppState, ch: &NotificationChannel) {
    let ChannelConfig::TelegramApp(cfg) = &ch.config else {
        return;
    };
    let Ok(chat_id) = cfg.chat_id.parse::<i64>() else {
        return;
    };
    let Some(token) = state.cfg.telegram.delivery_token() else {
        return;
    };
    if chat_id >= 0 {
        return;
    }
    let client = crate::telegram::TelegramClient::new(state.outbound_http.clone(), token);
    let store = state.notification_channel_store.clone();
    let external_ref = cfg.chat_id.clone();
    tokio::spawn(async move {
        match store
            .count_by_external_ref(crate::domain::ChannelKind::TelegramApp, &external_ref)
            .await
        {
            Ok(0) => {
                if let Err(err) = client.leave_chat(chat_id).await {
                    tracing::warn!(?err, chat_id, "telegram leaveChat failed");
                }
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(?err, chat_id, "telegram leave check failed"),
        }
    });
}

#[utoipa::path(
    post,
    path = "/api/v1/notification-channels/{id}/test",
    tag = "notification-channels",
    summary = "Send a synthetic test alert to a notification channel",
    description = "Delivers one clearly-labelled test alert through the \
                   channel's transport so the operator can confirm the \
                   webhook/token works before binding targets to it. Works on \
                   a disabled channel too.",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = TestNotificationResponse),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError, description = "The channel's transport rejected the test delivery"),
    ),
)]
pub async fn test_send(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsExecute>,
    Path(id): Path<Uuid>,
) -> Result<Json<TestNotificationResponse>> {
    let channel = state
        .notification_channel_store
        .get(org, id)
        .await?
        .ok_or_else(channel_not_found)?;
    // A stored config can predate a deny-list entry — gate the test too.
    let established = !channel.awaiting_verification();
    check_channel_abuse(&state, org, &channel.config, established).await?;
    if channel.awaiting_verification() {
        return Err(AppError::unprocessable(
            codes::CHANNEL_UNVERIFIED,
            "email address not verified — confirm the verification link first",
        ));
    }
    deliver_test(&state, &channel.config).await?;
    // A test that lands proves the endpoint is back, so it clears the run. Not
    // for a disabled channel, which delivers nothing whatever the test proves,
    // and never at the cost of failing a test that already went out.
    if channel.enabled
        && let Err(err) = state
            .notification_channel_store
            .record_delivery_outcome(org, id, true)
            .await
    {
        tracing::warn!(channel_id = %id, error = %err, "channel delivery run not cleared");
    }
    Ok(Json(TestNotificationResponse { delivered: true }))
}

/// Body of `POST /test`: a full transport config to exercise without saving.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TestChannelConfigRequest {
    pub config: ChannelConfig,
}

#[utoipa::path(
    post,
    path = "/api/v1/notification-channels/test",
    tag = "notification-channels",
    summary = "Send a test alert through an unsaved transport config",
    description = "Delivers one clearly-labelled test alert through the config \
                   in the request body without persisting anything, so the \
                   operator can verify a webhook/token before creating or \
                   saving a channel. The config is validated exactly as on \
                   create.",
    request_body(content = TestChannelConfigRequest, example = json!({
        "config": {
            "type": "slack",
            "webhook_url": "https://hooks.slack.com/services/T000/B000/XXXX"
        }
    })),
    responses(
        (status = 200, body = TestNotificationResponse),
        (status = 400, body = ApiError, description = "Invalid transport config or a redaction sentinel in place of a real secret"),
        (status = 422, body = ApiError, description = "The transport rejected the test delivery"),
    ),
)]
pub async fn test_config(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsExecute>,
    Json(mut req): Json<TestChannelConfigRequest>,
) -> Result<Json<TestNotificationResponse>> {
    // Same spam vector as create: the test would message a caller-supplied
    // chat id with the operator bot.
    reject_managed_kind(&req.config)?;
    // Same cleaning a save would do, or a paste that saves fine fails its test.
    req.config.normalize();
    validate_config(&req.config)?;
    // This body is a channel the caller has not saved yet, so it gets the gate
    // create would apply. A channel already stored tests through `/{id}/test`.
    let plan = state.quotas.limit_for_org(org).await?;
    gate_sms(&state, &req.config, &plan, false)?;
    check_channel_abuse(&state, org, &req.config, false).await?;
    // An unsaved email config can never have proven its inbox — testing it
    // would mail an arbitrary caller-supplied address.
    if matches!(req.config, ChannelConfig::Email(_)) {
        return Err(AppError::unprocessable(
            codes::CHANNEL_UNVERIFIED,
            "email channels must be saved and verified before a test send",
        ));
    }
    deliver_test(&state, &req.config).await?;
    Ok(Json(TestNotificationResponse { delivered: true }))
}

// ── Email verification ───────────────────────────────────────────────────

/// Sender + From identity for alert email; built per call from app state.
fn email_delivery(state: &AppState) -> crate::notifier::EmailDelivery {
    crate::notifier::EmailDelivery {
        sender: state.email_sender.clone(),
        from_address: state.cfg.email.from_address.clone(),
        from_name: state.cfg.email.from_name.clone(),
    }
}

fn stop_link(state: &AppState, channel_id: Uuid) -> Option<String> {
    crate::storage::notification_channels::channel_stop_url(
        &state.cfg.auth.public_base_url,
        &state.alert_channel_stop_secret,
        channel_id,
    )
}

/// One verification mail, composed identically for the create/update hook
/// and the resend endpoint: mint a token (all daily caps enforced in the
/// mint) and send the link off the response path.
async fn mint_and_send_verification(
    state: &AppState,
    org: crate::domain::OrgId,
    channel_id: Uuid,
    channel_name: String,
    to: String,
) -> Result<channel_verification::MintOutcome> {
    let pool = state.require_db()?;
    let outcome = channel_verification::mint(pool, org, channel_id, &to).await?;
    if let channel_verification::MintOutcome::Created { token } = &outcome {
        let delivery = email_delivery(state);
        let org_name = crate::storage::orgs::get_org(pool, org)
            .await
            .ok()
            .flatten()
            .map(|o| o.name);
        let decline_url = stop_link(state, channel_id);
        let outgoing = TransactionalEmail {
            from: EmailAddress::new(delivery.from_address, delivery.from_name),
            to: EmailAddress::new(to.clone(), to),
            template: EmailTemplate::ChannelVerification {
                channel_name,
                verify_url: token_link(&state.cfg.auth.public_base_url, "/verify-channel", token),
                expires_hours: channel_verification::VERIFICATION_TTL_HOURS,
                org_name,
                decline_url,
            },
        };
        let sender = delivery.sender;
        tokio::spawn(async move {
            if let Err(err) = sender.send(outgoing).await {
                tracing::warn!(%channel_id, error = %err, "channel verification mail failed");
            }
        });
    }
    Ok(outcome)
}

/// Skip the verify round-trip when the destination is the confirmed login email
/// of a member of this org: joining the org already proved control of that
/// mailbox, so no separate opt-in click is needed. A third-party address still
/// falls through to the mailed confirmation. Best-effort — a lookup failure
/// leaves the channel unverified.
async fn fast_verify_member_email(
    state: &AppState,
    org: crate::domain::OrgId,
    ch: NotificationChannel,
) -> Result<NotificationChannel> {
    let ChannelConfig::Email(cfg) = &ch.config else {
        return Ok(ch);
    };
    if !ch.awaiting_verification() {
        return Ok(ch);
    }
    let Some(pool) = state.db.as_ref() else {
        return Ok(ch);
    };
    let (is_member,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (
            SELECT 1 FROM memberships m
            JOIN users u ON u.id = m.user_id
            WHERE m.org_id = $1
              AND u.deleted_at IS NULL
              AND u.email_verified_at IS NOT NULL
              AND u.email = $2
        )",
    )
    .bind(org.0)
    .bind(&cfg.to)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("fast-verify lookup: {e}")))?;
    if !is_member {
        return Ok(ch);
    }
    let mut ch = ch;
    if state
        .notification_channel_store
        .set_verified(org, ch.id, ch.updated_at)
        .await?
    {
        ch.verified_at = Some(Utc::now());
    }
    Ok(ch)
}

/// Best-effort hook on create / config replace. Cap breaches and failures
/// only log — the channel stays unverified and the operator has the resend
/// action.
pub(crate) fn spawn_send_verification(
    state: &AppState,
    org: crate::domain::OrgId,
    ch: &NotificationChannel,
) {
    if !ch.awaiting_verification() {
        return;
    }
    let ChannelConfig::Email(cfg) = &ch.config else {
        return;
    };
    if state.db.is_none() {
        return;
    }
    let state = state.clone();
    let (channel_id, channel_name, to) = (ch.id, ch.name.clone(), cfg.to.clone());
    tokio::spawn(async move {
        match mint_and_send_verification(&state, org, channel_id, channel_name, to).await {
            Ok(channel_verification::MintOutcome::Created { .. }) => {}
            Ok(channel_verification::MintOutcome::LimitReached) => {
                tracing::warn!(%channel_id, "channel verification mint rate-limited");
            }
            Err(err) => {
                tracing::warn!(%channel_id, error = %err, "channel verification mint failed");
            }
        }
    });
}

#[utoipa::path(
    post,
    path = "/api/v1/notification-channels/{id}/resend-verification",
    tag = "notification-channels",
    summary = "Resend the verification mail for an email channel",
    params(("id" = Uuid, Path)),
    responses(
        (status = 202, description = "Verification mail queued"),
        (status = 404, body = ApiError),
        (status = 422, body = ApiError,
            description = "Not an unverified email channel, or the daily \
                           verification-mail cap is reached"),
    ),
)]
pub async fn resend_verification(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    let channel = state
        .notification_channel_store
        .get(org, id)
        .await?
        .ok_or_else(channel_not_found)?;
    let ChannelConfig::Email(cfg) = &channel.config else {
        return Err(AppError::unprocessable(
            codes::CHANNEL_NOT_VERIFIABLE,
            "only email channels need verification",
        ));
    };
    if !channel.awaiting_verification() {
        return Err(AppError::unprocessable(
            codes::CHANNEL_ALREADY_VERIFIED,
            "this address is already verified",
        ));
    }
    // Mint synchronously so a cap breach surfaces to the caller; the send
    // itself stays off the response path.
    match mint_and_send_verification(
        &state,
        org,
        channel.id,
        channel.name.clone(),
        cfg.to.clone(),
    )
    .await?
    {
        channel_verification::MintOutcome::LimitReached => Err(AppError::unprocessable(
            codes::CHANNEL_VERIFICATION_LIMIT,
            "verification mail limit reached — try again tomorrow",
        )),
        channel_verification::MintOutcome::Created { .. } => Ok(StatusCode::ACCEPTED),
    }
}

// ── Central-bot Telegram linking ─────────────────────────────────────────

const ONE_TAP_LINK_TTL_MINUTES: i64 = 15;
/// Outstanding codes per org — bounds drive-by minting.
const ONE_TAP_LINK_MAX_OUTSTANDING: i64 = 5;

/// Shared by the telegram-link and whatsapp-link mints.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct OneTapLinkRequest {
    /// Optional name for the channel created when the code is consumed;
    /// defaults to the linked chat title / WhatsApp profile name.
    #[serde(default)]
    #[schema(example = "Ops Telegram", max_length = 100)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TelegramLinkResponse {
    /// Poll handle for `GET /telegram-link/{id}`.
    pub id: Uuid,
    /// The raw single-use code — shown once, never stored.
    pub code: String,
    /// `https://t.me/<bot>?start=<code>` — links a private chat.
    pub deep_link: String,
    /// `https://t.me/<bot>?startgroup=<code>` — adds the bot to a group the
    /// user picks and links it. Same code; whichever chat consumes it wins.
    pub group_deep_link: String,
    pub expires_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OneTapLinkStatusResponse {
    /// `pending`, `consumed`, or `expired`.
    pub status: &'static str,
    /// The created channel, present once `status` is `consumed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<Uuid>,
}

#[utoipa::path(
    post,
    path = "/api/v1/notification-channels/telegram-link",
    tag = "notification-channels",
    summary = "Mint a single-use Telegram link code",
    description = "Returns a short-lived code wrapped in a t.me deep link. \
                   Sending it to the bot (tap Start) creates a `telegram_app` \
                   channel for the chat. 404 on deployments without a central \
                   bot configured.",
    request_body(content = OneTapLinkRequest, example = json!({ "name": "Ops Telegram" })),
    responses(
        (status = 201, body = TelegramLinkResponse),
        (status = 400, body = ApiError, description = "Invalid channel-name hint"),
        (status = 422, body = ApiError, description = "Too many outstanding link codes"),
    ),
)]
pub async fn telegram_link_mint(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<OneTapLinkRequest>,
) -> Result<(StatusCode, Json<TelegramLinkResponse>)> {
    require_central_bot(&state)?;
    let name = match req.name.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(n) => {
            validate_name(n)?;
            Some(n.to_string())
        }
    };
    let code = generate_raw_token();
    let expires_at = Utc::now() + chrono::Duration::minutes(ONE_TAP_LINK_TTL_MINUTES);
    let outcome = state
        .channel_link_code_store
        .mint(
            org,
            LinkPurpose::Telegram,
            Some(user),
            &sha256_hex(&code),
            name.as_deref(),
            None,
            expires_at,
            ONE_TAP_LINK_MAX_OUTSTANDING,
        )
        .await?;
    let minted = match outcome {
        MintOutcome::Created(c) => c,
        MintOutcome::LimitReached => {
            return Err(AppError::unprocessable(
                codes::TELEGRAM_LINK_LIMIT,
                "too many outstanding link codes; wait for one to expire or complete a pending link",
            ));
        }
    };
    let bot = state.cfg.telegram.bot_username.trim_start_matches('@');
    Ok((
        StatusCode::CREATED,
        Json(TelegramLinkResponse {
            id: minted.id,
            deep_link: format!("https://t.me/{bot}?start={code}"),
            group_deep_link: format!("https://t.me/{bot}?startgroup={code}"),
            code,
            expires_at: minted.expires_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/notification-channels/telegram-link/{id}",
    tag = "notification-channels",
    summary = "Poll a Telegram link code",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = OneTapLinkStatusResponse),
        (status = 404, body = ApiError),
    ),
)]
pub async fn telegram_link_status(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<OneTapLinkStatusResponse>> {
    require_central_bot(&state)?;
    let status = state
        .channel_link_code_store
        .status(org, id)
        .await?
        .ok_or_else(|| {
            AppError::not_found(codes::TELEGRAM_LINK_NOT_FOUND, "link code not found")
        })?;
    Ok(Json(match status {
        LinkCodeStatus::Pending => OneTapLinkStatusResponse {
            status: "pending",
            channel_id: None,
        },
        LinkCodeStatus::Consumed { channel_id } => OneTapLinkStatusResponse {
            status: "consumed",
            channel_id: Some(channel_id),
        },
        LinkCodeStatus::Expired => OneTapLinkStatusResponse {
            status: "expired",
            channel_id: None,
        },
    }))
}

// ── WhatsApp one-tap linking ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WhatsAppLinkResponse {
    /// Poll handle for `GET /whatsapp-link/{id}`.
    pub id: Uuid,
    /// The raw single-use code — shown once, never stored.
    pub code: String,
    /// `https://wa.me/<number>?text=<code>` — opens WhatsApp with the code
    /// prefilled; sending it links the sender's number.
    pub deep_link: String,
    pub expires_at: chrono::DateTime<Utc>,
}

#[utoipa::path(
    post,
    path = "/api/v1/notification-channels/whatsapp-link",
    tag = "notification-channels",
    summary = "Mint a single-use WhatsApp link code",
    description = "Returns a short-lived code wrapped in a wa.me deep link. \
                   Sending the prefilled message creates a `whatsapp_app` \
                   channel for the sender's number. 404 on deployments \
                   without the operator WhatsApp number enabled.",
    request_body(content = OneTapLinkRequest, example = json!({ "name": "Ops WhatsApp" })),
    responses(
        (status = 201, body = WhatsAppLinkResponse),
        (status = 400, body = ApiError, description = "Invalid channel-name hint"),
        (status = 422, body = ApiError, description = "Too many outstanding link codes"),
    ),
)]
pub async fn whatsapp_link_mint(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<OneTapLinkRequest>,
) -> Result<(StatusCode, Json<WhatsAppLinkResponse>)> {
    require_central_whatsapp(&state)?;
    let name = match req.name.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(n) => {
            validate_name(n)?;
            Some(n.to_string())
        }
    };
    let code = generate_raw_token();
    let expires_at = Utc::now() + chrono::Duration::minutes(ONE_TAP_LINK_TTL_MINUTES);
    let outcome = state
        .channel_link_code_store
        .mint(
            org,
            LinkPurpose::Whatsapp,
            Some(user),
            &sha256_hex(&code),
            name.as_deref(),
            None,
            expires_at,
            ONE_TAP_LINK_MAX_OUTSTANDING,
        )
        .await?;
    let minted = match outcome {
        MintOutcome::Created(c) => c,
        MintOutcome::LimitReached => {
            return Err(AppError::unprocessable(
                codes::WHATSAPP_LINK_LIMIT,
                "too many outstanding link codes; wait for one to expire or complete a pending link",
            ));
        }
    };
    let number = state.cfg.whatsapp_app.public_number.trim();
    Ok((
        StatusCode::CREATED,
        Json(WhatsAppLinkResponse {
            id: minted.id,
            deep_link: format!("https://wa.me/{number}?text={code}"),
            code,
            expires_at: minted.expires_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/notification-channels/whatsapp-link/{id}",
    tag = "notification-channels",
    summary = "Poll a WhatsApp link code",
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = OneTapLinkStatusResponse),
        (status = 404, body = ApiError),
    ),
)]
pub async fn whatsapp_link_status(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<OneTapLinkStatusResponse>> {
    require_central_whatsapp(&state)?;
    let status = state
        .channel_link_code_store
        .status(org, id)
        .await?
        .ok_or_else(|| {
            AppError::not_found(codes::WHATSAPP_LINK_NOT_FOUND, "link code not found")
        })?;
    Ok(Json(match status {
        LinkCodeStatus::Pending => OneTapLinkStatusResponse {
            status: "pending",
            channel_id: None,
        },
        LinkCodeStatus::Consumed { channel_id } => OneTapLinkStatusResponse {
            status: "consumed",
            channel_id: Some(channel_id),
        },
        LinkCodeStatus::Expired => OneTapLinkStatusResponse {
            status: "expired",
            channel_id: None,
        },
    }))
}

// ── Delegation links (/c/<code>) ─────────────────────────────────────────

const DELEGATE_LINK_TTL_DAYS: i64 = 7;
/// Outstanding delegate links per org — bounds drive-by minting.
const DELEGATE_LINK_MAX_OUTSTANDING: i64 = 5;

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct DelegateLinkRequest {
    /// Optional name for the channel the invitee creates.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional pinned channel kind; the connect page then offers only
    /// that transport.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DelegateLinkResponse {
    pub id: Uuid,
    /// Shareable connect-page URL.
    pub url: String,
    pub expires_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DelegateLinkRow {
    pub id: Uuid,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    pub expires_at: chrono::DateTime<Utc>,
    /// `pending` | `consumed` | `expired` (revoked reads as expired).
    pub status: &'static str,
    pub channel_id: Option<Uuid>,
}

pub(crate) fn delegate_status_parts(status: LinkCodeStatus) -> (&'static str, Option<Uuid>) {
    match status {
        LinkCodeStatus::Pending => ("pending", None),
        LinkCodeStatus::Consumed { channel_id } => ("consumed", Some(channel_id)),
        LinkCodeStatus::Expired => ("expired", None),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/notification-channels/delegate",
    tag = "notification-channels",
    summary = "Mint a delegation link for connecting one channel",
    description = "Returns a single-use `/c/<code>` URL to hand to the person \
                   who owns the Slack workspace / Telegram group / inbox. The \
                   link can create exactly one channel in this org and nothing \
                   else; it expires after seven days and is revocable.",
    request_body(content = DelegateLinkRequest, example = json!({ "name": "Ops Slack", "kind": "slack" })),
    responses(
        (status = 201, body = DelegateLinkResponse),
        (status = 400, body = ApiError, description = "Invalid name or unknown kind"),
        (status = 422, body = ApiError, description = "Too many outstanding delegation links"),
    ),
)]
pub async fn delegate_link_mint(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<DelegateLinkRequest>,
) -> Result<(StatusCode, Json<DelegateLinkResponse>)> {
    let name = match req.name.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(n) => {
            validate_name(n)?;
            Some(n.to_string())
        }
    };
    let kind = match req.kind.as_deref().map(str::trim) {
        Some("") | None => None,
        Some(k) => {
            if !crate::domain::ChannelKind::ALL
                .iter()
                .any(|c| c.as_db_str() == k)
            {
                return Err(AppError::bad_request_field(
                    codes::DELEGATE_KIND_INVALID,
                    "unknown channel kind",
                    "kind",
                ));
            }
            Some(k.to_string())
        }
    };
    let code = generate_raw_token();
    let expires_at = Utc::now() + chrono::Duration::days(DELEGATE_LINK_TTL_DAYS);
    let outcome = state
        .channel_link_code_store
        .mint(
            org,
            LinkPurpose::Delegate,
            Some(user),
            &sha256_hex(&code),
            name.as_deref(),
            kind.as_deref(),
            expires_at,
            DELEGATE_LINK_MAX_OUTSTANDING,
        )
        .await?;
    let minted = match outcome {
        MintOutcome::Created(c) => c,
        MintOutcome::LimitReached => {
            return Err(AppError::unprocessable(
                codes::DELEGATE_LINK_LIMIT,
                "too many outstanding delegation links; revoke one or wait for one to expire",
            ));
        }
    };
    Ok((
        StatusCode::CREATED,
        Json(DelegateLinkResponse {
            id: minted.id,
            url: format!(
                "{}/c/{code}",
                state.cfg.auth.public_base_url.trim_end_matches('/')
            ),
            expires_at: minted.expires_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/notification-channels/delegate",
    tag = "notification-channels",
    summary = "List delegation links",
    responses((status = 200, body = [DelegateLinkRow])),
)]
pub async fn delegate_link_list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsRead>,
) -> Result<Json<Vec<DelegateLinkRow>>> {
    let rows = state.channel_link_code_store.list_delegates(org).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let (status, channel_id) = delegate_status_parts(r.status);
                DelegateLinkRow {
                    id: r.id,
                    name: r.channel_name,
                    kind: r.kind_hint,
                    created_at: r.created_at,
                    expires_at: r.expires_at,
                    status,
                    channel_id,
                }
            })
            .collect(),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/v1/notification-channels/delegate/{id}",
    tag = "notification-channels",
    summary = "Revoke a delegation link",
    params(("id" = Uuid, Path)),
    responses(
        (status = 204, description = "Revoked"),
        (status = 404, body = ApiError, description = "Unknown, already used, or already revoked"),
    ),
)]
pub async fn delegate_link_revoke(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<ChannelsWrite>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if state.channel_link_code_store.revoke(org, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::DELEGATE_LINK_NOT_FOUND,
            "delegation link not found",
        ))
    }
}

/// One synthetic, clearly-labelled delivery through `config`'s transport.
/// Shared by the saved-channel and ad-hoc test endpoints so both exercise
/// the exact notifier path real incidents use.
async fn deliver_test(state: &AppState, config: &ChannelConfig) -> Result<()> {
    let central =
        state
            .cfg
            .telegram
            .delivery_token()
            .map(|bot_token| crate::notifier::CentralTelegram {
                bot_token,
                budget: &state.telegram_send_budget,
            });
    let email = email_delivery(state);
    let whatsapp = state
        .cfg
        .whatsapp_app
        .enabled()
        .then_some(&state.cfg.whatsapp_app);
    // Targeted pings ride along so a wrong id shows; broadcasts do not, so
    // trying a config out never wakes the channel.
    let quieted = config.quieted_for_test();
    let notifier = build_notifier(
        &quieted,
        &state.outbound_http,
        central,
        whatsapp,
        Some(&email),
        None,
        None,
    )?;
    let notice = IncidentNotice {
        incident_id: Uuid::nil(),
        reason: NotificationReason::Opened,
        monitor_name: Some("uptimepage test notification".to_string()),
        title: None,
        severity: IncidentSeverity::Minor,
        urgency: IncidentUrgency::Low,
        origin: IncidentOrigin::Monitor,
        started_at: Utc::now(),
        ended_at: None,
        error_sample: Some(
            "This is a test notification confirming the channel is configured correctly."
                .to_string(),
        ),
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        url: None,
        note: None,
    };
    notifier.notify_incident(&notice).await.map_err(|e| {
        AppError::unprocessable(
            codes::CHANNEL_TEST_FAILED,
            format!("test delivery failed: {e}"),
        )
    })
}

// ── Validation ──────────────────────────────────────────────────────────

fn channel_not_found() -> AppError {
    AppError::not_found(codes::CHANNEL_NOT_FOUND, "notification channel not found")
}

/// Without a central bot the linking surface answers as absent.
fn require_central_bot(state: &AppState) -> Result<()> {
    if state.cfg.telegram.enabled() {
        Ok(())
    } else {
        Err(AppError::not_found(
            codes::TELEGRAM_LINK_NOT_FOUND,
            "telegram linking is not available on this deployment",
        ))
    }
}

fn require_central_whatsapp(state: &AppState) -> Result<()> {
    if state.cfg.whatsapp_app.enabled() {
        Ok(())
    } else {
        Err(AppError::not_found(
            codes::WHATSAPP_LINK_NOT_FOUND,
            "whatsapp linking is not available on this deployment",
        ))
    }
}

/// A caller-supplied operator-managed config (`telegram_app` chat id) would
/// let anyone alert-spam an arbitrary destination with our credentials —
/// only the transport's own flow may mint one.
pub(crate) fn reject_managed_kind(cfg: &ChannelConfig) -> Result<()> {
    if cfg.operator_managed() {
        return Err(AppError::unprocessable(
            codes::CHANNEL_KIND_MANAGED,
            "telegram channels are created by linking a chat through the bot; \
             mint a link code instead of supplying config",
        ));
    }
    Ok(())
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    validate_channel_name(name)
        .map_err(|m| AppError::bad_request_field(codes::CHANNEL_NAME_INVALID, m, "name"))
}

/// Monitor-tag rules and bounds — a rule that cannot name a legal tag would
/// match nothing — reported against the field this request sent.
fn normalize_rule_tags(tags: &[String]) -> Result<Vec<String>> {
    let field = |e| match e {
        AppError::BadRequest { code, message, .. } => {
            AppError::bad_request_field(code, message, "auto_bind_tags")
        }
        other => other,
    };
    // The list the client sent, so a positional fault names the tag they typed.
    match crate::api::handlers::targets::normalize_tags(tags) {
        Ok(valid) => Ok(fold_spellings(&valid)),
        // Matching folds case, so spellings that collapse into one entry must
        // not spend the cap twice.
        Err(AppError::BadRequest { code, .. }) if code == codes::TOO_MANY_TAGS => {
            crate::api::handlers::targets::normalize_tags(&fold_spellings(tags))
                .map(|v| fold_spellings(&v))
                .map_err(field)
        }
        Err(e) => Err(field(e)),
    }
}

/// One entry per tag, first spelling kept: it is what the operator typed and
/// what the chips show.
fn fold_spellings(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = tag.trim();
        if !out
            .iter()
            .any(|kept: &String| kept.to_lowercase() == tag.to_lowercase())
        {
            out.push(tag.to_string());
        }
    }
    out
}

/// Redaction-sentinel guard first (so a `GET → PATCH` round-trip or a
/// copy-pasted redacted create reports `REDACTION_SENTINEL`, not a generic
/// invalid-URL — `***` does not parse as a URL), then the structural
/// transport check.
pub(crate) fn validate_config(cfg: &crate::domain::ChannelConfig) -> Result<()> {
    if cfg.has_redaction_sentinel() {
        return Err(AppError::bad_request_field(
            codes::REDACTION_SENTINEL,
            "config still contains the redaction sentinel; send the real secret \
             or omit config to keep the stored value",
            "config",
        ));
    }
    cfg.validate()
        .map_err(|m| AppError::bad_request_field(codes::INVALID_CHANNEL_CONFIG, m, "config"))?;
    Ok(())
}

/// Deny-list gate for the config's outbound URL, mirroring the targets
/// test path: a hit is recorded as an `abuse_blocked` quota event and
/// rejected. Transports with a fixed vendor endpoint expose no URL and
/// pass through.
/// `established` skips the deliverability gate only; the deny-list is a
/// security control and always applies.
pub(crate) async fn check_channel_abuse(
    state: &AppState,
    org: crate::domain::OrgId,
    config: &ChannelConfig,
    established: bool,
) -> Result<()> {
    if let ChannelConfig::Email(cfg) = config {
        let ops = crate::security::abuse::operator_domains(
            &state.cfg.email.from_address,
            &state.cfg.auth.public_base_url,
        );
        if let Some(detail) = crate::security::abuse::blocked_email_destination(&cfg.to, &ops) {
            crate::quotas::service::record_quota_event(
                state.db.clone(),
                Some(org),
                None,
                "abuse_blocked",
                Some("email_destination"),
                serde_json::json!({ "detail": detail }),
                None,
            );
            return Err(AppError::bad_request_field(
                codes::EMAIL_DESTINATION_BLOCKED,
                detail,
                "config.to",
            ));
        }
        // Refused whatever `signup_policy` says: an alert nobody can read is
        // worse than no channel, because it looks configured. Not for an
        // already-verified address — a list changing its mind would lock the
        // owner out of a channel that is still delivering, and the pinned floor
        // is compile-time, so nothing short of a release could clear it.
        if !established
            && let Some(risk) = state
                .undeliverable_email(&cfg.to, "notification_channel")
                .await
        {
            crate::quotas::service::record_quota_event(
                state.db.clone(),
                Some(org),
                None,
                "abuse_blocked",
                Some("email_destination"),
                serde_json::json!({ "detail": risk.as_db_str() }),
                None,
            );
            return Err(risk.into_app_error("config.to"));
        }
        return Ok(());
    }
    let Some(url) = config.abuse_url() else {
        return Ok(());
    };
    let Some(hit) = state.abuse.inspect_url(url) else {
        return Ok(());
    };
    crate::quotas::service::record_quota_event(
        state.db.clone(),
        Some(org),
        None,
        "abuse_blocked",
        Some(hit.quota_name()),
        serde_json::json!({ "detail": hit.detail }),
        None,
    );
    Err(hit.into_app_error())
}

#[cfg(test)]
mod tests {
    use super::normalize_rule_tags;

    /// Matching folds case, so storing both spellings would spend two of the
    /// tag budget on one rule and show the operator a duplicate chip.
    #[test]
    fn a_rule_tag_repeated_in_another_case_is_stored_once() {
        assert_eq!(
            normalize_rule_tags(&["DB".into(), "db".into(), "cache".into()]).unwrap(),
            vec!["DB".to_string(), "cache".to_string()],
            "the first spelling is kept for display"
        );
        // Folded before the cap: these fit once folded, so the cap must not
        // count spellings the store will never hold.
        let over_cap: Vec<String> = (0..crate::domain::target::MAX_TAGS_PER_TARGET)
            .map(|i| format!("tag{i}"))
            .chain(["TAG0".to_string()])
            .collect();
        assert!(normalize_rule_tags(&over_cap).is_ok());
    }
}
