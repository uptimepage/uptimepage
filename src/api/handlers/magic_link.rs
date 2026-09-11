//! Magic-link sign-in endpoints. These are mounted only when
//! `auth.enabled_methods` contains `"magic_link"`; otherwise the router never
//! exposes them and the surface is a 404.
//!
//! - `POST /auth/magic-link/request {email}` — anti-enumeration: the response
//!   shape is identical for every input, and the handler does the same
//!   argon2+INSERT work for every well-formed address regardless of whether a
//!   user owns it. Malformed inputs short-circuit (they disclose only that the
//!   string isn't an email shape — no signal about specific accounts). Email
//!   delivery and the per-email send throttle run inside `tokio::spawn` so
//!   neither SMTP latency nor rate-limit state leaks via response time. The
//!   throttle keeps a recipient's inbox to one mail per
//!   `auth.magic_link.rate_limit_seconds` regardless of source.
//! - `GET  /auth/magic-link/verify?token=…` — peeks the token and renders a
//!   one-click confirmation carrying a double-submit nonce, so a mail
//!   scanner's prefetch cannot burn it.
//! - `POST /auth/magic-link/verify` — consumes the token, destroys any
//!   pre-login session bound to the browser (fixation defence), mints a fresh
//!   session cookie, and redirects: `/account/restore` for an account
//!   scheduled for deletion, else the joined org, else `/`.
//! - `POST /auth/magic-link/code` — the same redemption proved by the code
//!   printed in that mail, and only from the browser that asked for it.

