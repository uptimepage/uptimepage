//! Removing a sign-in method from the caller's own account. Adding one is the
//! dance in [`crate::api::handlers::auth`].

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::app::AppState;
use crate::auth::passkey;
use crate::domain::OauthProvider;
use crate::error::Result;
use crate::storage::oauth_identities;
use crate::web::{BrowserUser, CurrentUser};

#[derive(Debug, Deserialize, ToSchema)]
pub struct UnlinkQuery {
    /// Which account at that provider, so a second one from the same vendor is
    /// not swept along. Omitted when only one is linked.
    pub provider_user_id: Option<String>,
}

#[utoipa::path(
    delete,
    path = "/api/v1/me/sign-in-methods/{provider}",
    tag = "account",
    summary = "Remove one provider account from the caller's sign-in methods",
    description = "Every linked provider account signs in to this one on its \
                   own, so removing one takes a credential away. Rejects with \
                   400 LAST_SIGN_IN_METHOD when it would leave the account \
                   with no way in at all — which, on a deployment that offers \
                   magic-link sign-in, it never does. Adding a method is the \
                   dance at POST /auth/{provider}/link.",
    params(
        ("provider" = OauthProvider, Path, description = "Provider to unlink"),
        ("provider_user_id" = Option<String>, Query, description = "Which account at that provider"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 400, body = crate::api::error::ApiError, description = "Would leave no way to sign in"),
        (status = 404, body = crate::api::error::ApiError, description = "No such sign-in method on this account"),
    ),
)]
pub async fn unlink(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    session: crate::web::auth::Session,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    headers: axum::http::HeaderMap,
    Path(provider): Path<OauthProvider>,
    Query(q): Query<UnlinkQuery>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = crate::auth::fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    let ua_hash = crate::auth::fingerprint::hash_fingerprint(
        salt,
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
    );
    let rp_id = passkey_rp_id(&state);
    let email = oauth_identities::unlink(
        pool,
        user_id,
        provider,
        q.provider_user_id.as_deref(),
        &crate::auth::ways_in(&state.cfg),
        rp_id.as_deref(),
        oauth_identities::RequestOrigin {
            ip_hash: ip_hash.as_deref(),
            user_agent_hash: ua_hash.as_deref(),
        },
    )
    .await?;
    // A session the removed credential opened is what someone would actually
    // be holding, and it would outlive the removal by its absolute timeout.
    let revoked = match session.session_id_hash.as_deref() {
        Some(keep) => crate::auth::session::destroy_others_for_user(pool, user_id, keep).await,
        // Unreachable behind `BrowserUser`. If reached, we cannot tell which
        // session is the caller's, and leaving one alive is the worse mistake.
        None => crate::auth::session::destroy_all_for_user(pool, user_id).await,
    };
    match revoked {
        Ok(n) if n > 0 => {
            tracing::info!(user_id = %user_id.0, revoked = n, "other sessions revoked with a sign-in method")
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, user_id = %user_id.0, "revoking other sessions failed")
        }
    }

    crate::api::handlers::auth::notify_credential_change(
        &state,
        &email,
        provider,
        crate::api::handlers::auth::CredentialChange::Unlinked,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `None` where no passkey could sign anyone in, so the rows this deployment
/// cannot answer for are counted as rows rather than ways back.
fn passkey_rp_id(state: &AppState) -> Option<String> {
    passkey::login_enabled(&state.cfg.auth)
        .then(|| passkey::relying_party_id(&state.cfg.auth.public_base_url).ok())
        .flatten()
}
