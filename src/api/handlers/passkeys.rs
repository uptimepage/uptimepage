//! The two passkey ceremonies. Registration attaches a credential to the
//! caller's own account; login resolves an account from the credential alone.
//!
//! Both halves of a ceremony are POSTs carrying an opaque handle. The
//! challenge state behind that handle is deleted as it is read, so a replayed
//! answer finds nothing.

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tower_cookies::Cookies;
use utoipa::ToSchema;
use webauthn_rs::prelude::*;

use crate::app::AppState;
use crate::auth::login_audit::{self, LoginAttempt, LoginMethod};
use crate::auth::session as session_store;
use crate::auth::{fingerprint, passkey};
use crate::domain::UserId;
use crate::error::{AppError, Result};
use crate::storage::{oauth_identities, passkeys};
use crate::web::auth::Session;
use crate::web::{BrowserUser, CurrentUser};

/// One ceremony's opaque handle plus the options `navigator.credentials` wants.
#[derive(Debug, Serialize, ToSchema)]
pub struct CeremonyStart {
    pub handle: String,
    #[schema(value_type = Object)]
    pub options: serde_json::Value,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FinishRegistration {
    pub handle: String,
    pub nickname: Option<String>,
    #[schema(value_type = Object)]
    pub credential: serde_json::Value,
}

/// Resolved at the start and kept with the challenge, so the answer cannot name
/// a destination the request did not.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(default)]
pub struct StartLogin {
    pub redirect_after: Option<String>,
    pub invitation: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LoginCeremony {
    ceremony: DiscoverableAuthentication,
    redirect_after: Option<String>,
    invitation_id: Option<uuid::Uuid>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FinishLogin {
    pub handle: String,
    #[schema(value_type = Object)]
    pub credential: serde_json::Value,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginComplete {
    /// By the same priority the OAuth callback settles on.
    pub redirect: String,
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/auth/passkey/register/start",
    tag = "account",
    summary = "Begin adding a passkey to the caller's account",
    responses((status = 200, body = CeremonyStart), (status = 401, description = "Not signed in")),
)]
pub async fn register_start(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<CeremonyStart>> {
    let user = signed_in(&session)?;
    let pool = state.require_db()?;
    let webauthn = enabled(&state)?;
    // Offering a credential the account already holds lets the authenticator
    // replace it rather than quietly minting a second one for the same device.
    let exclude = passkeys::credential_ids(pool, user.id).await?;
    let (options, ceremony) = webauthn
        .start_passkey_registration(user.id.0, &user.email, &user.email, Some(exclude))
        .map_err(|e| AppError::Other(anyhow::anyhow!("start passkey registration: {e}")))?;
    let handle = passkeys::generate_handle();
    passkeys::put_state(pool, &handle, Some(user.id), &ceremony).await?;
    Ok(Json(CeremonyStart {
        handle,
        options: serde_json::to_value(options)
            .map_err(|e| AppError::Other(anyhow::anyhow!("encode challenge: {e}")))?,
    }))
}

#[utoipa::path(
    post,
    path = "/auth/passkey/register/finish",
    tag = "account",
    summary = "Store the passkey the authenticator just minted",
    responses(
        (status = 204, description = "Added"),
        (status = 400, body = crate::api::error::ApiError, description = "Challenge expired, already answered, or the credential did not verify"),
        (status = 401, description = "Not signed in"),
    ),
)]
pub async fn register_finish(
    State(state): State<AppState>,
    session: Session,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    Json(body): Json<FinishRegistration>,
) -> Result<StatusCode> {
    let user = signed_in(&session)?;
    let pool = state.require_db()?;
    let webauthn = enabled(&state)?;
    let rp_id = passkey::relying_party_id(&state.cfg.auth.public_base_url)?;

    let Some((owner, ceremony)) =
        passkeys::take_state::<PasskeyRegistration>(pool, &body.handle).await?
    else {
        return Err(spent_registration());
    };
    // The handle is opaque but it is not a capability: without this, one
    // account could finish a ceremony another account started.
    if owner != Some(user.id) {
        tracing::warn!(
            user_id = %user.id.0,
            "passkey registration finished against a challenge another session started"
        );
        return Err(spent_registration());
    }

    let credential: RegisterPublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| {
            AppError::bad_request("PASSKEY_MALFORMED", "that is not a registration response")
        })?;
    // Sign-in is discoverable-only, so a non-resident credential could never
    // answer one and would still count as a way in. The hint is unsigned and
    // often absent, so only an explicit "no" is refused.
    if credential.extensions.cred_props.as_ref().and_then(|p| p.rk) == Some(false) {
        tracing::warn!(user_id = %user.id.0, "passkey registration was not discoverable");
        return Err(AppError::bad_request(
            "PASSKEY_NOT_DISCOVERABLE",
            "that authenticator did not save the passkey to itself, so it could not sign you in",
        ));
    }
    let stored = webauthn
        .finish_passkey_registration(&credential, &ceremony)
        .map_err(|e| {
            tracing::warn!(user_id = %user.id.0, error = %e, "passkey registration rejected");
            AppError::bad_request("PASSKEY_REJECTED", "that passkey could not be verified")
        })?;

    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    let ua_hash = fingerprint::hash_fingerprint(salt, user_agent(&headers));
    passkeys::insert(
        pool,
        user.id,
        &stored,
        &rp_id,
        nickname(body.nickname.as_deref()).as_deref(),
        oauth_identities::RequestOrigin {
            ip_hash: ip_hash.as_deref(),
            user_agent_hash: ua_hash.as_deref(),
        },
    )
    .await?;
    crate::api::handlers::auth::notify_credential_change_labelled(
        &state,
        &user.email,
        PASSKEY_LABEL,
        crate::api::handlers::auth::CredentialChange::Linked,
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/auth/passkey/login/start",
    tag = "auth",
    summary = "Begin a passkey sign-in",
    description = "Discoverable, so no address is named. Asking for one would \
                   answer whether that address has a passkey.",
    responses((status = 200, body = CeremonyStart), (status = 404, description = "Passkeys are off on this deployment")),
)]
pub async fn login_start(
    State(state): State<AppState>,
    Json(body): Json<StartLogin>,
) -> Result<Json<CeremonyStart>> {
    let pool = state.require_db()?;
    let webauthn = enabled(&state)?;
    let (options, ceremony) = webauthn
        .start_discoverable_authentication()
        .map_err(|e| AppError::Other(anyhow::anyhow!("start passkey login: {e}")))?;
    // Validated here rather than at the finish, for the same reason the OAuth
    // start does it: an open redirect is decided by what we agreed to honour.
    let carried = LoginCeremony {
        ceremony,
        redirect_after: crate::auth::url::safe_redirect_target(
            body.redirect_after.as_deref().unwrap_or_default(),
        )
        .map(str::to_string),
        invitation_id: crate::auth::invitations::resolve_pending_invitation_id(
            pool,
            body.invitation.as_deref(),
        )
        .await?,
    };
    let handle = passkeys::generate_handle();
    passkeys::put_state(pool, &handle, None, &carried).await?;
    Ok(Json(CeremonyStart {
        handle,
        options: serde_json::to_value(options)
            .map_err(|e| AppError::Other(anyhow::anyhow!("encode challenge: {e}")))?,
    }))
}

#[utoipa::path(
    post,
    path = "/auth/passkey/login/finish",
    tag = "auth",
    summary = "Complete a passkey sign-in and open a session",
    responses(
        (status = 200, body = LoginComplete),
        (status = 400, body = crate::api::error::ApiError, description = "Challenge expired, already answered, or the assertion did not verify"),
    ),
)]
pub async fn login_finish(
    State(state): State<AppState>,
    cookies: Cookies,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    Json(body): Json<FinishLogin>,
) -> Result<Response> {
    let pool = state.require_db()?;
    let webauthn = enabled(&state)?;
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    let ua_hash = fingerprint::hash_fingerprint(salt, user_agent(&headers));

    // Mirror of the registration check: a state row that names an owner is a
    // registration, and answering it here would consume it and leak that the
    // handle existed at all.
    let Some((owner, carried)) = passkeys::take_state::<LoginCeremony>(pool, &body.handle).await?
    else {
        return Err(spent_login());
    };
    if owner.is_some() {
        return Err(spent_login());
    }
    let credential: PublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| {
            AppError::bad_request(
                "PASSKEY_MALFORMED",
                "that is not an authentication response",
            )
        })?;

    // The user handle is the account id, but nothing is trusted from it until
    // the signature verifies against a credential that account holds.
    let claimed = match webauthn.identify_discoverable_authentication(&credential) {
        Ok((claimed, _)) => claimed,
        Err(_) => {
            return Err(refused(pool, &ip_hash, &ua_hash, "unidentifiable_credential").await);
        }
    };
    let user_id = UserId(claimed);

    let held = passkeys::passkeys_for_user(pool, user_id).await?;
    if held.is_empty() {
        return Err(refused(pool, &ip_hash, &ua_hash, "no_passkey_on_account").await);
    }
    let discoverable: Vec<DiscoverableKey> = held.iter().map(DiscoverableKey::from).collect();
    let result = match webauthn.finish_discoverable_authentication(
        &credential,
        carried.ceremony,
        &discoverable,
    ) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(error = %e, "passkey assertion rejected");
            return Err(refused(pool, &ip_hash, &ua_hash, "assertion_rejected").await);
        }
    };

    if let Some(mut used) = held.into_iter().find(|p| p.cred_id() == result.cred_id()) {
        let id: Vec<u8> = AsRef::<[u8]>::as_ref(used.cred_id()).to_vec();
        // Surfaced, not enforced: a stalled counter is the spec's cloned-key
        // signal, but refusing would lock someone out over a firmware quirk
        // and the assertion itself is already sound.
        if let Ok(Some(previous)) = passkeys::stored_counter(pool, &id).await
            && previous > 0
            && i64::from(result.counter()) <= previous
        {
            tracing::warn!(
                user_id = %user_id.0,
                previous,
                presented = result.counter(),
                "passkey signature counter did not advance; the authenticator may be cloned"
            );
            metrics::counter!(crate::observability::metrics::names::PASSKEY_COUNTER_STALLED)
                .increment(1);
        }
        // Unconditional: a synced passkey never advances a counter, so gating
        // on `update_credential` would freeze `last_used_at` at creation.
        used.update_credential(&result);
        if let Err(e) = passkeys::record_use(pool, &id, &used).await {
            tracing::warn!(error = %e, "passkey use not written back");
        }
    }

    complete_login(
        &state,
        &cookies,
        user_id,
        client_ip,
        &headers,
        Proved {
            ip_hash: ip_hash.as_deref(),
            ua_hash: ua_hash.as_deref(),
            redirect_after: carried.redirect_after.as_deref(),
            invitation_id: carried.invitation_id,
        },
    )
    .await
}