use crate::api::json::Json;
use anyhow::Context;
use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Form, Query, State};
use axum::http::header::{CACHE_CONTROL, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration as CookieDuration;
use tower_cookies::{Cookie, Cookies};

use crate::app::AppState;
use crate::auth::email_norm;
use crate::auth::login_audit::{self, LoginAttempt, LoginMethod};
use crate::auth::url::token_link;
use crate::auth::{fingerprint, magic_link, session as session_store, token_hash};
use crate::config::SessionConfig;
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::error::Result;
use crate::storage::orgs as orgs_store;
use crate::web::filters;

/// Name of the double-submit nonce cookie that ties the confirm-page GET to the
/// sign-in POST. Set on the GET, echoed in a hidden form field, checked on the
/// POST. A cross-site forged POST can neither read nor set it, so it can't
/// forge a login. Path-scoped to the verify endpoint; nothing else needs it.
const CONFIRM_NONCE_COOKIE: &str = "_sm_ml_confirm";
/// Binds an emailed code to the browser that asked for it.
const CODE_NONCE_COOKIE: &str = "_sm_ml_code";

/// Covers `/request` as well as `/code`: the request handler reuses the nonce
/// it finds here, and RFC 6265 would not send a cookie scoped to `/code` there.
const CODE_COOKIE_PATH: &str = "/auth/magic-link";
const CONFIRM_COOKIE_PATH: &str = "/auth/magic-link/verify";

#[derive(Debug, Deserialize)]
pub struct RequestBody {
    pub email: String,
    #[serde(default)]
    pub redirect_after: Option<String>,
    /// Raw invitation token; resolved to its row id like the OAuth starts.
    #[serde(default)]
    pub invitation: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct RequestResponse {
    /// Always `true`. The shape never branches on whether a user exists so
    /// the response is indistinguishable for enumeration probes.
    pub sent: bool,
}

/// `POST /auth/magic-link/request`. Anti-enumeration: returns the same shape
/// for every input and performs the same DB work for every well-formed email
/// regardless of whether a user owns it. Mail send is spawned so SMTP latency
/// doesn't make the response observable.
pub async fn request(
    State(state): State<AppState>,
    cookies: Cookies,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    Json(body): Json<RequestBody>,
) -> Result<Json<RequestResponse>> {
    let pool = state.require_db()?;
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    // Behind a declared proxy an internal address means the chain lost the client.
    let ip_hint = crate::web::client_ip::reportable(client_ip, &state.cfg.security.trusted_proxies);
    if ip_hint.is_none() {
        tracing::warn!(
            %client_ip,
            "magic_link: client ip is internal, check security.trusted_proxies and IPv6 ingress"
        );
    }

    if let Some(email) = email_norm::normalize(&body.email) {
        let cfg = &state.cfg.auth.magic_link;
        // Reminting binds the browser to a row whose mail the throttle may
        // suppress, and the code already in the reader's inbox stops matching.
        let nonce = cookies
            .get(CODE_NONCE_COOKIE)
            .map(|c| c.value().to_string())
            .unwrap_or_else(token_hash::generate_raw_token);
        set_code_nonce(
            &cookies,
            &state.cfg.auth.session,
            &nonce,
            cfg.expiry_minutes,
        );
        let redirect_after = body
            .redirect_after
            .as_deref()
            .and_then(crate::auth::url::safe_redirect_target);
        let invitation_id = crate::auth::invitations::resolve_pending_invitation_id(
            pool,
            body.invitation.as_deref(),
        )
        .await?;
        // Always insert — even for unknown emails — so timing can't
        // distinguish them; a no-user row never redeems at /verify.
        let created = magic_link::create(
            pool,
            magic_link::NewMagicLink {
                email,
                ip_hash: ip_hash.as_deref(),
                expiry_minutes: cfg.expiry_minutes,
                redirect_after,
                invitation_id,
                nonce: Some(&nonce),
            },
        )
        .await?;
        let verify_url = token_link(
            &state.cfg.auth.public_base_url,
            "/auth/magic-link/verify",
            &created.token,
        );

        let outgoing = TransactionalEmail {
            from: EmailAddress::new(
                state.cfg.email.from_address.clone(),
                state.cfg.email.from_name.clone(),
            ),
            to: EmailAddress::new(email.to_string(), email.to_string()),
            template: EmailTemplate::MagicLink {
                url: verify_url,
                code: created.code.clone(),
                expires_in_minutes: cfg.expiry_minutes,
                ip_hint: ip_hint.map(|ip| ip.to_string()),
                opens_accounts: state.cfg.auth.open_signup_enabled(),
            },
        };
        let sender = state.email_sender.clone();
        let throttle_pool = pool.clone();
        let throttle_email = email.to_string();
        let created_id = created.row.id;
        let rate_limit = cfg.rate_limit_seconds;
        let policy = state.email_policy.clone();
        let refuses_listed =
            state.cfg.email_policy.signup_policy() == Some(crate::config::SignupPolicy::Block);
        // Spawn so SMTP duration doesn't leak via response time and so the
        // per-email throttle below also runs off the response path. The
        // handler returns the anti-enum response immediately.
        tokio::spawn(async move {
            // Only under `block`, where /verify would refuse this address
            // anyway. Under `flag` the mail must go out or the account could
            // never open to be marked. The row is written either way, so timing
            // cannot tell a listed domain from an unknown one.
            if refuses_listed
                && let Some(risk) = policy
                    .disposable_domain(&throttle_email)
                    .map(|_| crate::security::EmailRisk::Disposable)
            {
                crate::security::email_policy::record("magic_link_send", "refused", risk);
                tracing::info!("magic_link: listed domain refused by policy, suppressing email");
                return;
            }
            // Fail open on DB error: a transient hiccup must not swallow a
            // legitimate sign-in attempt.
            match magic_link::claim_send(&throttle_pool, created_id, &throttle_email, rate_limit)
                .await
            {
                Ok(false) => {
                    tracing::info!(
                        window_seconds = rate_limit,
                        "magic_link: rate-limited, suppressing email"
                    );
                    return;
                }
                Ok(true) => {}
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "magic_link: throttle claim failed; sending anyway"
                    );
                    // Stamp anyway: an unstamped row mails a code that cannot
                    // be redeemed, and the refusal would blame the reader.
                    if let Err(err) =
                        magic_link::claim_send(&throttle_pool, created_id, &throttle_email, 0).await
                    {
                        tracing::warn!(error = %err, "magic_link: send stamp failed");
                    }
                }
            }
            if let Err(err) = sender.send(outgoing).await {
                tracing::warn!(error = %err, "magic_link: email send failed (background)");
                if let Err(err) = magic_link::release_send(&throttle_pool, created_id).await {
                    tracing::warn!(error = %err, "magic_link: send release failed");
                }
                return;
            }
            match magic_link::supersede_others(&throttle_pool, &throttle_email, created_id).await {
                Ok(n) if n > 0 => tracing::info!(retired = n, "magic_link: earlier links retired"),
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, "magic_link: supersede failed"),
            }
        });
    }

    Ok(Json(RequestResponse { sent: true }))
}

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    pub token: String,
}

