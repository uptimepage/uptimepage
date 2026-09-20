//! The tail every sign-in shares once a credential is proved: the org the
//! session opens in and the alert channel a freshly opened one gets, fixation
//! defence, the session row, the audit row, and the cookies a browser needs
//! before it lands. The OAuth callback, the magic-link redeem and the passkey
//! ceremony each prove a user their own way and hand over here.

use std::net::IpAddr;

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tower_cookies::Cookies;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::login_audit::{self, LoginAttempt, LoginMethod};
use crate::auth::magic_link::RedeemedVia;
use crate::auth::session as session_store;
use crate::auth::url::safe_redirect_target;
use crate::domain::{OrgId, UserId};
use crate::error::Result;
use crate::storage::users::SessionOrg;

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

/// Everything a proved sign-in carries into the session.
pub(super) struct Proved<'a> {
    pub user_id: UserId,
    pub method: LoginMethod,
    /// The account was created by the signup that ran just before this call.
    pub new_user: bool,
    /// Set when the account is inside its deletion grace window.
    pub pending_deletion: Option<DateTime<Utc>>,
    pub invited: Invited,
    pub redirect_after: Option<&'a str>,
    pub via: Option<RedeemedVia>,
    pub ip_hash: Option<&'a str>,
    pub ua_hash: Option<&'a str>,
}

/// The org this sign-in opened for the user, as opposed to joined or already
/// held: the one that gets the owner's alert channel. A signup that arrived
/// with no invitation opened the org it holds moments before this call, so
/// a held org counts as opened for it. A signup that carried an invitation
/// never opened one: whatever it holds when the accept misses is the
/// inviter's org, landed by another redeem of the same invitation.
fn opened_org(invited: &Invited, held: SessionOrg, new_user: bool) -> Option<OrgId> {
    match (invited, held) {
        (Invited::Joined(_), _) => None,
        (_, SessionOrg::Opened(org)) => Some(org),
        (Invited::Nobody, SessionOrg::Held(org)) if new_user => Some(org),
        (_, SessionOrg::Held(_) | SessionOrg::Absent) => None,
    }
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
        invited,
        redirect_after,
        via,
        ip_hash,
        ua_hash,
    } = proved;
    let pool = state.require_db()?;
    let session_cfg = &state.cfg.auth.session;

    if let Some(deleted_at) = pending_deletion {
        tracing::info!(
            user_id = %user_id.0,
            method = method.as_db_str(),
            deleted_at = %deleted_at,
            "sign-in on an account scheduled for deletion; routing to the restore choice"
        );
    }

    // Joined outranks held. A user left with no org at all gets a personal one
    // here: an invitee whose invitation died before the accept still holds a
    // proven identity, so the sign-in stands.
    let (active_org, opened) = match invited.org() {
        Some(org) => (Some(org), None),
        None => {
            let held =
                match crate::storage::users::session_org(pool, user_id, pending_deletion.is_some())
                    .await
                {
                    Ok(held) => held,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            user_id = %user_id.0,
                            method = method.as_db_str(),
                            "resolving the org a sign-in opens in failed"
                        );
                        login_audit::record(
                            pool,
                            method,
                            LoginAttempt {
                                user_id: Some(user_id),
                                success: false,
                                ip_hash,
                                user_agent_hash: ua_hash,
                                failure_reason: Some("signup_org_failed"),
                            },
                        )
                        .await;
                        return Err(e);
                    }
                };
            let active = match held {
                SessionOrg::Opened(org) | SessionOrg::Held(org) => Some(org),
                SessionOrg::Absent => None,
            };
            (active, opened_org(&invited, held, new_user))
        }
    };
    if let Some(org) = opened
        && let Some(email) = account_email(pool, user_id).await
    {
        crate::channels::seed_owner_email(
            state.notification_channel_store.as_ref(),
            &state.quotas,
            &state.cfg.email,
            org,
            user_id,
            &email,
        )
        .await;
    }

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

    // Post-commit: the session is already valid whatever the audit row does.
    login_audit::record(
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
    .await;

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

    Ok(if pending_deletion.is_some() {
        super::auth::RESTORE_PATH.to_string()
    } else {
        match invited {
            Invited::Joined(joined) => joined.landing_url(),
            Invited::Missed => "/".to_string(),
            Invited::Nobody => redirect_after
                .and_then(safe_redirect_target)
                .map(str::to_string)
                .unwrap_or_else(|| "/".to_string()),
        }
    })
}

/// A lookup failure reads as no address: the mail it feeds is the safety
/// story, so losing it leaves a line rather than failing the sign-in.
pub(super) async fn account_email(pool: &PgPool, user: UserId) -> Option<String> {
    match crate::storage::users::live_email(pool, user).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, user_id = %user.0, "account address lookup failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org() -> OrgId {
        OrgId(Uuid::now_v7())
    }

    fn joined(org: OrgId) -> Invited {
        Invited::Joined(AcceptedInvitation {
            org_id: org,
            org_slug: "team".into(),
        })
    }

    #[test]
    fn a_plain_signup_opens_the_org_it_holds() {
        let own = org();
        assert_eq!(
            opened_org(&Invited::Nobody, SessionOrg::Held(own), true),
            Some(own)
        );
    }

    #[test]
    fn a_user_holding_no_org_opens_a_personal_one() {
        let fresh = org();
        for invited in [Invited::Nobody, Invited::Missed] {
            assert_eq!(
                opened_org(&invited, SessionOrg::Opened(fresh), false),
                Some(fresh)
            );
            assert_eq!(
                opened_org(&invited, SessionOrg::Opened(fresh), true),
                Some(fresh)
            );
        }
    }

    #[test]
    fn a_joined_org_is_the_inviters_to_route() {
        let team = org();
        assert_eq!(
            opened_org(&joined(team), SessionOrg::Held(org()), true),
            None
        );
        assert_eq!(
            opened_org(&joined(team), SessionOrg::Opened(org()), true),
            None
        );
        assert_eq!(opened_org(&joined(team), SessionOrg::Absent, false), None);
    }

    #[test]
    fn an_invited_signup_whose_accept_missed_never_claims_the_org_it_landed_in() {
        assert_eq!(
            opened_org(&Invited::Missed, SessionOrg::Held(org()), true),
            None
        );
    }

    #[test]
    fn an_existing_user_keeps_their_org_and_opens_nothing() {
        assert_eq!(
            opened_org(&Invited::Nobody, SessionOrg::Held(org()), false),
            None
        );
        assert_eq!(
            opened_org(&Invited::Missed, SessionOrg::Held(org()), false),
            None
        );
    }

    #[test]
    fn a_pending_deletion_holding_nothing_opens_nothing() {
        assert_eq!(
            opened_org(&Invited::Nobody, SessionOrg::Absent, false),
            None
        );
    }
}
