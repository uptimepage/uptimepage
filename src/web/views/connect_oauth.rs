//! Provider-parameterized pieces shared by the OAuth connect flows
//! (`slack_connect`, `discord_connect`, and the delegate starts), including
//! the whole callback body — the per-provider handlers only supply the code
//! exchange.

use axum::Json;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::{discord, oauth_state, slack};
use crate::config::{AppConfig, ConnectOauthConfig};
use crate::domain::credential::{DISCORD_CONNECT_PROVIDER, SLACK_CONNECT_PROVIDER};
use crate::domain::{ChannelConfig, OrgId};
use crate::error::{AppError, Result};
use crate::storage::orgs::is_active_member;
use crate::web::CurrentUser;
use crate::web::views::delegate_connect::{audit_delegated_create, finish_create};
use crate::web::views::notification_channels::{QuotaBlockLog, create_channel_deduped};

/// Everything that distinguishes one OAuth connect provider from another.
pub struct ConnectProvider {
    /// Path segment of the callback route, delegate `kind_hint`, and the
    /// bounce query key.
    pub kind: &'static str,
    /// `oauth_states.provider` value the starts mint.
    pub state_provider: &'static str,
    /// Dashboard form the callback bounces back to.
    pub form_url: &'static str,
    /// Audit/quota-log flow tag.
    pub flow: &'static str,
    pub cfg: fn(&AppConfig) -> &ConnectOauthConfig,
    pub authorize_url: fn(&ConnectOauthConfig, &str, &str) -> String,
}

pub const SLACK: ConnectProvider = ConnectProvider {
    kind: "slack",
    state_provider: SLACK_CONNECT_PROVIDER,
    form_url: "/settings/notifications/new?kind=slack",
    flow: "slack_oauth",
    cfg: |c| &c.slack_oauth,
    authorize_url: slack::authorize_url,
};

pub const DISCORD: ConnectProvider = ConnectProvider {
    kind: "discord",
    state_provider: DISCORD_CONNECT_PROVIDER,
    form_url: "/settings/notifications/new?kind=discord",
    flow: "discord_oauth",
    cfg: |c| &c.discord_oauth,
    authorize_url: discord::authorize_url,
};

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    /// `json` returns `{ "url": … }` for the QR variant instead of a 302.
    pub format: Option<String>,
}

impl StartQuery {
    pub fn wants_json(&self) -> bool {
        self.format.as_deref() == Some("json")
    }
}

/// `?<kind>=<outcome>` appended to the bounce target, which already carries
/// a query string on the dashboard form but not on a `/c/<code>` page.
pub fn bounce(p: &ConnectProvider, base: &str, outcome: &str) -> Response {
    let sep = if base.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{base}{sep}{}={outcome}", p.kind)).into_response()
}

pub fn callback_uri(state: &AppState, p: &ConnectProvider) -> String {
    format!(
        "{}/auth/{}/callback",
        state.cfg.auth.public_base_url.trim_end_matches('/'),
        p.kind
    )
}

pub fn invalid_state() -> AppError {
    AppError::forbidden_code("INVALID_STATE", "OAuth state is invalid or has expired")
}

