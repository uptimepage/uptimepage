//! Public delegation connect page (`/c/<code>`): the person who owns the
//! Slack workspace / Telegram group / inbox attaches ONE notification
//! channel to the inviting org without an account. Possession of the code
//! is the whole authority — it can create exactly one channel and read
//! nothing; expired, revoked, or spent codes all render the same generic
//! 404 page (no enumeration signal).

use askama::Template;
use askama_web::WebTemplate;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::api::error::codes;
use crate::api::handlers::notification_channels::{
    check_channel_abuse, delegate_status_parts, reject_managed_kind, spawn_send_verification,
    validate_config, validate_name,
};
use crate::app::AppState;
use crate::auth::sha256_hex;
use crate::domain::{ChannelConfig, ChannelKind, NotificationChannel, OrgId};
use crate::error::{AppError, Result};
use crate::storage::LinkPurpose;
use crate::storage::channel_link_codes::ConsumedLink;
use crate::storage::orgs::record_audit_tx;
use crate::web::client_ip::ClientIp;
use crate::web::filters;
use crate::web::views::channel_kind_label;
use crate::web::views::connect_oauth::{self, ConnectProvider, StartQuery, mint_start_response};
use crate::web::views::notification_channels::{QuotaBlockLog, create_channel_deduped};

/// Manual-form kinds the page offers. Multi-secret transports (BYO
/// telegram bot, WhatsApp) stay dashboard-only.
const MANUAL_KINDS: &[ChannelKind] = &[
    ChannelKind::Slack,
    ChannelKind::Discord,
    ChannelKind::MsTeams,
    ChannelKind::GoogleChat,
    ChannelKind::Email,
    ChannelKind::Webhook,
];

#[derive(Template, WebTemplate)]
#[template(path = "delegate_connect.html")]
pub struct DelegateConnectPage {
    pub ok: bool,
    pub org_name: String,
    pub code: String,
    /// Pinned kind (db string) or empty = invitee picks.
    pub kind_hint: String,
    pub name_hint: String,
    pub offers_telegram: bool,
    pub offers_slack_oauth: bool,
    pub offers_discord_oauth: bool,
    pub telegram_deep_link: String,
    pub telegram_group_deep_link: String,
    /// `(db value, display label)` per offered manual kind.
    pub manual_kinds: Vec<(&'static str, &'static str)>,
}

