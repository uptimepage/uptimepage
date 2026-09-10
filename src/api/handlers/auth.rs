//! `/auth/*` endpoints: GitHub/Google/Microsoft/GitLab OAuth login + callback,
//! logout.
//!
//! Every provider shares one start/finish runner; only Phase B (the upstream
//! identity fetch) dispatches per provider. The callback follows a strict
//! three-phase rule — no DB transaction held across upstream HTTP calls. New
//! users get a signup org auto-created in the same Phase C transaction that
//! links their identity; the resolved default org id is stamped onto the new
//! session row so the next request lands on a real org.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::header::USER_AGENT;
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;
use tower_cookies::Cookies;

use crate::app::AppState;
use crate::auth::{
    OauthProvider, fingerprint, github, gitlab, google,
    login_audit::{self, LoginAttempt, LoginMethod},
    microsoft, oauth_login, oauth_state, session as session_store,
    url::safe_redirect_target,
};
use crate::config::OauthClientConfig;
use crate::domain::{OrgId, UserId};
use crate::error::{AppError, Result};
use crate::observability::metrics::names;
use crate::web::CurrentUser;
use crate::web::auth::Session;

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub redirect_after: Option<String>,
    pub invitation: Option<String>,
}

/// Signing in must never be the thing that cancels a deletion, so the choice
/// gets its own page.
pub const RESTORE_PATH: &str = "/account/restore";

/// Per-provider plumbing the shared runners dispatch on.
struct ProviderParts<'a> {
    cfg: &'a OauthClientConfig,
    method: LoginMethod,
    enabled: bool,
}

fn provider_parts(state: &AppState, provider: OauthProvider) -> ProviderParts<'_> {
    let auth = &state.cfg.auth;
    match provider {
        OauthProvider::Github => ProviderParts {
            cfg: &auth.github,
            method: LoginMethod::GithubOauth,
            enabled: auth.github_login_enabled(),
        },
        OauthProvider::Google => ProviderParts {
            cfg: &auth.google,
            method: LoginMethod::GoogleOauth,
            enabled: auth.google_login_enabled(),
        },
        OauthProvider::Microsoft => ProviderParts {
            cfg: &auth.microsoft.client,
            method: LoginMethod::MicrosoftOauth,
            enabled: auth.microsoft_login_enabled(),
        },
        OauthProvider::Gitlab => ProviderParts {
            cfg: &auth.gitlab.client,
            method: LoginMethod::GitlabOauth,
            enabled: auth.gitlab_login_enabled(),
        },
    }
}

/// 404, not 500 — scanner probes must not pollute the 5xx rate. Logs the
/// listed-but-misconfigured case so a half-set provider doesn't silently
/// look like deliberate policy.
fn unavailable(state: &AppState, provider: OauthProvider) -> AppError {
    let p = provider_parts(state, provider);
    if !p.cfg.is_configured() && state.cfg.auth.method_enabled(p.method.as_db_str()) {
        tracing::warn!(
            provider = provider.as_db_str(),
            "oauth login listed in enabled_methods but client_id/client_secret/redirect_url incomplete"
        );
    }
    AppError::not_found(
        "AUTH_METHOD_UNAVAILABLE",
        "this sign-in method is not enabled",
    )
}

/// Never fails a login: an org with no channel is recoverable, a sign-in that
/// 500s is not.
async fn seed_owner_email_channel(
    state: &AppState,
    org: crate::domain::OrgId,
    user: crate::domain::UserId,
    email: &str,
) {
    // A channel seeded against the log-only sender reads as configured while
    // dropping every alert.
    if !state.cfg.email.delivers() {
        return;
    }
    let seeded = async {
        let limit = i64::from(
            state
                .quotas
                .limit_for_org(org)
                .await?
                .max_notification_channels,
        );
        state
            .notification_channel_store
            .seed_owner_email(org, email, user, limit)
            .await
    }
    .await;
    match seeded {
        Ok(Some(ch)) => {
            tracing::info!(org_id = %org.0, channel_id = %ch.id, "seeded the owner's email alert channel")
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, org_id = %org.0, "seeding the owner's email alert channel failed")
        }
    }
}

pub async fn github_login(state: State<AppState>, q: Query<LoginQuery>) -> Result<Redirect> {
    start_login(state, q, OauthProvider::Github).await
}