/// POST body of the confirmation form rendered by [`verify_landing`]. `token`
/// is the raw magic-link token echoed from the GET; `csrf` is the double-submit
/// nonce that must equal the [`CONFIRM_NONCE_COOKIE`] value.
#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    pub token: String,
    #[serde(default)]
    pub csrf: String,
}

/// Verify responses that aren't a redirect render a page, not JSON.
#[derive(Template, WebTemplate)]
#[template(path = "auth/magic_link_invalid.html")]
struct MagicLinkInvalidPage {
    expiry_minutes: u32,
    analytics: Option<&'static str>,
}

/// Separate from the dead-link page: the link in that mail is still live.
#[derive(Template, WebTemplate)]
#[template(path = "auth/magic_link_code_refused.html")]
struct MagicLinkCodeRefusedPage {
    analytics: Option<&'static str>,
    /// False when the input was never a code, so the reader still holds theirs.
    spent: bool,
}

/// The one-click confirmation served on GET so a mail link-scanner's prefetch
/// can't burn the token. The token round-trips through a hidden field; the
/// human's button press is the POST that signs in.
#[derive(Template, WebTemplate)]
#[template(path = "auth/magic_link_confirm.html")]
struct MagicLinkConfirmPage {
    email: String,
    token: String,
    csrf: String,
    analytics: Option<&'static str>,
}

/// Failed redemptions render the indistinguishable invalid page. Status is
/// caller-chosen: `GONE` for a dead/used/expired token (never cacheable as
/// success, stays visible in status-code dashboards), `FORBIDDEN` for a
/// missing or mismatched confirmation nonce.
fn invalid_page(state: &AppState, status: StatusCode) -> Response {
    (
        status,
        MagicLinkInvalidPage {
            expiry_minutes: state.cfg.auth.magic_link.expiry_minutes,
            analytics: crate::analytics::website_id(&state.cfg.auth.public_base_url),
        },
    )
        .into_response()
}

/// `SameSite=Lax` is why both paths are exempt from the CSRF header check.
/// The extra five minutes keep a live cookie behind a still-valid token.
fn nonce_cookie(
    name: &'static str,
    path: &'static str,
    cfg: &SessionConfig,
    nonce: &str,
    token_ttl_minutes: u32,
) -> Cookie<'static> {
    let mut c = Cookie::new(name, nonce.to_string());
    c.set_http_only(true);
    c.set_secure(cfg.cookie_secure);
    c.set_same_site(SameSite::Lax);
    c.set_path(path);
    if !cfg.cookie_domain.is_empty() {
        c.set_domain(cfg.cookie_domain.clone());
    }
    c.set_max_age(CookieDuration::minutes(i64::from(token_ttl_minutes) + 5));
    c
}

fn set_code_nonce(cookies: &Cookies, cfg: &SessionConfig, nonce: &str, token_ttl_minutes: u32) {
    cookies.add(nonce_cookie(
        CODE_NONCE_COOKIE,
        CODE_COOKIE_PATH,
        cfg,
        nonce,
        token_ttl_minutes,
    ));
}

