//! The tail every sign-in shares once a credential is proved: fixation
//! defence, the session row, the audit row, and the cookies a browser needs
//! before it lands. The OAuth callback, the magic-link redeem and the passkey
//! ceremony each prove a user their own way and hand over here.

use std::net::IpAddr;

use axum::http::HeaderMap;
use sqlx::PgPool;
use tower_cookies::Cookies;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::login_audit::{self, LoginAttempt, LoginMethod};
use crate::auth::magic_link::RedeemedVia;
use crate::auth::session as session_store;
use crate::auth::url::{safe_redirect_target, url_encode};
use crate::domain::{OrgId, UserId};
use crate::error::Result;

use super::invitations::{self, AcceptedInvitation};

/// What the dance carried and what came of it.
pub(super) enum Invited {
    Nobody,
    /// The invitation died between the mail and the sign-in. The sign-in
    /// stands: the credential was proved regardless.
    Missed,
    Joined(AcceptedInvitation),
}

impl Invited {
    pub(super) fn org(&self) -> Option<OrgId> {
        match self {
            Self::Joined(joined) => Some(joined.org_id),
            Self::Nobody | Self::Missed => None,
        }
    }
}

/// Redeems a carried invitation before the session is minted so the session
/// opens directly in the joined org. Soft-fails: a stale, raced or over-quota
/// invitation never breaks the sign-in itself.
pub(super) async fn redeem(
    state: &AppState,
    user_id: UserId,
    invitation_id: Option<Uuid>,
) -> Invited {
    match invitation_id {
        None => Invited::Nobody,
        Some(id) => match invitations::try_auto_accept(state, user_id, id).await {
            Some(joined) => Invited::Joined(joined),
            None => Invited::Missed,
        },
    }
}

/// The org the session opens in: the one just joined, else whatever the
/// account holds or gets on demand.
pub(super) async fn session_org(
    pool: &PgPool,
    invited: &Invited,
    user_id: UserId,
    pending_deletion: bool,
) -> Result<Option<OrgId>> {
    match invited.org() {
        Some(org) => Ok(Some(org)),
        None => crate::storage::users::session_org(pool, user_id, pending_deletion).await,
    }
}

/// Everything a proved sign-in carries into the session.
pub(super) struct Proved<'a> {
    pub user_id: UserId,
    pub method: LoginMethod,
    pub new_user: bool,
    pub pending_deletion: bool,
    pub active_org: Option<OrgId>,
    pub invited: Invited,
    pub redirect_after: Option<&'a str>,
    pub via: Option<RedeemedVia>,
    pub ip_hash: Option<&'a str>,
    pub ua_hash: Option<&'a str>,
}

/// Mints the session and returns where the browser goes next. A pending
/// deletion outranks every destination: the session only proves who is asking.
pub(super) async fn complete(
    state: &AppState,
    cookies: &Cookies,
    client_ip: IpAddr,
    headers: &HeaderMap,
    proved: Proved<'_>,
) -> Result<String> {
    let Proved {
        user_id,
        method,
        new_user,
        pending_deletion,
        active_org,
        invited,
        redirect_after,
        via,
        ip_hash,
        ua_hash,
    } = proved;
    let pool = state.require_db()?;
    let session_cfg = &state.cfg.auth.session;

    // Session fixation: drop any pre-login session bound to this browser
    // before minting the new one. Without this an attacker who pre-seeded a
    // cookie inherits the just-authenticated session.
    if let Some(prev) = cookies
        .get(&session_cfg.cookie_name)
        .map(|c| c.value().to_string())
        && !prev.is_empty()
        && let Err(err) = session_store::destroy(pool, &prev).await
    {
        tracing::warn!(error = %err, "session fixation: pre-login destroy failed");
    }

    let created =
        session_store::create(pool, session_cfg, user_id, active_org, ip_hash, ua_hash).await?;

    // Audit post-commit: a failure here logs but the session is already valid.
    if let Err(err) = login_audit::record(
        pool,
        method,
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
        tracing::warn!(error = %err, "login_audit write failed (non-fatal)");
    }

    // The one line that says a sign-in happened. `login_attempts` holds the
    // durable record, but following a support report through the log stream
    // otherwise means querying the database to find out anything happened at
    // all. Ids only: the address belongs in neither logs nor metrics.
    tracing::info!(
        user_id = %user_id.0,
        method = method.as_db_str(),
        new_user,
        org_id = ?active_org.map(|o| o.0),
        "sign-in complete"
    );

    crate::analytics::track_login(
        &state.outbound_http,
        &state.cfg.auth.public_base_url,
        crate::analytics::Login {
            method,
            new_user,
            redirect_after,
            via,
        },
        client_ip,
        headers,
    );

    cookies.add(session_store::build_cookie(
        session_cfg,
        created.cookie_token,
    ));
    crate::request::login_hint::set(cookies, session_cfg, method.as_db_str());
    if let Err(err) = crate::request::display_prefs::issue_cookies(
        pool,
        session_cfg.cookie_secure,
        cookies,
        user_id,
    )
    .await
    {
        tracing::warn!(error = %err, "display-preference cookie issue failed (non-fatal)");
    }

    // One-shot banners ride a flash cookie (unspoofable, fires once); only the
    // slug-validated `joined` stays a query param.
    let invite_missed = matches!(invited, Invited::Missed);
    crate::request::flash::set(
        cookies,
        &crate::request::flash::Flash {
            invite_missed,
            ..Default::default()
        },
        session_cfg.cookie_secure,
        &session_cfg.cookie_domain,
    );

    Ok(if pending_deletion {
        super::auth::RESTORE_PATH.to_string()
    } else {
        match invited {
            Invited::Joined(joined) => format!("/?joined={}", url_encode(&joined.org_slug)),
            Invited::Missed => "/".to_string(),
            Invited::Nobody => redirect_after
                .and_then(safe_redirect_target)
                .map(str::to_string)
                .unwrap_or_else(|| "/".to_string()),
        }
    })
}