pub async fn google_login(state: State<AppState>, q: Query<LoginQuery>) -> Result<Redirect> {
    start_login(state, q, OauthProvider::Google).await
}

pub async fn microsoft_login(state: State<AppState>, q: Query<LoginQuery>) -> Result<Redirect> {
    start_login(state, q, OauthProvider::Microsoft).await
}

pub async fn gitlab_login(state: State<AppState>, q: Query<LoginQuery>) -> Result<Redirect> {
    start_login(state, q, OauthProvider::Gitlab).await
}

async fn start_login(
    State(state): State<AppState>,
    Query(q): Query<LoginQuery>,
    provider: OauthProvider,
) -> Result<Redirect> {
    let pool = state.db.as_ref().ok_or_else(|| {
        AppError::Other(anyhow::anyhow!(
            "oauth login: no Postgres pool — auth requires tenancy mode"
        ))
    })?;
    let parts = provider_parts(&state, provider);
    if !parts.enabled {
        return Err(unavailable(&state, provider));
    }
    let cfg = parts.cfg;
    // Only same-origin paths survive: anything else gets dropped to None so
    // the callback redirects to `/`. Without this, `?redirect_after=https://evil.test`
    // turns a legit OAuth dance into an open-redirect into attacker territory.
    let redirect_after = q.redirect_after.as_deref().and_then(safe_redirect_target);

    // Resolve token → row id at this edge so the raw token is never at rest
    // in `oauth_states`; the id alone isn't replayable (accept still
    // requires the session-bound email to match).
    let invitation_id =
        crate::auth::invitations::resolve_pending_invitation_id(pool, q.invitation.as_deref())
            .await?;

    let s = oauth_state::generate_state();
    oauth_state::insert(
        pool,
        &s,
        provider.as_db_str(),
        oauth_state::StateBinding {
            redirect_after,
            invitation_id,
            ..Default::default()
        },
    )
    .await?;
    let url = match provider {
        OauthProvider::Github => github::authorize_url(cfg, &s),
        OauthProvider::Google => google::authorize_url(cfg, &s),
        // Tenant sits beside the credentials, so this arm needs the whole section.
        OauthProvider::Microsoft => microsoft::authorize_url(&state.cfg.auth.microsoft, &s),
        OauthProvider::Gitlab => gitlab::authorize_url(&state.cfg.auth.gitlab, &s),
    };
    Ok(Redirect::to(&url))
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    /// Absent when the user denies consent (provider sends `error=` instead).
    pub code: Option<String>,
    pub state: String,
    pub error: Option<String>,
}

pub async fn github_callback(
    state: State<AppState>,
    q: Query<CallbackQuery>,
    ip: crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
) -> Result<axum::response::Response> {
    finish_login(state, q, ip, headers, cookies, OauthProvider::Github).await
}

pub async fn google_callback(
    state: State<AppState>,
    q: Query<CallbackQuery>,
    ip: crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
) -> Result<axum::response::Response> {
    finish_login(state, q, ip, headers, cookies, OauthProvider::Google).await
}

pub async fn microsoft_callback(
    state: State<AppState>,
    q: Query<CallbackQuery>,
    ip: crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
) -> Result<axum::response::Response> {
    finish_login(state, q, ip, headers, cookies, OauthProvider::Microsoft).await
}

pub async fn gitlab_callback(
    state: State<AppState>,
    q: Query<CallbackQuery>,
    ip: crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
) -> Result<axum::response::Response> {
    finish_login(state, q, ip, headers, cookies, OauthProvider::Gitlab).await
}

pub const ACCOUNT_PATH: &str = "/settings/account";

#[derive(Debug, Clone, Copy)]
pub enum CredentialChange {
    Linked,
    Unlinked,
}

/// What makes a provider linking itself in on an email match something the
/// owner can act on, and the only signal a removal happened at all.
/// Best-effort: `oauth_identities::record_event` keeps the durable trail.
pub fn notify_credential_change(
    state: &AppState,
    email: &str,
    provider: OauthProvider,
    change: CredentialChange,
) {
    notify_credential_change_labelled(state, email, provider.label(), change);
}