fn set_confirm_nonce(cookies: &Cookies, cfg: &SessionConfig, nonce: &str, token_ttl_minutes: u32) {
    cookies.add(nonce_cookie(
        CONFIRM_NONCE_COOKIE,
        CONFIRM_COOKIE_PATH,
        cfg,
        nonce,
        token_ttl_minutes,
    ));
}

/// Constant-time double-submit check: the posted `csrf` must be non-empty and
/// equal the nonce cookie the GET set on this browser.
fn confirm_nonce_ok(cookies: &Cookies, posted: &str) -> bool {
    let Some(cookie) = cookies.get(CONFIRM_NONCE_COOKIE) else {
        return false;
    };
    !posted.is_empty() && cookie.value().as_bytes().ct_eq(posted.as_bytes()).into()
}

fn clear_nonce(cookies: &Cookies, cfg: &SessionConfig, name: &'static str, path: &'static str) {
    let mut gone = Cookie::new(name, "");
    gone.set_path(path);
    if !cfg.cookie_domain.is_empty() {
        gone.set_domain(cfg.cookie_domain.clone());
    }
    cookies.remove(gone);
}

/// `GET /auth/magic-link/verify`. Read-only: peeks the token without consuming
/// it and, for a live token, renders a one-click confirmation page carrying a
/// fresh double-submit nonce. A mail scanner's prefetch lands here, leaves the
/// token intact, and the recipient's button press completes sign-in via POST.
pub async fn verify_landing(
    State(state): State<AppState>,
    Query(q): Query<VerifyQuery>,
    cookies: Cookies,
) -> Result<Response> {
    let pool = state.require_db()?;
    let Some(row) = magic_link::peek(pool, q.token.trim()).await? else {
        return Ok(invalid_page(&state, StatusCode::GONE));
    };
    let nonce = token_hash::generate_raw_token();
    set_confirm_nonce(
        &cookies,
        &state.cfg.auth.session,
        &nonce,
        state.cfg.auth.magic_link.expiry_minutes,
    );
    let mut resp = MagicLinkConfirmPage {
        email: row.email,
        token: q.token.trim().to_string(),
        csrf: nonce,
        analytics: crate::analytics::website_id(&state.cfg.auth.public_base_url),
    }
    .into_response();
    // Token-bearing page: never let an intermediary cache it.
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

/// `POST /auth/magic-link/verify`. Validates the double-submit nonce, then
/// atomically consumes the token, destroys any pre-login session bound to the
/// browser (fixation defence), creates a fresh session, and redirects by the
/// same priority as the OAuth callback.
pub async fn verify_confirm(
    State(state): State<AppState>,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    Form(form): Form<ConfirmForm>,
) -> Result<Response> {
    let pool = state.require_db()?;
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    let ua_hash = fingerprint::hash_fingerprint(
        salt,
        headers
            .get(USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
    );

    if !confirm_nonce_ok(&cookies, &form.csrf) {
        login_audit::record_failure_anon(
            pool,
            LoginMethod::MagicLink,
            ip_hash.as_deref(),
            ua_hash.as_deref(),
            "confirm_csrf",
        )
        .await;
        return Ok(invalid_page(&state, StatusCode::FORBIDDEN));
    }

    let Some(row) = magic_link::consume(pool, form.token.trim()).await? else {
        login_audit::record_failure_anon(
            pool,
            LoginMethod::MagicLink,
            ip_hash.as_deref(),
            ua_hash.as_deref(),
            "invalid_token",
        )
        .await;
        return Ok(invalid_page(&state, StatusCode::GONE));
    };
    clear_nonce(
        &cookies,
        &state.cfg.auth.session,
        CONFIRM_NONCE_COOKIE,
        CONFIRM_COOKIE_PATH,
    );
    open_session_for(
        &state,
        &cookies,
        row,
        magic_link::RedeemedVia::Link,
        Requester {
            client_ip,
            headers: &headers,
            ip_hash,
            ua_hash,
        },
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct CodeForm {
    pub code: String,
}

/// Redeems the code printed beside the link, only from the browser that
/// asked for it.
pub async fn submit_code(
    State(state): State<AppState>,
    crate::web::client_ip::ClientIp(client_ip): crate::web::client_ip::ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    Form(form): Form<CodeForm>,
) -> Result<Response> {
    let pool = state.require_db()?;
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    let ip_hash = fingerprint::hash_fingerprint(salt, &client_ip.to_string());
    let ua_hash = fingerprint::hash_fingerprint(
        salt,
        headers
            .get(USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
    );

    // Not the dead-link page: the link in that mail may still sign them in.
    let Some(nonce) = cookies
        .get(CODE_NONCE_COOKIE)
        .map(|c| c.value().to_string())
    else {
        return Ok(code_refused_page(&state, true));
    };
    match magic_link::consume_code(pool, &nonce, &form.code).await? {
        magic_link::CodeOutcome::Ok(row) => {
            clear_nonce(
                &cookies,
                &state.cfg.auth.session,
                CODE_NONCE_COOKIE,
                CODE_COOKIE_PATH,
            );
            open_session_for(
                &state,
                &cookies,
                row,
                magic_link::RedeemedVia::Code,
                Requester {
                    client_ip,
                    headers: &headers,
                    ip_hash,
                    ua_hash,
                },
            )
            .await
        }
        magic_link::CodeOutcome::Refused => {
            login_audit::record_failure_anon(
                pool,
                LoginMethod::MagicLink,
                ip_hash.as_deref(),
                ua_hash.as_deref(),
                "code_refused",
            )
            .await;
            // Spent whatever refused it.
            clear_nonce(
                &cookies,
                &state.cfg.auth.session,
                CODE_NONCE_COOKIE,
                CODE_COOKIE_PATH,
            );
            Ok(code_refused_page(&state, true))
        }
        // The binding survives, because the attempt behind it does.
        magic_link::CodeOutcome::Malformed => Ok(code_refused_page(&state, false)),
    }
}

fn code_refused_page(state: &AppState, spent: bool) -> Response {
    let mut resp = MagicLinkCodeRefusedPage {
        analytics: crate::analytics::website_id(&state.cfg.auth.public_base_url),
        spent,
    }
    .into_response();
    *resp.status_mut() = StatusCode::GONE;
    resp
}

/// Everything the redemption learns from the request itself.
struct Requester<'a> {
    client_ip: std::net::IpAddr,
    headers: &'a HeaderMap,
    ip_hash: Option<String>,
    ua_hash: Option<String>,
}

async fn open_session_for(
    state: &AppState,
    cookies: &Cookies,
    row: magic_link::MagicLinkRow,
    via: magic_link::RedeemedVia,
    from: Requester<'_>,
) -> Result<Response> {
    let Requester {
        client_ip,
        headers,
        ip_hash,
        ua_hash,
    } = from;
    let pool = state.require_db()?;

    // Tombstone-inclusive: a verified link proves email ownership. Resolving
    // the row is not restoring it — same as the OAuth callbacks.
    let (user_id, pending_deletion, bootstrapped) =
        match orgs_store::find_user_by_email_including_deleted(pool, &row.email).await? {
            Some((user_id, deleted_at)) => {
                if let Some(deleted_at) = deleted_at {
                    tracing::info!(
                        user_id = %user_id.0,
                        deleted_at = %deleted_at,
                        "magic-link sign-in on an account scheduled for deletion; routing to the restore choice"
                    );
                }
                (user_id, deleted_at.is_some(), Bootstrap::Existing)
            }
            None => match bootstrap_unknown_email(state, &row).await? {
                Some((user_id, created)) => (user_id, false, created),
                None => {
                    login_audit::record_failure_anon(
                        pool,
                        LoginMethod::MagicLink,
                        ip_hash.as_deref(),
                        ua_hash.as_deref(),
                        "no_user_for_email",
                    )
                    .await;
                    return Ok(invalid_page(state, StatusCode::GONE));
                }
            },
        };

    // Redeem a carried invitation before the session is minted so the
    // session opens in the joined org. Soft-fails — login never breaks.
    let joined = match row.invitation_id {
        Some(id) => crate::api::handlers::invitations::try_auto_accept(state, user_id, id).await,
        None => None,
    };
    if bootstrapped == Bootstrap::Invited && joined.is_none() {
        // The freshly minted user exists ONLY to redeem this invitation; a
        // raced revoke between pre-flight and accept must not leave an
        // org-less orphan account. No FK children yet — plain DELETE.
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id.0)
            .execute(pool)
            .await
            .context("magic-link bootstrap: compensating user delete")?;
        tracing::warn!(user_id = %user_id.0, "invited bootstrap raced a revoke/seat; user row compensated");
        login_audit::record_failure_anon(
            pool,
            LoginMethod::MagicLink,
            ip_hash.as_deref(),
            ua_hash.as_deref(),
            "invited_bootstrap_raced",
        )
        .await;
        return Ok(invalid_page(state, StatusCode::GONE));
    }
    let active_org = match joined.as_ref().map(|j| j.org_id) {
        Some(org) => Some(org),
        None => crate::storage::users::session_org(pool, user_id, pending_deletion).await?,
    };

    let cookie_name = state.cfg.auth.session.cookie_name.as_str();
    if let Some(prev) = cookies.get(cookie_name).map(|c| c.value().to_string())
        && !prev.is_empty()
        && let Err(err) = session_store::destroy(pool, &prev).await
    {
        tracing::warn!(error = %err, "magic_link: pre-login session destroy failed");
    }

    let created = session_store::create(
        pool,
        &state.cfg.auth.session,
        user_id,
        active_org,
        ip_hash.as_deref(),
        ua_hash.as_deref(),
    )
    .await?;

    if let Err(err) = login_audit::record(
        pool,
        LoginMethod::MagicLink,
        LoginAttempt {
            user_id: Some(user_id),
            success: true,
            ip_hash: ip_hash.as_deref(),
            user_agent_hash: ua_hash.as_deref(),
            failure_reason: None,
        },
    )
    .await
    {
        tracing::warn!(error = %err, "magic_link audit write failed (non-fatal)");
    }

    crate::analytics::track_login(
        state,
        crate::analytics::Login {
            method: LoginMethod::MagicLink,
            new_user: bootstrapped != Bootstrap::Existing,
            redirect_after: row.redirect_after.as_deref(),
            via: Some(via),
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
        LoginMethod::MagicLink.as_db_str(),
    );
    if let Err(err) = crate::web::display_prefs::issue_cookies(state, cookies, user_id).await {
        tracing::warn!(error = %err, "display-preference cookie issue failed (non-fatal)");
    }
    // One-shot banners ride a flash cookie (unspoofable, fires once); only the
    // slug-validated `joined` stays a query param. Same priority as the OAuth
    // callback.
    let invite_missed = joined.is_none() && row.invitation_id.is_some();
    crate::web::flash::set(
        cookies,
        &crate::web::flash::Flash {
            restored: false,
            invite_missed,
            ..Default::default()
        },
        state.cfg.auth.session.cookie_secure,
        &state.cfg.auth.session.cookie_domain,
    );
    let redirect = if pending_deletion {
        crate::api::handlers::auth::RESTORE_PATH.to_string()
    } else if let Some(j) = joined {
        format!("/?joined={}", crate::auth::url::url_encode(&j.org_slug))
    } else if invite_missed {
        "/".to_string()
    } else {
        row.redirect_after
            .as_deref()
            .and_then(crate::auth::url::safe_redirect_target)
            .map(str::to_string)
            .unwrap_or_else(|| "/".to_string())
    };
    Ok(Redirect::to(&redirect).into_response())
}

/// B2 bootstrap gate. Returns the new user id only when the token row carries
/// a pending invitation whose address equals the token's email and whose org
/// can still take a member — checked BEFORE the user INSERT so failures
/// leave no orphan row. Every refusal collapses to `None` (anti-enum).
async fn bootstrap_invited_user(
    state: &AppState,
    row: &magic_link::MagicLinkRow,
) -> Result<Option<(crate::domain::UserId, Bootstrap)>> {
    let pool = state.require_db()?;
    let Some(invitation_id) = row.invitation_id else {
        return Ok(None);
    };
    let Some(invitation) =
        crate::auth::invitations::find_pending_by_id(pool, invitation_id).await?
    else {
        return Ok(None);
    };
    if !invitation.email.eq_ignore_ascii_case(&row.email) {
        return Ok(None);
    }
    if let Err(err) =
        crate::api::handlers::invitations::validate_acceptable(state, &invitation, None).await
    {
        tracing::warn!(error = %err, %invitation_id, "invited bootstrap pre-flight failed");
        return Ok(None);
    }
    // Stamped, never refused: a member vouched, and the click already proved
    // the address takes mail. The mark is only for later churn analysis.
    let risk = state.listed_disposable(&row.email);
    if let Some(risk) = risk {
        crate::security::email_policy::record("invite_accept", "flagged", risk);
    }
    let (user_id, created) =
        crate::storage::users::create_invited_user(pool, &row.email, risk).await?;
    if created {
        tracing::info!(user_id = %user_id.0, via = "invitation", "magic-link bootstrap created account");
    }
    // A row this call did not create must not be compensated away.
    let how = if created {
        Bootstrap::Invited
    } else {
        Bootstrap::Existing
    };
    Ok(Some((user_id, how)))
}

/// Only one of these can be rolled back.
#[derive(Clone, Copy, PartialEq)]
enum Bootstrap {
    Existing,
    /// Minted to redeem an invitation; rolled back if that fails.
    Invited,
    /// Opened its own org under `auth.open_signup`.
    Founded,
}

/// An invitation works whatever the signup policy says. Anything else is
/// `auth.open_signup`.
async fn bootstrap_unknown_email(
    state: &AppState,
    row: &magic_link::MagicLinkRow,
) -> Result<Option<(crate::domain::UserId, Bootstrap)>> {
    // Founding a different org would strand the invitation and its seat.
    if row.invitation_id.is_some() {
        return bootstrap_invited_user(state, row).await;
    }
    if !state.cfg.auth.open_signup_enabled() {
        return Ok(None);
    }
    // The one place open signup mints an account, so the one place a verdict
    // can be spent without touching anybody who already has one. A refusal
    // returns `None`, so the caller renders the same invalid page as any other
    // failed redemption rather than naming which addresses it will take.
    let risk = match state
        .admit_email(&row.email)
        .await
        .allow_new("email", "signup_magic_link")
    {
        Ok(risk) => risk,
        Err(err) => {
            tracing::info!(error = %err, "magic-link signup refused, address not usable");
            return Ok(None);
        }
    };
    let (user_id, created) =
        crate::storage::users::create_signup_user(state.require_db()?, &row.email, risk).await?;
    if created {
        tracing::info!(user_id = %user_id.0, via = "open_signup", "magic-link bootstrap created account");
    }
    // A concurrent signup may have won the race, and the account it resolved is
    // not a new one to compensate away or to count as a signup.
    let how = if created {
        Bootstrap::Founded
    } else {
        Bootstrap::Existing
    };
    Ok(Some((user_id, how)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_cookie_cannot_ride_a_cross_site_post() {
        let cfg = SessionConfig::default();
        for (name, path) in [
            (CODE_NONCE_COOKIE, CODE_COOKIE_PATH),
            (CONFIRM_NONCE_COOKIE, CONFIRM_COOKIE_PATH),
        ] {
            let c = nonce_cookie(name, path, &cfg, "n", 15);
            // The CSRF exemption for both endpoints rests on these.
            assert_eq!(c.same_site(), Some(SameSite::Lax), "{name}");
            assert_eq!(c.http_only(), Some(true), "{name}");
            assert_eq!(c.path(), Some(path), "{name}");
            assert_eq!(c.max_age(), Some(CookieDuration::minutes(20)), "{name}");
        }
    }

    #[test]
    fn an_unset_cookie_domain_leaves_the_cookie_host_only() {
        // `Domain=` with nothing after it is a cookie the browser drops.
        let cfg = SessionConfig::default();
        assert_eq!(
            nonce_cookie(CODE_NONCE_COOKIE, CODE_COOKIE_PATH, &cfg, "n", 15).domain(),
            None
        );
        let cfg = SessionConfig {
            cookie_domain: "example.test".into(),
            ..SessionConfig::default()
        };
        assert_eq!(
            nonce_cookie(CODE_NONCE_COOKIE, CODE_COOKIE_PATH, &cfg, "n", 15).domain(),
            Some("example.test")
        );
    }

    #[test]
    fn confirm_page_scrubs_the_token_from_every_analytics_send() {
        let html = MagicLinkConfirmPage {
            email: "who@example.com".into(),
            token: "s3cret-token".into(),
            csrf: "nonce".into(),
            analytics: Some("website-id"),
        }
        .render()
        .expect("renders");
        // The page URL holds a single-use credential.
        assert!(html.contains(r#"data-before-send="smScrubAuthUrl""#));
        assert!(html.contains("auth_url_scrub.js"));
        assert!(html.contains(r#"data-umami-event="magic-link-confirmed""#));
    }

    #[test]
    fn auth_pages_load_no_tracker_when_analytics_is_off() {
        let confirm = MagicLinkConfirmPage {
            email: "who@example.com".into(),
            token: "s3cret-token".into(),
            csrf: "nonce".into(),
            analytics: None,
        }
        .render()
        .expect("renders");
        assert!(!confirm.contains("analytics.uptimepage.dev"));

        let invalid = MagicLinkInvalidPage {
            expiry_minutes: 15,
            analytics: None,
        }
        .render()
        .expect("renders");
        assert!(!invalid.contains("analytics.uptimepage.dev"));
        assert!(!invalid.contains("auth_analytics.js"));
    }

    #[test]
    fn a_refused_code_does_not_read_as_a_dead_link() {
        let html = MagicLinkCodeRefusedPage {
            analytics: Some("website-id"),
            spent: true,
        }
        .render()
        .expect("renders");
        assert!(html.contains("still signs"));
        assert!(!html.contains("request a new link"));
        assert!(html.contains(r#"data-auth-event-reason="code-refused""#));
        // One reason for wrong, spent and wrong-browser alike.
        assert_eq!(html.matches("data-auth-event-reason").count(), 1);
    }

    #[test]
    fn a_missing_binding_does_not_read_as_a_dead_link() {
        // Where the route lands with no cookie behind it.
        let html = MagicLinkCodeRefusedPage {
            analytics: None,
            spent: true,
        }
        .render()
        .expect("renders");
        assert!(!html.contains("expired"));
        assert!(html.contains("still signs"));
    }

    #[test]
    fn a_shape_refusal_does_not_claim_the_attempt_is_gone() {
        let html = MagicLinkCodeRefusedPage {
            analytics: None,
            spent: false,
        }
        .render()
        .expect("renders");
        assert!(html.contains("Nothing has been used up"));
        assert!(!html.contains("one attempt"));
        assert!(html.contains(r#"data-auth-event-reason="code-malformed""#));
    }

    #[test]
    fn invalid_page_reports_the_failure_without_revealing_the_cause() {
        let html = MagicLinkInvalidPage {
            expiry_minutes: 15,
            analytics: Some("website-id"),
        }
        .render()
        .expect("renders");
        assert!(html.contains(r#"data-auth-event="login-error""#));
        assert!(html.contains(r#"data-auth-event-reason="link-invalid""#));
        assert!(html.contains("auth_analytics.js"));
        // A fixed reason cannot leak which of the three cases happened.
        assert_eq!(html.matches("data-auth-event-reason").count(), 1);
    }
}