/// Everything a finished ceremony carries except the account it proved.
struct Proved<'a> {
    ip_hash: Option<&'a str>,
    ua_hash: Option<&'a str>,
    redirect_after: Option<&'a str>,
    invitation_id: Option<uuid::Uuid>,
}

/// What a sign-in owes once the credential is proved, fixation defence first.
async fn complete_login(
    state: &AppState,
    cookies: &Cookies,
    user_id: UserId,
    client_ip: std::net::IpAddr,
    headers: &HeaderMap,
    proved: Proved<'_>,
) -> Result<Response> {
    let Proved {
        ip_hash,
        ua_hash,
        redirect_after,
        invitation_id,
    } = proved;
    let pool = state.require_db()?;
    let pending_deletion = crate::storage::orgs::user_deleted_at(pool, user_id)
        .await?
        .is_some();
    // Redeemed server-side, so the session opens directly in the joined org.
    let joined = match invitation_id {
        Some(id) => crate::api::handlers::invitations::try_auto_accept(state, user_id, id).await,
        None => None,
    };
    let active_org = match joined.as_ref().map(|j| j.org_id) {
        Some(org) => Some(org),
        None => crate::storage::users::session_org(pool, user_id, pending_deletion).await?,
    };

    let cookie_name = state.cfg.auth.session.cookie_name.as_str();
    if let Some(prev) = cookies.get(cookie_name).map(|c| c.value().to_string())
        && !prev.is_empty()
        && let Err(err) = session_store::destroy(pool, &prev).await
    {
        tracing::warn!(error = %err, "passkey: pre-login session destroy failed");
    }

    let created = session_store::create(
        pool,
        &state.cfg.auth.session,
        user_id,
        active_org,
        ip_hash,
        ua_hash,
    )
    .await?;

    if let Err(err) = login_audit::record(
        pool,
        LoginMethod::Passkey,
        LoginAttempt {
            user_id: Some(user_id),
            success: true,
            ip_hash,
            user_agent_hash: ua_hash,
            failure_reason: None,
        },
    )
    .await
    {
        tracing::warn!(error = %err, "passkey audit write failed (non-fatal)");
    }

    tracing::info!(
        user_id = %user_id.0,
        provider = passkeys::PROVIDER_SLUG,
        org_id = ?active_org.map(|o| o.0),
        "sign-in complete"
    );
    crate::analytics::track_login(
        state,
        crate::analytics::Login {
            method: LoginMethod::Passkey,
            new_user: false,
            redirect_after,
            via: None,
        },
        client_ip,
        headers,
    );

    cookies.add(session_store::build_cookie(
        &state.cfg.auth.session,
        created.cookie_token,
    ));
    crate::web::login_hint::set(
        cookies,
        &state.cfg.auth.session,
        LoginMethod::Passkey.as_db_str(),
    );
    if let Err(err) = crate::web::display_prefs::issue_cookies(state, cookies, user_id).await {
        tracing::warn!(error = %err, "display-preference cookie issue failed (non-fatal)");
    }

    let invite_missed = joined.is_none() && invitation_id.is_some();
    crate::web::flash::set(
        cookies,
        &crate::web::flash::Flash {
            invite_missed,
            ..Default::default()
        },
        state.cfg.auth.session.cookie_secure,
        &state.cfg.auth.session.cookie_domain,
    );
    // Same order the OAuth callback settles on: the session only proves who is
    // asking, so a pending deletion outranks every destination.
    let redirect = if pending_deletion {
        crate::api::handlers::auth::RESTORE_PATH.to_string()
    } else if let Some(j) = joined {
        format!("/?joined={}", crate::auth::url::url_encode(&j.org_slug))
    } else if invite_missed {
        "/".to_string()
    } else {
        redirect_after.unwrap_or("/").to_string()
    };
    Ok(Json(LoginComplete { redirect }).into_response())
}