/// The same mail for a credential with no vendor behind it, so a passkey
/// arriving or leaving is announced exactly like a provider would be.
pub fn notify_credential_change_labelled(
    state: &AppState,
    email: &str,
    provider_label: &str,
    change: CredentialChange,
) {
    let account_url = format!(
        "{}{ACCOUNT_PATH}",
        state.cfg.auth.public_base_url.trim_end_matches('/')
    );
    let provider_label = provider_label.to_string();
    let template = match change {
        CredentialChange::Linked => crate::email::EmailTemplate::IdentityLinked {
            provider_label,
            account_url,
        },
        CredentialChange::Unlinked => crate::email::EmailTemplate::IdentityUnlinked {
            provider_label,
            account_url,
        },
    };
    let outgoing = crate::email::TransactionalEmail {
        from: crate::email::EmailAddress::new(
            state.cfg.email.from_address.clone(),
            state.cfg.email.from_name.clone(),
        ),
        to: crate::email::EmailAddress::new(email.to_string(), email.to_string()),
        template,
    };
    let sender = state.email_sender.clone();
    tokio::spawn(async move {
        if let Err(err) = sender.send(outgoing).await {
            tracing::warn!(error = %err, ?change, "sign-in-method email send failed");
        }
    });
}

/// Mints a dance bound to the caller, so the callback attaches the identity to
/// them rather than resolving a user from the provider's email — the only way
/// to add a provider whose address differs from the account's.
async fn start_link(
    State(state): State<AppState>,
    session: Session,
    provider: OauthProvider,
) -> Result<axum::response::Response> {
    let Some(user) = session.user_id() else {
        return Err(AppError::Unauthorized);
    };
    let parts = provider_parts(&state, provider);
    if !parts.enabled {
        return Err(unavailable(&state, provider));
    }
    let pool = state.require_db()?;
    let s = oauth_state::generate_state();
    oauth_state::insert(
        pool,
        &s,
        provider.as_db_str(),
        oauth_state::StateBinding {
            link_user_id: Some(user.0),
            ..Default::default()
        },
    )
    .await?;
    let cfg = parts.cfg;
    let url = match provider {
        OauthProvider::Github => github::authorize_url(cfg, &s),
        OauthProvider::Google => google::authorize_url(cfg, &s),
        OauthProvider::Microsoft => microsoft::authorize_url(&state.cfg.auth.microsoft, &s),
        OauthProvider::Gitlab => gitlab::authorize_url(&state.cfg.auth.gitlab, &s),
    };
    // POST so the CSRF guard covers it, which means the header rides a fetch,
    // which a 302 would not survive. The page navigates itself.
    Ok(axum::Json(serde_json::json!({ "url": url })).into_response())
}

pub async fn github_link(
    state: State<AppState>,
    session: Session,
) -> Result<axum::response::Response> {
    start_link(state, session, OauthProvider::Github).await
}

pub async fn google_link(
    state: State<AppState>,
    session: Session,
) -> Result<axum::response::Response> {
    start_link(state, session, OauthProvider::Google).await
}

pub async fn microsoft_link(
    state: State<AppState>,
    session: Session,
) -> Result<axum::response::Response> {
    start_link(state, session, OauthProvider::Microsoft).await
}

pub async fn gitlab_link(
    state: State<AppState>,
    session: Session,
) -> Result<axum::response::Response> {
    start_link(state, session, OauthProvider::Gitlab).await
}