impl DelegateConnectPage {
    fn invalid() -> Self {
        Self {
            ok: false,
            org_name: String::new(),
            code: String::new(),
            kind_hint: String::new(),
            name_hint: String::new(),
            offers_telegram: false,
            offers_slack_oauth: false,
            offers_discord_oauth: false,
            telegram_deep_link: String::new(),
            telegram_group_deep_link: String::new(),
            manual_kinds: Vec::new(),
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "delegate_done.html")]
pub struct DelegateDonePage {
    /// `false` = the dance was cancelled or failed; nothing was created.
    pub connected: bool,
}

fn not_found_page() -> Response {
    (StatusCode::NOT_FOUND, DelegateConnectPage::invalid()).into_response()
}

async fn live_link(state: &AppState, code: &str) -> Result<Option<ConsumedLink>> {
    if code.trim().is_empty() {
        return Ok(None);
    }
    state
        .channel_link_code_store
        .peek(LinkPurpose::Delegate, &sha256_hex(code.trim()))
        .await
}

pub async fn page(State(state): State<AppState>, Path(code): Path<String>) -> Result<Response> {
    let Some(link) = live_link(&state, &code).await? else {
        tracing::debug!(reason = "link_dead_or_unknown", "delegate page rejected");
        return Ok(not_found_page());
    };
    let org_name = match state.db.as_ref() {
        Some(pool) => crate::storage::get_org(pool, link.org_id)
            .await?
            .map(|o| o.name)
            .unwrap_or_default(),
        None => String::new(),
    };
    let kind_hint = link.kind_hint.unwrap_or_default();
    let pin = |k: &str| kind_hint.is_empty() || kind_hint == k;
    let bot = state.cfg.telegram.bot_username.trim_start_matches('@');
    let offers_telegram = state.cfg.telegram.enabled()
        && pin(ChannelKind::TelegramApp.as_db_str())
        && !bot.is_empty();
    let manual_kinds: Vec<(&'static str, &'static str)> = MANUAL_KINDS
        .iter()
        .filter(|k| pin(k.as_db_str()))
        .map(|k| (k.as_db_str(), channel_kind_label(*k)))
        .collect();
    Ok(DelegateConnectPage {
        ok: true,
        org_name,
        code: code.trim().to_string(),
        offers_telegram,
        offers_slack_oauth: state.cfg.slack_oauth.enabled() && pin("slack"),
        offers_discord_oauth: state.cfg.discord_oauth.enabled() && pin("discord"),
        telegram_deep_link: format!("https://t.me/{bot}?start={}", code.trim()),
        telegram_group_deep_link: format!("https://t.me/{bot}?startgroup={}", code.trim()),
        name_hint: link.channel_name.unwrap_or_default(),
        kind_hint,
        manual_kinds,
    }
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct DoneQuery {
    pub slack: Option<String>,
    pub discord: Option<String>,
}

pub async fn done(Query(q): Query<DoneQuery>) -> DelegateDonePage {
    DelegateDonePage {
        connected: q.slack.or(q.discord).is_none(),
    }
}

/// Poll for the page: flips to `consumed` when any of the connect paths
/// (telegram webhook, OAuth callback, manual create) lands the channel.
pub async fn status(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let status = state
        .channel_link_code_store
        .status_by_hash(LinkPurpose::Delegate, &sha256_hex(code.trim()))
        .await?
        .ok_or_else(|| {
            AppError::not_found(codes::DELEGATE_LINK_NOT_FOUND, "unknown delegation link")
        })?;
    let (status, _) = delegate_status_parts(status);
    Ok(Json(json!({ "status": status })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateCreateRequest {
    #[serde(default)]
    pub name: Option<String>,
    pub config: ChannelConfig,
}

pub async fn create(
    State(state): State<AppState>,
    ClientIp(client_ip): ClientIp,
    Path(code): Path<String>,
    Json(mut req): Json<DelegateCreateRequest>,
) -> Result<Json<serde_json::Value>> {
    let Some(link) = live_link(&state, &code).await? else {
        return Err(AppError::not_found(
            codes::DELEGATE_LINK_NOT_FOUND,
            "this link is invalid, expired, or already used",
        ));
    };
    if let Some(hint) = link.kind_hint.as_deref()
        && req.config.kind().as_db_str() != hint
    {
        return Err(AppError::unprocessable(
            codes::DELEGATE_KIND_INVALID,
            format!("this link only accepts a {hint} channel"),
        ));
    }
    reject_managed_kind(&req.config)?;
    req.config.normalize();
    validate_config(&req.config)?;
    check_channel_abuse(&state, link.org_id, &req.config, false).await?;
    if let Some(name) = req.name.as_deref().filter(|n| !n.trim().is_empty()) {
        validate_name(name)?;
    }
    let base_name = req
        .name
        .as_deref()
        .or(link.channel_name.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Delegated channel")
        .to_string();

    // Claim the code BEFORE the create so two submits race on the atomic
    // consume, not on the channel insert; a failed create restores it.
    let Some(link) = state.channel_link_code_store.consume_by_id(link.id).await? else {
        return Err(AppError::not_found(
            codes::DELEGATE_LINK_NOT_FOUND,
            "this link is invalid, expired, or already used",
        ));
    };
    let channel = match finish_create(&state, &link, &base_name, req.config, "form").await {
        Ok(ch) => ch,
        Err(err) => {
            state.channel_link_code_store.restore(link.id).await?;
            return Err(err);
        }
    };
    audit_delegated_create(&state, link.org_id, &channel, &client_ip.to_string()).await;
    Ok(Json(json!({ "channel_id": channel.id })))
}

/// Quota-capped create + link attach, shared by the manual form and the
/// OAuth callbacks' delegate branch.
pub(crate) async fn finish_create(
    state: &AppState,
    link: &ConsumedLink,
    base_name: &str,
    config: ChannelConfig,
    via: &'static str,
) -> Result<NotificationChannel> {
    let limit = i64::from(
        state
            .quotas
            .limit_for_org(link.org_id)
            .await?
            .max_notification_channels,
    );
    let channel = create_channel_deduped(
        state.notification_channel_store.as_ref(),
        link.org_id,
        base_name,
        config,
        limit,
        QuotaBlockLog {
            db: state.db.clone(),
            user: None,
            flow: via,
        },
    )
    .await?;
    // Best-effort: the channel exists and works either way, and never
    // failing after the create means a restore can never resurrect a link
    // whose channel was already made.
    if let Err(err) = state
        .channel_link_code_store
        .attach_channel(link.id, channel.id)
        .await
    {
        tracing::warn!(?err, channel_id = %channel.id, "delegate link attach failed");
    }
    if channel.awaiting_verification() {
        spawn_send_verification(state, link.org_id, &channel);
    }
    tracing::info!(
        org_id = %link.org_id.0,
        channel_id = %channel.id,
        via,
        "channel created via delegation link"
    );
    Ok(channel)
}

/// Best-effort compliance trail; the channel exists either way and the
/// tracing line above already records the event operationally.
pub(crate) async fn audit_delegated_create(
    state: &AppState,
    org: OrgId,
    channel: &NotificationChannel,
    client_ip: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let ip_hash = crate::auth::hash_fingerprint(&state.cfg.auth.fingerprint_salt, client_ip);
    let meta = json!({
        "channel_id": channel.id,
        "kind": channel.kind.as_db_str(),
        "ip_hash": ip_hash,
    });
    let res = async {
        let mut tx = pool.begin().await?;
        record_audit_tx(&mut tx, org, None, "channel.created_via_delegation", meta).await?;
        tx.commit().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(err) = res {
        tracing::warn!(org_id = %org.0, error = %err, "delegation audit write failed");
    }
}

/// One delegate OAuth start for every provider: link liveness + kind-hint
/// gate, then the shared state mint. No redirect_after — it would store the
/// raw delegate code at rest while the link table keeps only its hash; the
/// callback bounces delegate outcomes to /c/done instead.
async fn oauth_start(
    state: &AppState,
    code: &str,
    q: &StartQuery,
    p: &ConnectProvider,
) -> Result<Response> {
    if !(p.cfg)(&state.cfg).enabled() {
        return Ok(not_found_page());
    }
    let Some(link) = live_link(state, code).await? else {
        return Ok(not_found_page());
    };
    if link.kind_hint.as_deref().is_some_and(|h| h != p.kind) {
        return Ok(not_found_page());
    }
    mint_start_response(state, p, q.wants_json(), link.org_id, Some(link.id)).await
}

pub async fn slack_start(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(q): Query<StartQuery>,
) -> Result<Response> {
    oauth_start(&state, &code, &q, &connect_oauth::SLACK).await
}

pub async fn discord_start(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(q): Query<StartQuery>,
) -> Result<Response> {
    oauth_start(&state, &code, &q, &connect_oauth::DISCORD).await
}