// ---------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------

#[utoipa::path(
    delete,
    path = "/api/v1/me/passkeys/{id}",
    tag = "account",
    summary = "Remove one passkey from the caller's account",
    params(("id" = String, Path, description = "Passkey id")),
    responses(
        (status = 204, description = "Removed"),
        (status = 400, body = crate::api::error::ApiError, description = "Would leave no way to sign in"),
        (status = 404, body = crate::api::error::ApiError, description = "No such passkey on this account"),
    ),
)]
pub async fn remove(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    session: Session,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let rp_id = passkey::relying_party_id(&state.cfg.auth.public_base_url).ok();
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    let ua_hash = fingerprint::hash_fingerprint(salt, user_agent(&headers));

    let email = passkeys::remove(
        pool,
        user_id,
        id,
        rp_id.as_deref(),
        &oauth_identities::WaysIn::from_config(&state.cfg),
        oauth_identities::RequestOrigin {
            ip_hash: ip_hash.as_deref(),
            user_agent_hash: ua_hash.as_deref(),
        },
    )
    .await?;
    // A session this passkey opened would outlive it by its absolute timeout,
    // which is what the confirm dialog promises does not happen.
    revoke_other_sessions(pool, user_id, session.session_id_hash.as_deref()).await;
    crate::api::handlers::auth::notify_credential_change_labelled(
        &state,
        &email,
        PASSKEY_LABEL,
        crate::api::handlers::auth::CredentialChange::Unlinked,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Same rule as unlinking a provider: everything but the caller's own session.
async fn revoke_other_sessions(pool: &sqlx::PgPool, user_id: UserId, keep: Option<&str>) {
    let revoked = match keep {
        Some(keep) => crate::auth::session::destroy_others_for_user(pool, user_id, keep).await,
        // Unreachable behind `BrowserUser`. If reached, we cannot tell which
        // session is the caller's, and leaving one alive is the worse mistake.
        None => crate::auth::session::destroy_all_for_user(pool, user_id).await,
    };
    match revoked {
        Ok(n) if n > 0 => {
            tracing::info!(user_id = %user_id.0, revoked = n, "other sessions revoked with a passkey")
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, user_id = %user_id.0, "revoking other sessions failed")
        }
    }
}

// ---------------------------------------------------------------------------

const PASSKEY_LABEL: &str = "Passkey";

/// The column is unbounded text, so a label is trimmed and capped before it
/// reaches a row.
const NICKNAME_MAX_CHARS: usize = 64;

fn nickname(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    (!trimmed.is_empty()).then(|| trimmed.chars().take(NICKNAME_MAX_CHARS).collect())
}

fn signed_in(session: &Session) -> Result<crate::web::auth::User> {
    session.user.clone().ok_or_else(|| AppError::Unauthorized)
}

fn enabled(state: &AppState) -> Result<Webauthn> {
    if !state.cfg.auth.passkey_login_enabled() {
        return Err(AppError::not_found(
            "PASSKEY_UNAVAILABLE",
            "passkeys are not enabled here",
        ));
    }
    passkey::build(&state.cfg.auth.public_base_url)
}

/// One answer for expired, already-answered, mis-owned and refused, so a caller
/// cannot tell which by probing. It names no cause for the same reason: a
/// credential this deployment will never accept is not worth retrying, and
/// saying "expired" would send its owner round that loop for good.
fn spent_login() -> AppError {
    AppError::bad_request(
        "PASSKEY_CHALLENGE_SPENT",
        "that passkey did not work, try again or use another way in",
    )
}

/// The same refusal on the settings page, where the caller is already signed in
/// and has no other way in to reach for.
fn spent_registration() -> AppError {
    AppError::bad_request("PASSKEY_CHALLENGE_SPENT", "that took too long, start again")
}

fn user_agent(headers: &HeaderMap) -> &str {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

async fn refused(
    pool: &sqlx::PgPool,
    ip_hash: &Option<String>,
    ua_hash: &Option<String>,
    reason: &'static str,
) -> AppError {
    tracing::warn!(reason, "passkey sign-in refused");
    metrics::counter!(
        crate::observability::metrics::names::PASSKEY_LOGIN_REFUSED,
        "reason" => reason,
    )
    .increment(1);
    login_audit::record_failure_anon(
        pool,
        LoginMethod::Passkey,
        ip_hash.as_deref(),
        ua_hash.as_deref(),
        reason,
    )
    .await;
    spent_login()
}