/// Reached only once the live session has been matched against the state's
/// `link_user_id`. Mints no session: the caller already had one.
async fn finish_link(
    state: &AppState,
    pool: &sqlx::PgPool,
    provider: OauthProvider,
    identity: &crate::auth::oauth_login::RemoteIdentity,
    link_user: UserId,
    cookies: &Cookies,
    from: crate::storage::oauth_identities::RequestOrigin<'_>,
) -> Result<axum::response::Response> {
    let flash = match oauth_login::link_identity_to_user(pool, provider, identity, link_user).await
    {
        Ok(oauth_login::LinkOutcome::Linked) => {
            crate::storage::oauth_identities::record_event(
                pool,
                link_user,
                crate::storage::oauth_identities::CredentialEvent {
                    provider: provider.as_db_str(),
                    provider_user_id: &identity.provider_user_id,
                    action: crate::auth::CredentialAction::Linked,
                    origin: crate::auth::CredentialOrigin::Session,
                    ip_hash: from.ip_hash,
                    user_agent_hash: from.user_agent_hash,
                },
            )
            .await;
            if let Some(email) = account_email(pool, link_user).await {
                notify_credential_change(state, &email, provider, CredentialChange::Linked);
            }
            crate::web::flash::Flash {
                identity_linked: Some(provider),
                ..Default::default()
            }
        }
        // Not an empty flash: `set` skips those, leaving a stale banner from
        // another page to answer for this round trip.
        Ok(oauth_login::LinkOutcome::AlreadyLinked) => crate::web::flash::Flash {
            identity_already_linked: true,
            ..Default::default()
        },
        Err(AppError::BadRequest { code, .. }) if code == oauth_login::IDENTITY_TAKEN => {
            tracing::warn!(
                provider = provider.as_db_str(),
                user_id = %link_user.0,
                "link refused: that provider account already opens a different account"
            );
            metrics::counter!(names::CREDENTIAL_LINK_REFUSED, "reason" => "identity_taken")
                .increment(1);
            crate::web::flash::Flash {
                identity_taken: true,
                ..Default::default()
            }
        }
        Err(e) => return Err(e),
    };
    crate::web::flash::set(
        cookies,
        &flash,
        state.cfg.auth.session.cookie_secure,
        &state.cfg.auth.session.cookie_domain,
    );
    Ok(Redirect::to(ACCOUNT_PATH).into_response())
}

/// Resolved here rather than by the extractor, so an ordinary sign-in does not
/// pay for a lookup on a row [`finish_login`] destroys a moment later.
async fn live_session_user(
    state: &AppState,
    pool: &sqlx::PgPool,
    cookies: &Cookies,
) -> Option<UserId> {
    let raw = cookies
        .get(state.cfg.auth.session.cookie_name.as_str())
        .map(|c| c.value().to_string())
        .filter(|v| !v.is_empty())?;
    let user = match session_store::lookup(pool, &state.cfg.auth.session, &raw).await {
        Ok(session_store::LookupOutcome::Active(row)) => row.user_id,
        _ => return None,
    };
    // Filters tombstones, rather than leaving it to `request_deletion` having
    // dropped their sessions.
    account_email(pool, user).await.map(|_| user)
}

