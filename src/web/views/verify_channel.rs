//! Public `GET /verify-channel?token=…` — consumes an email-channel
//! verification token and stamps the channel verified. Chrome-less page,
//! unauthenticated by design: possession of the mailed token is the proof.
//! Every miss (unknown, expired, used, address changed since mint) renders
//! one generic invalid page so the surface gives no enumeration signal.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::app::AppState;
use crate::domain::{ChannelConfig, OrgId};
use crate::storage::channel_verification;
use crate::templates::filters;
use crate::web::error::WebResult;

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    #[serde(default)]
    pub token: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "verify_channel.html")]
pub struct VerifyChannelPage {
    pub ok: bool,
    /// Recipient address, shown on success only.
    pub email: String,
}

pub async fn verify(
    State(state): State<AppState>,
    Query(q): Query<VerifyQuery>,
) -> WebResult<Response> {
    let invalid = || {
        (
            StatusCode::NOT_FOUND,
            VerifyChannelPage {
                ok: false,
                email: String::new(),
            },
        )
            .into_response()
    };
    let token = q.token.trim();
    if token.is_empty() {
        tracing::debug!(reason = "empty_token", "channel verification rejected");
        return Ok(invalid());
    }
    let Some(pool) = state.db.as_ref() else {
        tracing::warn!(reason = "db_unavailable", "channel verification rejected");
        return Ok(invalid());
    };
    let Some(consumed) = channel_verification::consume(pool, token).await? else {
        tracing::debug!(
            reason = "token_unknown_expired_or_used",
            "channel verification rejected"
        );
        return Ok(invalid());
    };
    let org = OrgId(consumed.org_id);
    let Some(channel) = state
        .notification_channel_store
        .get(org, consumed.channel_id)
        .await?
    else {
        tracing::info!(
            channel_id = %consumed.channel_id,
            reason = "channel_gone",
            "channel verification rejected"
        );
        return Ok(invalid());
    };
    // The token proves the address it was mailed to — not whatever address
    // the channel may have been edited to since.
    let ChannelConfig::Email(cfg) = &channel.config else {
        tracing::info!(
            channel_id = %channel.id,
            reason = "kind_changed",
            "channel verification rejected"
        );
        return Ok(invalid());
    };
    if !cfg.to.eq_ignore_ascii_case(&consumed.email) {
        tracing::info!(
            channel_id = %channel.id,
            reason = "address_changed_since_mint",
            "channel verification rejected"
        );
        return Ok(invalid());
    }
    if !state
        .notification_channel_store
        .set_verified(org, channel.id, channel.updated_at)
        .await?
    {
        tracing::info!(
            channel_id = %channel.id,
            reason = "config_updated_concurrently",
            "channel verification rejected"
        );
        return Ok(invalid());
    }
    tracing::info!(channel_id = %channel.id, "email channel verified");
    Ok(VerifyChannelPage {
        ok: true,
        email: cfg.to.clone(),
    }
    .into_response())
}