/// Mints an `oauth_states` row bound to `org` (and, for delegate starts,
/// the link), then answers with the provider's authorize URL.
pub async fn mint_start_response(
    state: &AppState,
    p: &ConnectProvider,
    wants_json: bool,
    org: OrgId,
    link_code_id: Option<Uuid>,
) -> Result<Response> {
    let pool = state.require_db()?;
    let s = oauth_state::generate_state();
    oauth_state::insert(
        pool,
        &s,
        p.state_provider,
        oauth_state::StateBinding {
            org_id: Some(org.0),
            link_code_id,
            ..Default::default()
        },
    )
    .await?;
    let url = (p.authorize_url)((p.cfg)(&state.cfg), &callback_uri(state, p), &s);
    Ok(if wants_json {
        Json(json!({ "url": url })).into_response()
    } else {
        Redirect::to(&url).into_response()
    })
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

/// The callback body shared by every connect provider; `exchange` turns the
/// authorization code into a channel config plus default name.
pub async fn run_callback(
    state: &AppState,
    p: &ConnectProvider,
    user: Result<CurrentUser, AppError>,
    client_ip: &str,
    q: CallbackQuery,
    exchange: impl AsyncFnOnce(String) -> Result<(ChannelConfig, String)>,
) -> Result<Response> {
    let pool = state.require_db()?;
    let raw_state = q.state.as_deref().unwrap_or_default();
    if raw_state.is_empty() {
        return Err(invalid_state());
    }
    // Consume FIRST — also on the user-cancelled path, so a denied dance
    // burns its state.
    let Some(consumed) = oauth_state::consume(pool, raw_state).await? else {
        tracing::info!(
            provider = p.kind,
            reason = "state_unknown_or_expired",
            "oauth connect rejected"
        );
        return Err(invalid_state());
    };
    if consumed.provider != p.state_provider {
        tracing::info!(
            provider = p.kind,
            reason = "provider_mismatch",
            "oauth connect rejected"
        );
        return Err(invalid_state());
    }
    let Some(org) = consumed.org_id.map(OrgId) else {
        tracing::warn!(
            provider = p.kind,
            reason = "state_without_org",
            "oauth connect rejected"
        );
        return Err(invalid_state());
    };
    // Authority: a delegate-minted state rides the link's one-channel
    // budget; a dashboard-minted state needs the session user to still be
    // an active member of the state org.
    let session_user = if consumed.link_code_id.is_none() {
        let CurrentUser(user_id) = user?;
        if !is_active_member(pool, user_id, org).await? {
            tracing::info!(
                provider = p.kind,
                org_id = %org.0,
                reason = "membership_lost",
                "oauth connect rejected"
            );
            return Err(AppError::Forbidden);
        }
        Some(user_id)
    } else {
        None
    };
    let bounce_base =
        consumed
            .redirect_after
            .as_deref()
            .unwrap_or(if consumed.link_code_id.is_some() {
                "/c/done"
            } else {
                p.form_url
            });
    if let Some(err) = q.error.as_deref().filter(|e| !e.is_empty()) {
        tracing::info!(provider = p.kind, org_id = %org.0, reason = err, "oauth connect cancelled");
        return Ok(bounce(p, bounce_base, "cancelled"));
    }
    let Some(code) = q.code.as_deref().filter(|c| !c.is_empty()) else {
        return Err(AppError::bad_request(
            "INVALID_STATE",
            "OAuth callback carried neither code nor error",
        ));
    };
    // Spend the delegate link before the upstream exchange; every failure
    // path below restores it so a transient error doesn't burn the invite.
    let delegate = match consumed.link_code_id {
        Some(link_id) => match state.channel_link_code_store.consume_by_id(link_id).await? {
            Some(link) => Some(link),
            None => {
                tracing::info!(
                    provider = p.kind,
                    reason = "delegate_link_dead",
                    "oauth connect rejected"
                );
                return Err(invalid_state());
            }
        },
        None => None,
    };

    let (config, base_name) = match exchange(code.to_string()).await {
        Ok(out) => out,
        Err(err) => {
            tracing::warn!(provider = p.kind, org_id = %org.0, error = %err, "oauth connect exchange failed");
            if let Some(link) = &delegate {
                state.channel_link_code_store.restore(link.id).await?;
            }
            return Ok(bounce(p, bounce_base, "failed"));
        }
    };
    // The provider minted the value, but the shared transport validation
    // still runs — a malformed config must not enter the store through this
    // side door.
    if let Err(err) = config.validate() {
        tracing::warn!(provider = p.kind, org_id = %org.0, error = %err, "oauth connect returned an invalid config");
        if let Some(link) = &delegate {
            state.channel_link_code_store.restore(link.id).await?;
        }
        return Ok(bounce(p, bounce_base, "failed"));
    }

    if let Some(link) = &delegate {
        let base_name = link.channel_name.clone().unwrap_or(base_name);
        return match finish_create(state, link, &base_name, config, p.flow).await {
            Ok(ch) => {
                audit_delegated_create(state, org, &ch, client_ip).await;
                Ok(Redirect::to("/c/done").into_response())
            }
            Err(AppError::Unprocessable { code, .. })
                if code == crate::error::codes::CHANNEL_QUOTA_EXCEEDED =>
            {
                state.channel_link_code_store.restore(link.id).await?;
                Ok(bounce(p, bounce_base, "quota"))
            }
            Err(err) => {
                state.channel_link_code_store.restore(link.id).await?;
                Err(err)
            }
        };
    }

    let limit = i64::from(
        state
            .quotas
            .limit_for_org(org)
            .await?
            .max_notification_channels,
    );
    match create_channel_deduped(
        state.notification_channel_store.as_ref(),
        org,
        &base_name,
        config,
        limit,
        QuotaBlockLog {
            db: state.db.clone(),
            user: session_user,
            flow: p.flow,
        },
    )
    .await
    {
        Ok(ch) => {
            tracing::info!(provider = p.kind, org_id = %org.0, channel_id = %ch.id, "oauth channel connected");
            Ok(Redirect::to(&format!("/settings/notifications/{}/edit", ch.id)).into_response())
        }
        Err(AppError::Unprocessable { code, .. })
            if code == crate::error::codes::CHANNEL_QUOTA_EXCEEDED =>
        {
            tracing::info!(provider = p.kind, org_id = %org.0, reason = "quota", "oauth connect rejected");
            Ok(bounce(p, bounce_base, "quota"))
        }
        Err(err) => Err(err),
    }
}