/// Tombstoned accounts excluded: signed out everywhere else, and their address
/// is not ours to write to.
async fn account_email(pool: &sqlx::PgPool, user: UserId) -> Option<String> {
    match sqlx::query_scalar::<_, String>(
        "SELECT email::text FROM users WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(user.0)
    .fetch_optional(pool)
    .await
    {
        Ok(found) => found,
        // The mail is the safety story here, so losing it must leave a line.
        Err(e) => {
            tracing::warn!(error = %e, user_id = %user.0, "account address lookup failed");
            None
        }
    }
}

async fn finish_login(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    provider: OauthProvider,
) -> Result<axum::response::Response> {
    let pool = state
        .db
        .as_ref()
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("oauth callback: no Postgres pool")))?;
    let parts = provider_parts(&state, provider);
    // Policy-gated like start_login — a state minted before the method was
    // switched off must not complete a sign-in for the rest of its TTL.
    if !parts.enabled {
        return Err(unavailable(&state, provider));
    }
    let (cfg, method) = (parts.cfg, parts.method);
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    let ua_value = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let ua_hash = fingerprint::hash_fingerprint(salt, ua_value);

    // Phase A: consume state. Single-use; expired, unknown, or minted for a
    // different dance (e.g. a connect-purpose provider) → 400.
    let consumed = match oauth_state::consume(pool, &q.state).await? {
        Some(c) if c.provider == provider.as_db_str() => c,
        _ => {
            login_audit::record_failure_anon(
                pool,
                method,
                ip_hash.as_deref(),
                ua_hash.as_deref(),
                "invalid_state",
            )
            .await;
            // Routine — back button, stale tab, dance past its TTL. Logged so
            // a "sign-in does nothing" report has something to match against.
            tracing::debug!(
                provider = provider.as_db_str(),
                "oauth callback: state unknown, expired, or minted for another dance"
            );
            return Err(AppError::bad_request(
                "INVALID_STATE",
                "OAuth state is invalid or has expired",
            ));
        }
    };

    // The state alone must not authorise a link: leaked, it would let whoever
    // holds it attach their own provider account to someone else's. Decided
    // before the token exchange, so a state authorising nobody costs no call.
    let link_user = match consumed.link_user_id {
        Some(id) => {
            let id = UserId(id);
            let live = live_session_user(&state, pool, &cookies).await;
            if live != Some(id) {
                // An expired session is routine; somebody else's is not. One
                // counter holding both cannot be alerted on.
                let reason = if live.is_none() {
                    "no_session"
                } else {
                    "other_user"
                };
                tracing::warn!(
                    provider = provider.as_db_str(),
                    link_user_id = %id.0,
                    session_user_id = ?live.map(|u| u.0),
                    reason,
                    "link callback refused: the state names an account the live session is not"
                );
                metrics::counter!(names::CREDENTIAL_LINK_REFUSED, "reason" => reason).increment(1);
                return Ok(crate::web::auth::login_redirect(ACCOUNT_PATH).into_response());
            }
            Some(id)
        }
        None => None,
    };

    // Denied consent / provider error — state already burnt. A link dance was
    // never a sign-in attempt, so it writes no login-failure row.
    let Some(code) = q.code.as_deref().filter(|c| !c.is_empty()) else {
        if link_user.is_some() {
            return Ok(Redirect::to(ACCOUNT_PATH).into_response());
        }
        let reason = if q.error.is_some() {
            "oauth_denied"
        } else {
            "missing_code"
        };
        login_audit::record_failure_anon(
            pool,
            method,
            ip_hash.as_deref(),
            ua_hash.as_deref(),
            reason,
        )
        .await;
        return Ok(Redirect::to("/login").into_response());
    };

    // Phase B: upstream HTTP. NO DB connection held here.
    let fetched = match provider {
        OauthProvider::Github => github::fetch_identity(&state.oauth_http, cfg, code).await,
        OauthProvider::Google => google::fetch_identity(&state.oauth_http, cfg, code).await,
        OauthProvider::Microsoft => {
            microsoft::fetch_identity(&state.oauth_http, &state.cfg.auth.microsoft, code).await
        }
        OauthProvider::Gitlab => {
            gitlab::fetch_identity(&state.oauth_http, &state.cfg.auth.gitlab, code).await
        }
    };
    let identity = match fetched {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(
                error = %err,
                provider = provider.as_db_str(),
                purpose = if link_user.is_some() { "link" } else { "login" },
                "oauth callback: phase B failed"
            );
            // As above: a link dance belongs back on the settings page.
            if link_user.is_some() {
                crate::web::flash::set(
                    &cookies,
                    &crate::web::flash::Flash {
                        link_failed: true,
                        ..Default::default()
                    },
                    state.cfg.auth.session.cookie_secure,
                    &state.cfg.auth.session.cookie_domain,
                );
                return Ok(Redirect::to(ACCOUNT_PATH).into_response());
            }
            login_audit::record_failure_anon(
                pool,
                method,
                ip_hash.as_deref(),
                ua_hash.as_deref(),
                "oauth_upstream_failed",
            )
            .await;
            return Err(AppError::Other(anyhow::anyhow!(
                "oauth callback: phase B: {err}"
            )));
        }
    };

    if let Some(link_user) = link_user {
        return finish_link(
            &state,
            pool,
            provider,
            &identity,
            link_user,
            &cookies,
            crate::storage::oauth_identities::RequestOrigin {
                ip_hash: ip_hash.as_deref(),
                user_agent_hash: ua_hash.as_deref(),
            },
        )
        .await;
    }

    // Phase C: materialise user + identity, auto-create signup org for new
    // users unless the dance carries an invitation, and resolve their
    // default-org id for the session row. Fresh transaction; no upstream calls.
    // Judged here, enforced inside phase C on the brand-new-user branch only:
    // an account that predates its domain landing on a list keeps working.
    let admission = match identity.verified_email.as_deref() {
        Some(email)
            if crate::storage::orgs::find_user_by_email(pool, email)
                .await?
                .is_none() =>
        {
            state.admit_email(email).await
        }
        // Somebody already holds this address, so phase C takes the identity or
        // email-match branch and never looks at a verdict. Asking for one would
        // put a DNS round trip on every returning sign-in.
        Some(_) | None => crate::security::Admission::Clear,
    };
    let signup_org = match consumed.invitation_id {
        Some(_) => oauth_login::SignupOrg::Defer,
        None => oauth_login::SignupOrg::Create,
    };
    let resolved = match oauth_login::upsert_identity_and_signup_org(
        pool, provider, &identity, admission, signup_org,
    )
    .await
    {
        Ok(resolved) => resolved,
        // Provider setup, not a server fault — a 500 here would read as an outage.
        Err(AppError::BadRequest { code, .. }) if code == oauth_login::NO_VERIFIED_EMAIL => {
            tracing::warn!(
                provider = provider.as_db_str(),
                "oauth callback: provider attested no email and no identity matched"
            );
            login_audit::record_failure_anon(
                pool,
                method,
                ip_hash.as_deref(),
                ua_hash.as_deref(),
                "no_verified_email",
            )
            .await;
            return Ok(Redirect::to("/login").into_response());
        }
        // A page, not a JSON body, and an audit row so refusals are countable.
        Err(AppError::BadRequest { code, .. })
            if code == crate::api::error::codes::EMAIL_DESTINATION_BLOCKED =>
        {
            tracing::info!(
                provider = provider.as_db_str(),
                "oauth callback: signup refused, address not usable"
            );
            login_audit::record_failure_anon(
                pool,
                method,
                ip_hash.as_deref(),
                ua_hash.as_deref(),
                "email_destination_blocked",
            )
            .await;
            return Ok(Redirect::to("/login").into_response());
        }
        // A double-clicked sign-in produces this. Trying again works, so send
        // them somewhere they can, not to an internal-error page.
        Err(AppError::BadRequest { code, .. }) if code == oauth_login::IDENTITY_RACED => {
            tracing::info!(
                provider = provider.as_db_str(),
                "oauth callback: another dance claimed this identity first"
            );
            login_audit::record_failure_anon(
                pool,
                method,
                ip_hash.as_deref(),
                ua_hash.as_deref(),
                "identity_raced",
            )
            .await;
            return Ok(Redirect::to("/login").into_response());
        }
        Err(e) => {
            tracing::warn!(error = %e, provider = provider.as_db_str(), "oauth callback: phase C failed");
            return Err(e);
        }
    };
    if let Some(deleted_at) = resolved.pending_deletion {
        tracing::info!(
            user_id = %resolved.user_id.0,
            deleted_at = %deleted_at,
            "sign-in on an account scheduled for deletion; routing to the restore choice"
        );
    }
    // Nobody asked for this in so many words, so the account is told.
    if resolved.is_new_user {
        crate::storage::oauth_identities::record_event(
            pool,
            resolved.user_id,
            crate::storage::oauth_identities::CredentialEvent {
                provider: provider.as_db_str(),
                provider_user_id: &identity.provider_user_id,
                action: crate::auth::CredentialAction::Linked,
                origin: crate::auth::CredentialOrigin::Signup,
                ip_hash: ip_hash.as_deref(),
                user_agent_hash: ua_hash.as_deref(),
            },
        )
        .await;
    }

    if resolved.newly_linked {
        crate::storage::oauth_identities::record_event(
            pool,
            resolved.user_id,
            crate::storage::oauth_identities::CredentialEvent {
                provider: provider.as_db_str(),
                provider_user_id: &identity.provider_user_id,
                action: crate::auth::CredentialAction::Linked,
                origin: crate::auth::CredentialOrigin::EmailMatch,
                ip_hash: ip_hash.as_deref(),
                user_agent_hash: ua_hash.as_deref(),
            },
        )
        .await;
        // `account_email` filters tombstones: an account inside its deletion
        // grace window is signed out everywhere else, and its address is not
        // ours to write to.
        if let Some(email) = account_email(pool, resolved.user_id).await {
            notify_credential_change(&state, &email, provider, CredentialChange::Linked);
        }
    }

    // The dance carried an invitation — redeem it now, server-side; the
    // session then opens directly in the joined org. A user left with no org
    // at all gets a personal one here: an invitee whose invitation died before
    // the accept still holds a proven identity, so the sign-in stands.
    let joined = match consumed.invitation_id {
        Some(id) => {
            crate::api::handlers::invitations::try_auto_accept(&state, resolved.user_id, id).await
        }
        None => None,
    };
    let (active_org, created_org) = match landing(joined.as_ref().map(|j| j.org_id), &resolved) {
        Landing::Org { org, fresh } => (Some(org), fresh.then_some(org)),
        Landing::NoOrg => (None, None),
        Landing::OpenPersonalOrg => {
            match crate::storage::users::ensure_signup_org(pool, resolved.user_id).await {
                Ok((org, created)) => (Some(org), created.then_some(org)),
                Err(e) => {
                    tracing::warn!(error = %e, user_id = %resolved.user_id.0, "oauth callback: opening a personal org failed");
                    if let Err(err) = login_audit::record(
                        pool,
                        method,
                        LoginAttempt {
                            user_id: Some(resolved.user_id),
                            success: false,
                            ip_hash: ip_hash.as_deref(),
                            user_agent_hash: ua_hash.as_deref(),
                            failure_reason: Some("signup_org_failed"),
                        },
                    )
                    .await
                    {
                        tracing::warn!(error = %err, "login_audit write failed (non-fatal)");
                    }
                    return Err(e);
                }
            }
        }
    };
    if let Some(org) = created_org
        && let Some(email) = account_email(pool, resolved.user_id).await
    {
        seed_owner_email_channel(&state, org, resolved.user_id, &email).await;
    }

    // Session fixation: drop any pre-login session bound to this browser
    // before minting the new one. Without this an attacker who pre-seeded a
    // cookie inherits the just-authenticated session.
    let cookie_name = state.cfg.auth.session.cookie_name.as_str();
    if let Some(prev) = cookies.get(cookie_name).map(|c| c.value().to_string())
        && !prev.is_empty()
        && let Err(err) = session_store::destroy(pool, &prev).await
    {
        tracing::warn!(error = %err, "session fixation: pre-login destroy failed");
    }

    let created = session_store::create(
        pool,
        &state.cfg.auth.session,
        resolved.user_id,
        active_org,
        ip_hash.as_deref(),
        ua_hash.as_deref(),
    )
    .await?;

    // Audit post-commit: a failure here logs but the session is already valid.
    if let Err(err) = login_audit::record(
        pool,
        method,
        LoginAttempt {
            user_id: Some(resolved.user_id),
            success: true,
            ip_hash: ip_hash.as_deref(),
            user_agent_hash: ua_hash.as_deref(),
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
    // all. Ids only — the address belongs in neither logs nor metrics.
    tracing::info!(
        user_id = %resolved.user_id.0,
        provider = provider.as_db_str(),
        new_user = resolved.is_new_user,
        org_id = ?active_org.map(|o| o.0),
        "sign-in complete"
    );

    crate::analytics::track_login(
        &state,
        crate::analytics::Login {
            method,
            new_user: resolved.is_new_user,
            redirect_after: consumed.redirect_after.as_deref(),
            via: None,
        },
        client_ip,
        &headers,
    );

    cookies.add(session_store::build_cookie(
        &state.cfg.auth.session,
        created.cookie_token,
    ));
    crate::web::login_hint::set(&cookies, &state.cfg.auth.session, method.as_db_str());
    if let Err(err) =
        crate::web::display_prefs::issue_cookies(&state, &cookies, resolved.user_id).await
    {
        tracing::warn!(error = %err, "display-preference cookie issue failed (non-fatal)");
    }

    // One-shot banners ride a flash cookie (unspoofable, fires once); only the
    // slug-validated `joined` stays a query param.
    let invite_missed = joined.is_none() && consumed.invitation_id.is_some();
    crate::web::flash::set(
        &cookies,
        &crate::web::flash::Flash {
            restored: false,
            invite_missed,
            ..Default::default()
        },
        state.cfg.auth.session.cookie_secure,
        &state.cfg.auth.session.cookie_domain,
    );
    // Outranks every other target: the session only proves who is asking.
    let redirect = if resolved.pending_deletion.is_some() {
        RESTORE_PATH.to_string()
    } else if let Some(j) = joined {
        format!("/?joined={}", crate::auth::url::url_encode(&j.org_slug))
    } else if invite_missed {
        "/".to_string()
    } else {
        consumed
            .redirect_after
            .as_deref()
            .and_then(safe_redirect_target)
            .map(str::to_string)
            .unwrap_or_else(|| "/".to_string())
    };
    Ok(Redirect::to(&redirect).into_response())
}

/// Where a finished sign-in opens, read off what phase C and the invitation
/// accept left behind.
#[derive(Debug, PartialEq, Eq)]
enum Landing {
    /// `fresh` when phase C opened this org during the same sign-in.
    Org { org: OrgId, fresh: bool },
    /// No live membership anywhere: open a personal org before the session.
    OpenPersonalOrg,
    /// Deletion pending and nothing held; the restore choice needs no org.
    NoOrg,
}

fn landing(joined: Option<OrgId>, resolved: &oauth_login::ResolvedIdentity) -> Landing {
    match joined.or(resolved.signup_org_id) {
        Some(org) => Landing::Org {
            org,
            fresh: joined.is_none() && resolved.is_new_user,
        },
        None if resolved.pending_deletion.is_some() => Landing::NoOrg,
        None => Landing::OpenPersonalOrg,
    }
}

pub async fn logout(
    State(state): State<AppState>,
    cookies: Cookies,
) -> Result<axum::response::Response> {
    let cookie_name = state.cfg.auth.session.cookie_name.as_str();
    if let Some(c) = cookies.get(cookie_name)
        && let Some(pool) = state.db.as_ref()
    {
        let id = c.value().to_string();
        if let Err(err) = session_store::destroy(pool, &id).await {
            // Surface failures: silently dropping them leaves the DB row alive
            // while the browser thinks it's logged out — an attacker holding
            // the cookie value (from a log file, proxy, etc.) could still use
            // it on the next request.
            tracing::warn!(error = %err, "logout: session destroy failed");
        }
    }
    cookies.add(session_store::clear_cookie(&state.cfg.auth.session));
    Ok(Redirect::to("/login").into_response())
}

pub async fn logout_all(
    State(state): State<AppState>,
    cookies: Cookies,
    CurrentUser(user_id): CurrentUser,
) -> Result<axum::response::Response> {
    if let Some(pool) = state.db.as_ref() {
        session_store::destroy_all_for_user(pool, user_id).await?;
    }
    cookies.add(session_store::clear_cookie(&state.cfg.auth.session));
    Ok(Redirect::to("/login").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn resolved(
        signup_org_id: Option<OrgId>,
        is_new_user: bool,
        pending_deletion: bool,
    ) -> oauth_login::ResolvedIdentity {
        oauth_login::ResolvedIdentity {
            user_id: UserId(uuid::Uuid::now_v7()),
            signup_org_id,
            is_new_user,
            pending_deletion: pending_deletion.then(Utc::now),
            newly_linked: false,
        }
    }

    fn org() -> OrgId {
        OrgId(uuid::Uuid::now_v7())
    }

    #[test]
    fn a_plain_signup_opens_in_the_org_phase_c_created() {
        let personal = org();
        assert_eq!(
            landing(None, &resolved(Some(personal), true, false)),
            Landing::Org {
                org: personal,
                fresh: true
            }
        );
    }

    #[test]
    fn an_accepted_invitation_opens_in_the_joined_org_without_a_fresh_one() {
        let team = org();
        assert_eq!(
            landing(Some(team), &resolved(None, true, false)),
            Landing::Org {
                org: team,
                fresh: false
            }
        );
        assert_eq!(
            landing(Some(team), &resolved(Some(org()), false, false)),
            Landing::Org {
                org: team,
                fresh: false
            }
        );
    }

    #[test]
    fn a_user_holding_no_org_gets_a_personal_one() {
        assert_eq!(
            landing(None, &resolved(None, true, false)),
            Landing::OpenPersonalOrg
        );
        assert_eq!(
            landing(None, &resolved(None, false, false)),
            Landing::OpenPersonalOrg
        );
    }

    #[test]
    fn an_existing_user_keeps_their_org_and_seeds_nothing() {
        let own = org();
        assert_eq!(
            landing(None, &resolved(Some(own), false, false)),
            Landing::Org {
                org: own,
                fresh: false
            }
        );
    }

    #[test]
    fn a_pending_deletion_never_opens_a_new_org() {
        let own = org();
        assert_eq!(
            landing(None, &resolved(Some(own), false, true)),
            Landing::Org {
                org: own,
                fresh: false
            }
        );
        assert_eq!(landing(None, &resolved(None, false, true)), Landing::NoOrg);
    }
}
