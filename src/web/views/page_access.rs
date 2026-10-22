//! Signing in to a private status page, on the page's own host: an emailed
//! code (or the link beside it in the same mail), then an optional passkey;
//! or the page password the owner shared.
//!
//! Every answer to an address is the same whether or not the list covers it,
//! and the mail goes out from a spawned task so timing says nothing either.
//! Visitors here are not app users; nothing touches the app's own sign-in.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Form, Query, State};
use axum::http::header::{HOST, RETRY_AFTER, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration as CookieDuration;
use tower_cookies::{Cookie, Cookies};
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::*;
use webauthn_rs_proto::ResidentKeyRequirement;

use crate::app::AppState;
use crate::auth::fingerprint;
use crate::domain::{
    AccessEventKind, AccessEventMethod, MAX_PAGE_PASSWORD_CHARS, PageAccess, PageRef,
    normalize_email,
};
use crate::email::{EmailAddress, EmailTemplate, SendClass, TransactionalEmail};
use crate::error::public::PublicAppError;
use crate::i18n::Tr;
use crate::public_status::access::{
    Access, same_origin, session_clear_cookie, session_cookie, session_set_cookie,
};
use crate::request::client_ip::{ClientIp, network_key};
use crate::request::host::{
    is_subdomain_public_request, published_page_origin, resolve_status_page,
};
use crate::request::json::Json;
use crate::robots::mark_private;
use crate::storage::page_access::codes::{self, CODE_TTL_MINUTES, CodeOutcome};
use crate::storage::page_access::events::{self, NewEvent};
use crate::storage::page_access::password::{self, TryOutcome};
use crate::storage::page_access::{passkeys, sessions, viewers};
use crate::storage::status_pages;
use crate::templates::filters;

use super::page_branding::{BrandingView, resolve_branding, status_home};
use crate::web::error::render_public_error;

/// Binds an emailed code to the browser that asked for it. Scoped to the
/// sign-in paths; nothing else on the page needs it.
const CODE_NONCE_COOKIE: &str = "_up_page_code";
/// Double-submit nonce between the link's confirm page and its POST.
const CONFIRM_NONCE_COOKIE: &str = "_up_page_confirm";
const ACCESS_PATH: &str = "/access";

#[derive(Template, WebTemplate)]
#[template(path = "public/access.html")]
pub struct AccessPage {
    pub tr: Tr,
    pub branding: BrandingView,
    /// `email`, `code`, `refused`, `confirm`, `invalid` or `passkey`.
    pub step: &'static str,
    pub email: String,
    pub token: String,
    pub csrf: String,
    /// A translation key shown above the form, or empty.
    pub error: &'static str,
    /// Where to go once signed in: a path on this host.
    pub next: String,
    /// The passkey button and the save offer.
    pub passkeys: bool,
    /// The ways in the sign-in screen offers.
    pub email_way: bool,
    pub password_way: bool,
    /// The page host, as the password form's username: a password manager
    /// keys its saved entry on it, so it keeps one per page.
    pub username: String,
    /// Shown above the password field: a wrong password, or the wait.
    pub password_error: String,
    /// The step's interpolated lines, filled in by [`Self::respond`].
    pub title: String,
    pub lead: String,
}

impl AccessPage {
    async fn build(
        state: &AppState,
        headers: &HeaderMap,
        page: PageRef,
        step: &'static str,
    ) -> Self {
        let tr = page_tr(state, page).await;
        let mut branding = resolve_branding(
            state,
            headers,
            page.org,
            page.page,
            &tr.t("email-fallback-page-name"),
        )
        .await;
        branding.hide_from_search = true;
        let next = branding.home.to_owned();
        Self {
            tr,
            branding,
            step,
            email: String::new(),
            token: String::new(),
            csrf: String::new(),
            error: "",
            next,
            passkeys: false,
            email_way: true,
            password_way: false,
            username: String::new(),
            password_error: String::new(),
            title: String::new(),
            lead: String::new(),
        }
    }

    /// The sign-in screen's ways in, as the owner set them.
    fn ways(mut self, state: &AppState, access: &PageAccess, headers: &HeaderMap) -> Self {
        self.email_way = access.settings.email_enabled;
        self.password_way = access.settings.password_enabled;
        self.passkeys = passkeys_on(state, access, headers);
        self.username = headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        self
    }

    fn respond(mut self, status: StatusCode) -> Response {
        let tr = self.tr;
        let page = self.branding.display_name.clone();
        match self.step {
            "email" if !self.email_way && self.password_way => {
                self.lead = tr.t_args("access-lead-password", [("page", page.into())])
            }
            "email" => self.lead = tr.t_args("access-lead", [("page", page.into())]),
            "code" => {
                self.lead = tr.t_args(
                    "access-sent",
                    [
                        ("email", self.email.clone().into()),
                        ("minutes", CODE_TTL_MINUTES.into()),
                    ],
                )
            }
            "confirm" => {
                self.title = tr.t_args("access-confirm-title", [("page", page.into())]);
                self.lead = tr.t_args(
                    "access-confirm-lead",
                    [("email", self.email.clone().into())],
                );
            }
            _ => {}
        }
        let lang = self.tr.lang();
        let mut resp = (status, self).into_response();
        resp.headers_mut().insert(
            axum::http::header::CONTENT_LANGUAGE,
            axum::http::HeaderValue::from_static(lang),
        );
        mark_private(&mut resp);
        resp
    }
}

async fn page_tr(state: &AppState, page: PageRef) -> Tr {
    match state.db.as_ref() {
        Some(pool) => Tr::new(
            status_pages::public_locale(pool, page)
                .await
                .unwrap_or_default(),
        ),
        None => Tr::default(),
    }
}

/// The 401 every gated page answers with when nobody may read it yet.
pub async fn sign_in_screen(
    state: &AppState,
    headers: &HeaderMap,
    page: PageRef,
    access: &PageAccess,
    next: &str,
) -> Response {
    let mut view = AccessPage::build(state, headers, page, "email")
        .await
        .ways(state, access, headers);
    if let Some(next) = crate::auth::url::safe_redirect_target(next) {
        view.next = next.to_owned();
    }
    view.respond(StatusCode::UNAUTHORIZED)
}

/// Passkeys shortcut the email code, so they stop with it. They live on the
/// page's own host: a path-based deploy serves the page from the app host,
/// where they would sit beside the app's own passkeys and each sign-in screen
/// would offer the other's, so the page offers the email code alone there.
fn passkeys_on(state: &AppState, access: &PageAccess, headers: &HeaderMap) -> bool {
    access.settings.email_enabled
        && access.settings.passkeys_enabled
        && state.db.is_some()
        && is_subdomain_public_request(&state.request_state(), headers)
}

/// The page this host serves and its settings, for a sign-in path. A page
/// that is not private has nothing to sign in to.
async fn private_page(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(PageRef, std::sync::Arc<PageAccess>), Response> {
    let page = resolve_status_page(&state.request_state(), headers)
        .await
        .map_err(render_public_error)?;
    let access = state
        .page_gate
        .settings(state.db.as_ref(), page)
        .await
        .map_err(render_public_error)?;
    if !access.is_private() {
        let home = status_home(state, headers);
        return Err(Redirect::to(home).into_response());
    }
    Ok((page, access))
}

fn forbidden() -> Response {
    let mut resp = StatusCode::FORBIDDEN.into_response();
    mark_private(&mut resp);
    resp
}

struct Requester {
    ip_hash: Option<String>,
    ua_hash: Option<String>,
}

/// Keyed by network, so every per-IP limit here counts an IPv6 client's
/// whole /64 as one.
fn requester(state: &AppState, ip: std::net::IpAddr, headers: &HeaderMap) -> Requester {
    let salt = state.cfg.auth.fingerprint_salt.as_str();
    Requester {
        ip_hash: fingerprint::hash_fingerprint(salt, &network_key(ip).to_string()),
        ua_hash: fingerprint::hash_fingerprint(
            salt,
            headers
                .get(USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
        ),
    }
}

fn nonce_cookie(
    name: &'static str,
    path: &'static str,
    value: &str,
    secure: bool,
) -> Cookie<'static> {
    let mut c = Cookie::new(name, value.to_owned());
    c.set_http_only(true);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path(path);
    c.set_max_age(CookieDuration::minutes(CODE_TTL_MINUTES + 5));
    c
}

fn clear_nonce(cookies: &Cookies, name: &'static str, path: &'static str) {
    let mut gone = Cookie::new(name, "");
    gone.set_path(path);
    cookies.remove(gone);
}

fn safe_next(raw: &str, fallback: &str) -> String {
    crate::auth::url::safe_redirect_target(raw)
        .unwrap_or(fallback)
        .to_owned()
}

// ---------------------------------------------------------------------------
// Email code
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RequestForm {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub next: String,
}

/// `POST /access/request`. The same answer for every address; only one the
/// list covers, and that is not blocked, gets a mail.
pub async fn request_code(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    Form(form): Form<RequestForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (page, access) = match private_page(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let mut view = AccessPage::build(&state, &headers, page, "email")
        .await
        .ways(&state, &access, &headers);
    view.next = safe_next(&form.next, view.branding.home);
    let Some(email) = normalize_email(&form.email) else {
        view.error = "access-bad-email";
        view.email = form.email.trim().chars().take(254).collect();
        return view.respond(StatusCode::BAD_REQUEST);
    };
    let Ok(pool) = state.require_db().cloned() else {
        return view.respond(StatusCode::SERVICE_UNAVAILABLE);
    };
    // Reused while it lives, so a second request does not orphan the code
    // already in the reader's inbox.
    let nonce = cookies
        .get(CODE_NONCE_COOKIE)
        .map(|c| c.value().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(crate::security::token_hash::generate_raw_token);
    cookies.add(nonce_cookie(
        CODE_NONCE_COOKIE,
        ACCESS_PATH,
        &nonce,
        state.cfg.auth.session.cookie_secure,
    ));

    let who = requester(&state, ip, &headers);
    let origin =
        published_page_origin(&state.request_state(), &headers, page.page).unwrap_or_else(|| {
            state
                .cfg
                .auth
                .public_base_url
                .trim_end_matches('/')
                .to_owned()
        });
    let mail = MailContext {
        sender: state.email_sender.clone(),
        from: EmailAddress::new(
            state.cfg.email.from_address.clone(),
            state.cfg.email.from_name.clone(),
        ),
        locale: view.tr.locale(),
        page_name: view.branding.display_name.clone(),
        origin,
    };
    let admitted = access.admits(&email);
    tokio::spawn(send_code(
        pool,
        page,
        email.clone(),
        nonce,
        admitted,
        who,
        mail,
    ));

    view.step = "code";
    view.email = email;
    view.respond(StatusCode::OK)
}

struct MailContext {
    sender: std::sync::Arc<dyn crate::email::EmailSender>,
    from: EmailAddress,
    locale: crate::domain::Locale,
    page_name: String,
    origin: String,
}

async fn send_code(
    pool: sqlx::PgPool,
    page: PageRef,
    email: String,
    nonce: String,
    admitted: bool,
    who: Requester,
    mail: MailContext,
) {
    let failed = |viewer: Option<Uuid>, reason: &'static str| NewEvent {
        viewer,
        method: Some(AccessEventMethod::Email),
        failure_reason: Some(reason),
        ip_hash: who.ip_hash.as_deref(),
        user_agent_hash: who.ua_hash.as_deref(),
        ..NewEvent::new(page, AccessEventKind::CodeFailed)
    };
    if !admitted {
        events::record_quietly(&pool, failed(None, "not_listed")).await;
        return;
    }
    let viewer = match viewers::find_by_email(&pool, page, &email).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "page access: viewer lookup failed");
            return;
        }
    };
    if let Some(v) = viewer.as_ref().filter(|v| v.blocked_at.is_some()) {
        events::record_quietly(&pool, failed(Some(v.id), "blocked")).await;
        return;
    }
    let viewer = viewer.map(|v| v.id);
    let created = match codes::create_claimed(
        &pool,
        page,
        &email,
        &nonce,
        who.ip_hash.as_deref(),
        viewer.is_some(),
    )
    .await
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            events::record_quietly(&pool, failed(viewer, "throttled")).await;
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "page access: code not created");
            return;
        }
    };
    let url = crate::auth::url::token_link(&mail.origin, "/access/verify", &created.token);
    let outgoing = TransactionalEmail {
        from: mail.from,
        to: EmailAddress::new(email.clone(), email.clone()),
        template: EmailTemplate::PageAccessCode {
            locale: mail.locale,
            page_name: mail.page_name,
            url,
            code: created.code,
            expires_in_minutes: CODE_TTL_MINUTES as u32,
        },
    };
    if let Err(e) = mail.sender.send(outgoing, SendClass::Immediate).await {
        tracing::warn!(error = %e, "page access: code mail failed");
        if let Err(e) = codes::release_send(&pool, page, created.id).await {
            tracing::warn!(error = %e, "page access: send release failed");
        }
        return;
    }
    if let Err(e) = codes::supersede_others(&pool, page, &email, created.id).await {
        tracing::warn!(error = %e, "page access: supersede failed");
    }
    metrics::counter!(crate::metric_names::PAGE_ACCESS_CODES_SENT).increment(1);
    events::record_quietly(
        &pool,
        NewEvent {
            viewer,
            method: Some(AccessEventMethod::Email),
            ip_hash: who.ip_hash.as_deref(),
            user_agent_hash: who.ua_hash.as_deref(),
            ..NewEvent::new(page, AccessEventKind::CodeSent)
        },
    )
    .await;
}

#[derive(Debug, Deserialize)]
pub struct CodeForm {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub next: String,
    /// Shown again if the code is mistyped, so "send another" still works.
    #[serde(default)]
    pub email: String,
}

/// `POST /access/code`. One attempt, from the browser that asked.
pub async fn submit_code(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    Form(form): Form<CodeForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (page, access) = match private_page(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Ok(pool) = state.require_db() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let who = requester(&state, ip, &headers);
    let mut view = AccessPage::build(&state, &headers, page, "refused").await;
    let next = safe_next(&form.next, view.branding.home);
    view.next = next.clone();
    let Some(nonce) = cookies.get(CODE_NONCE_COOKIE).map(|c| c.value().to_owned()) else {
        return view.respond(StatusCode::UNAUTHORIZED);
    };
    match codes::consume_code(pool, page, &nonce, &form.code).await {
        Ok(CodeOutcome::Ok(email)) => {
            clear_nonce(&cookies, CODE_NONCE_COOKIE, ACCESS_PATH);
            sign_in(
                &state, &headers, &cookies, page, &access, &email, &who, &next,
            )
            .await
        }
        Ok(CodeOutcome::Malformed) => {
            view.step = "code";
            view.error = "access-bad-code";
            view.email = normalize_email(&form.email).unwrap_or_default();
            view.respond(StatusCode::BAD_REQUEST)
        }
        Ok(CodeOutcome::Refused) => {
            clear_nonce(&cookies, CODE_NONCE_COOKIE, ACCESS_PATH);
            refused_event(pool, page, &who, "code_refused").await;
            view.respond(StatusCode::UNAUTHORIZED)
        }
        Err(e) => {
            tracing::warn!(error = %e, "page access: code redemption failed");
            view.respond(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn refused_event(pool: &sqlx::PgPool, page: PageRef, who: &Requester, reason: &'static str) {
    metrics::counter!(crate::metric_names::PAGE_ACCESS_SIGN_IN_REFUSED, "reason" => reason)
        .increment(1);
    events::record_quietly(
        pool,
        NewEvent {
            failure_reason: Some(reason),
            ip_hash: who.ip_hash.as_deref(),
            user_agent_hash: who.ua_hash.as_deref(),
            ..NewEvent::new(page, AccessEventKind::CodeFailed)
        },
    )
    .await;
}

/// The address proved itself by email: open a session, and offer a passkey
/// to someone who has none on this host yet. The list is checked again here,
/// since it may have changed while the mail was in flight.
#[allow(clippy::too_many_arguments)]
async fn sign_in(
    state: &AppState,
    headers: &HeaderMap,
    cookies: &Cookies,
    page: PageRef,
    access: &PageAccess,
    email: &str,
    who: &Requester,
    next: &str,
) -> Response {
    let Ok(pool) = state.require_db() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let invalid = || async {
        let view = AccessPage::build(state, headers, page, "invalid").await;
        view.respond(StatusCode::UNAUTHORIZED)
    };
    if !access.admits(email) {
        refused_event(pool, page, who, "not_listed").await;
        return invalid().await;
    }
    let viewer = match viewers::record_verified(pool, page, email).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            refused_event(pool, page, who, "blocked").await;
            return invalid().await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "page access: viewer not recorded");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    if let Err(resp) = open_session(
        state,
        headers,
        cookies,
        page,
        access,
        viewer.id,
        who,
        AccessEventMethod::Email,
    )
    .await
    {
        return resp;
    }
    events::record_quietly(
        pool,
        NewEvent {
            viewer: Some(viewer.id),
            method: Some(AccessEventMethod::Email),
            ip_hash: who.ip_hash.as_deref(),
            user_agent_hash: who.ua_hash.as_deref(),
            ..NewEvent::new(page, AccessEventKind::SignedIn)
        },
    )
    .await;
    let offer = passkeys_on(state, access, headers)
        && match page_webauthn(state, headers) {
            Ok((_, rp_id)) => passkeys::for_viewer(pool, page, viewer.id, &rp_id)
                .await
                .map(|keys| keys.is_empty())
                .unwrap_or(false),
            Err(_) => false,
        };
    let to = if offer {
        let next: String = url::form_urlencoded::byte_serialize(next.as_bytes()).collect();
        format!("/access/passkey?next={next}")
    } else {
        next.to_owned()
    };
    Redirect::to(&to).into_response()
}

/// Replaces whatever session this browser held for the page, so a cookie
/// planted before sign-in never becomes a signed-in one.
#[allow(clippy::too_many_arguments)]
async fn open_session(
    state: &AppState,
    headers: &HeaderMap,
    cookies: &Cookies,
    page: PageRef,
    access: &PageAccess,
    viewer: Uuid,
    who: &Requester,
    method: AccessEventMethod,
) -> Result<(), Response> {
    let pool = state
        .require_db()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
    if let Some(old) = session_cookie(headers) {
        let _ = sessions::delete(pool, page, &old).await;
    }
    let created = sessions::create(
        pool,
        page,
        sessions::Opener::Email(viewer),
        access.settings.session_days,
        who.ip_hash.as_deref(),
        who.ua_hash.as_deref(),
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "page access: session not created");
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    })?;
    cookies.add(session_set_cookie(
        &created.cookie,
        created.session_days,
        state.cfg.auth.session.cookie_secure,
    ));
    metrics::counter!(crate::metric_names::PAGE_ACCESS_SIGN_INS, "method" => method.as_db_str())
        .increment(1);
    Ok(())
}

// ---------------------------------------------------------------------------
// The page password
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PasswordForm {
    #[serde(default)]
    pub password: Option<SecretString>,
    #[serde(default)]
    pub next: String,
}

/// `POST /access/password`. Each try claims one of the network's wrong-try
/// slots before argon2 runs, so a parallel burst gets no more checks than a
/// slow one; a right password hands its slot back. No page-wide lockout: that
/// would hand anyone a switch that locks out every real visitor.
pub async fn submit_password(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    Form(form): Form<PasswordForm>,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let (page, access) = match private_page(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let mut view = AccessPage::build(&state, &headers, page, "email")
        .await
        .ways(&state, &access, &headers);
    let next = safe_next(&form.next, view.branding.home);
    view.next = next.clone();
    if !access.settings.password_enabled {
        return view.respond(StatusCode::UNAUTHORIZED);
    }
    let Ok(pool) = state.require_db() else {
        return view.respond(StatusCode::SERVICE_UNAVAILABLE);
    };
    let presented = form.password.unwrap_or_else(|| SecretString::from(""));
    // Never a match, so it neither runs argon2 nor spends one of the
    // network's tries.
    if presented.expose_secret().is_empty()
        || presented.expose_secret().chars().count() > MAX_PAGE_PASSWORD_CHARS
    {
        view.password_error = view.tr.t("access-password-wrong");
        return view.respond(StatusCode::UNAUTHORIZED);
    }
    let who = requester(&state, ip, &headers);
    let claim = match password::claim_try(
        pool,
        page,
        who.ip_hash.as_deref(),
        who.ua_hash.as_deref(),
    )
    .await
    {
        Ok(TryOutcome::Claimed(claim)) => claim,
        Ok(TryOutcome::Throttled { retry_after_secs }) => {
            metrics::counter!(crate::metric_names::PAGE_ACCESS_SIGN_IN_REFUSED, "reason" => "password_throttled")
                .increment(1);
            let minutes = (retry_after_secs + 59) / 60;
            view.password_error = view
                .tr
                .t_args("access-password-throttled", [("minutes", minutes.into())]);
            let mut resp = view.respond(StatusCode::TOO_MANY_REQUESTS);
            resp.headers_mut()
                .insert(RETRY_AFTER, retry_after_secs.into());
            return resp;
        }
        Err(e) => {
            tracing::warn!(error = %e, "page password: try not claimed");
            return view.respond(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let hash = match password::live_hash(pool, page).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "page password: hash not read");
            return view.respond(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    let right = match &hash {
        Some(h) => crate::security::page_password::verify(presented, h.clone()).await,
        None => false,
    };
    let opened = match hash.as_deref().filter(|_| right) {
        Some(h) => {
            password::open_session(
                pool,
                page,
                h,
                claim,
                session_cookie(&headers).as_deref(),
                who.ip_hash.as_deref(),
                who.ua_hash.as_deref(),
            )
            .await
        }
        None => Ok(None),
    };
    let created = match opened {
        Ok(Some(created)) => created,
        Ok(None) => {
            metrics::counter!(crate::metric_names::PAGE_ACCESS_SIGN_IN_REFUSED, "reason" => "wrong_password")
                .increment(1);
            view.password_error = view.tr.t("access-password-wrong");
            return view.respond(StatusCode::UNAUTHORIZED);
        }
        Err(e) => {
            tracing::warn!(error = %e, "page password: session not opened");
            return view.respond(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    cookies.add(session_set_cookie(
        &created.cookie,
        created.session_days,
        state.cfg.auth.session.cookie_secure,
    ));
    metrics::counter!(crate::metric_names::PAGE_ACCESS_SIGN_INS, "method" => AccessEventMethod::Password.as_db_str())
        .increment(1);
    events::record_quietly(
        pool,
        NewEvent {
            method: Some(AccessEventMethod::Password),
            ip_hash: who.ip_hash.as_deref(),
            user_agent_hash: who.ua_hash.as_deref(),
            ..NewEvent::new(page, AccessEventKind::SignedIn)
        },
    )
    .await;
    Redirect::to(&next).into_response()
}

// ---------------------------------------------------------------------------
// The link in the same mail
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct VerifyQuery {
    #[serde(default)]
    pub token: String,
}

/// `GET /access/verify`. Shows, never spends: mail scanners open links
/// before people do.
pub async fn verify_landing(
    State(state): State<AppState>,
    headers: HeaderMap,
    cookies: Cookies,
    Query(q): Query<VerifyQuery>,
) -> Response {
    let (page, _) = match private_page(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Ok(pool) = state.require_db() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let token = q.token.trim();
    match codes::peek_link(pool, page, token).await {
        Ok(Some(email)) => {
            let csrf = crate::security::token_hash::generate_raw_token();
            cookies.add(nonce_cookie(
                CONFIRM_NONCE_COOKIE,
                "/access/verify",
                &csrf,
                state.cfg.auth.session.cookie_secure,
            ));
            let mut view = AccessPage::build(&state, &headers, page, "confirm").await;
            view.email = email;
            view.token = token.to_owned();
            view.csrf = csrf;
            view.respond(StatusCode::OK)
        }
        Ok(None) => AccessPage::build(&state, &headers, page, "invalid")
            .await
            .respond(StatusCode::GONE),
        Err(e) => {
            tracing::warn!(error = %e, "page access: link lookup failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub csrf: String,
}

/// `POST /access/verify`. Spends the link and signs the browser in.
pub async fn verify_confirm(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    Form(form): Form<ConfirmForm>,
) -> Response {
    let (page, access) = match private_page(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let Ok(pool) = state.require_db() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let who = requester(&state, ip, &headers);
    let csrf_ok = cookies.get(CONFIRM_NONCE_COOKIE).is_some_and(|c| {
        !form.csrf.is_empty() && bool::from(c.value().as_bytes().ct_eq(form.csrf.as_bytes()))
    });
    if !csrf_ok {
        refused_event(pool, page, &who, "confirm_csrf").await;
        return AccessPage::build(&state, &headers, page, "invalid")
            .await
            .respond(StatusCode::FORBIDDEN);
    }
    clear_nonce(&cookies, CONFIRM_NONCE_COOKIE, "/access/verify");
    match codes::consume_link(pool, page, form.token.trim()).await {
        Ok(Some(email)) => {
            let home = status_home(&state, &headers);
            sign_in(
                &state, &headers, &cookies, page, &access, &email, &who, home,
            )
            .await
        }
        Ok(None) => {
            refused_event(pool, page, &who, "invalid_link").await;
            AccessPage::build(&state, &headers, page, "invalid")
                .await
                .respond(StatusCode::GONE)
        }
        Err(e) => {
            tracing::warn!(error = %e, "page access: link redemption failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Sign-out
// ---------------------------------------------------------------------------

/// `POST /access/sign-out`.
pub async fn sign_out(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
) -> Response {
    if !same_origin(&headers) {
        return forbidden();
    }
    let home = status_home(&state, &headers);
    let page = match resolve_status_page(&state.request_state(), &headers).await {
        Ok(p) => p,
        Err(err) => return render_public_error(err),
    };
    if let (Ok(pool), Some(cookie)) = (state.require_db(), session_cookie(&headers)) {
        match sessions::delete(pool, page, &cookie).await {
            Ok(viewer) => {
                let who = requester(&state, ip, &headers);
                events::record_quietly(
                    pool,
                    NewEvent {
                        viewer,
                        ip_hash: who.ip_hash.as_deref(),
                        user_agent_hash: who.ua_hash.as_deref(),
                        ..NewEvent::new(page, AccessEventKind::SignedOut)
                    },
                )
                .await;
            }
            Err(e) => tracing::warn!(error = %e, "page access: sign-out failed"),
        }
    }
    cookies.add(session_clear_cookie(state.cfg.auth.session.cookie_secure));
    Redirect::to(home).into_response()
}

// ---------------------------------------------------------------------------
// Passkeys
// ---------------------------------------------------------------------------

/// Built per request: a page passkey belongs to the host it was minted on,
/// which is the page's own subdomain, its custom domain, or the app host in
/// path mode. The host has already resolved to this page, so it is not a
/// value the client chose freely.
fn page_webauthn(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Webauthn, String), PublicAppError> {
    let host = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or(PublicAppError::NotFound)?;
    let scheme = Url::parse(&state.cfg.auth.public_base_url)
        .map(|u| u.scheme().to_owned())
        .unwrap_or_else(|_| "https".to_owned());
    let origin = Url::parse(&format!("{scheme}://{}", host.to_ascii_lowercase()))
        .map_err(|_| PublicAppError::NotFound)?;
    let rp_id = origin
        .host_str()
        .ok_or(PublicAppError::NotFound)?
        .to_owned();
    let webauthn = WebauthnBuilder::new(&rp_id, &origin)
        .and_then(|b| b.rp_name(&rp_id).build())
        .map_err(|e| PublicAppError::Internal(anyhow::anyhow!("page webauthn for {rp_id}: {e}")))?;
    Ok((webauthn, rp_id))
}

#[derive(Debug, Deserialize)]
pub struct PasskeyOfferQuery {
    #[serde(default)]
    pub next: String,
}

/// `GET /access/passkey`. The offer after a first sign-in, skippable.
pub async fn passkey_offer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<PasskeyOfferQuery>,
) -> Response {
    let (page, access) = match private_page(&state, &headers).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let home = status_home(&state, &headers);
    let next = safe_next(&q.next, home);
    let signed_in = matches!(
        state
            .page_gate
            .check(state.db.as_ref(), page, &headers)
            .await,
        Ok(Access::Viewer(_))
    );
    if !signed_in || !passkeys_on(&state, &access, &headers) {
        return Redirect::to(&next).into_response();
    }
    let mut view = AccessPage::build(&state, &headers, page, "passkey").await;
    view.next = next;
    view.passkeys = true;
    view.respond(StatusCode::OK)
}

#[derive(Debug, Serialize)]
pub struct CeremonyStart {
    pub handle: String,
    pub options: serde_json::Value,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CeremonyFinish {
    pub handle: String,
    #[schema(value_type = Object)]
    pub credential: serde_json::Value,
    #[serde(default)]
    pub next: String,
}

#[derive(Debug, Serialize)]
pub struct SignedIn {
    pub redirect: String,
}

/// One refusal for every way a passkey can fail here, so probing learns
/// nothing; the screen falls back to the email code.
#[derive(Debug)]
pub enum PasskeyError {
    Unavailable,
    Refused,
    /// A refusal the viewer can act on, such as a full passkey list.
    BadRequest(&'static str),
    Recheck,
    Forbidden,
    Internal,
}

impl IntoResponse for PasskeyError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Unavailable => (StatusCode::NOT_FOUND, "PASSKEY_UNAVAILABLE"),
            Self::Refused => (StatusCode::BAD_REQUEST, "PASSKEY_REFUSED"),
            Self::BadRequest(code) => (StatusCode::BAD_REQUEST, code),
            Self::Recheck => (StatusCode::FORBIDDEN, "PAGE_EMAIL_RECHECK"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            Self::Internal => (StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE"),
        };
        let mut resp = (
            status,
            axum::Json(serde_json::json!({ "error": { "code": code } })),
        )
            .into_response();
        mark_private(&mut resp);
        resp
    }
}

impl From<crate::error::AppError> for PasskeyError {
    fn from(e: crate::error::AppError) -> Self {
        if let crate::error::AppError::BadRequest { code, .. } = e {
            return Self::BadRequest(code);
        }
        tracing::warn!(error = %e, "page passkey ceremony failed");
        Self::Internal
    }
}

impl From<PublicAppError> for PasskeyError {
    fn from(e: PublicAppError) -> Self {
        match e {
            PublicAppError::NotFound => Self::Unavailable,
            other => {
                tracing::warn!(error = %other, "page passkey ceremony failed");
                Self::Internal
            }
        }
    }
}

/// The page, its settings and a pool, for a passkey ceremony. Refused while
/// the owner has passkeys off, at both halves of either ceremony.
async fn passkey_page(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(PageRef, std::sync::Arc<PageAccess>, sqlx::PgPool), PasskeyError> {
    if !same_origin(headers) {
        return Err(PasskeyError::Forbidden);
    }
    let page = resolve_status_page(&state.request_state(), headers).await?;
    let access = state.page_gate.settings(state.db.as_ref(), page).await?;
    if !access.is_private() || !passkeys_on(state, &access, headers) {
        return Err(PasskeyError::Unavailable);
    }
    let pool = state.require_db().cloned()?;
    Ok((page, access, pool))
}

/// `POST /access/passkey/register/start`, for the signed-in viewer.
pub async fn register_start(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CeremonyStart>, PasskeyError> {
    let (page, _, pool) = passkey_page(&state, &headers).await?;
    let Access::Viewer(viewer) = state.page_gate.check(Some(&pool), page, &headers).await? else {
        return Err(PasskeyError::Forbidden);
    };
    let (webauthn, rp_id) = page_webauthn(&state, &headers)?;
    let exclude =
        passkeys::credential_ids(&passkeys::for_viewer(&pool, page, viewer.id, &rp_id).await?);
    let (mut options, ceremony) = webauthn
        .start_passkey_registration(viewer.id, &viewer.email, &viewer.email, Some(exclude))
        .map_err(|e| {
            tracing::warn!(error = %e, "page passkey registration did not start");
            PasskeyError::Internal
        })?;
    if let Some(selection) = options.public_key.authenticator_selection.as_mut() {
        selection.resident_key = Some(ResidentKeyRequirement::Required);
        selection.require_resident_key = true;
    }
    let handle = crate::storage::passkeys::generate_handle();
    passkeys::put_state(&pool, page, &handle, Some(viewer.id), &ceremony).await?;
    Ok(Json(CeremonyStart {
        handle,
        options: serde_json::to_value(options).map_err(|_| PasskeyError::Internal)?,
    }))
}

/// `POST /access/passkey/register/finish`.
pub async fn register_finish(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Json(body): Json<CeremonyFinish>,
) -> Result<StatusCode, PasskeyError> {
    let (page, _, pool) = passkey_page(&state, &headers).await?;
    let Access::Viewer(viewer) = state.page_gate.check(Some(&pool), page, &headers).await? else {
        return Err(PasskeyError::Forbidden);
    };
    let (webauthn, rp_id) = page_webauthn(&state, &headers)?;
    let Some(taken) = passkeys::take_state(&pool, page, &body.handle).await? else {
        return Err(PasskeyError::Refused);
    };
    // A handle is opaque, not a capability: it finishes only for the viewer
    // who started it.
    if taken.viewer != Some(viewer.id) {
        return Err(PasskeyError::Refused);
    }
    let ceremony: PasskeyRegistration = taken.decode()?;
    let credential: RegisterPublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| PasskeyError::Refused)?;
    if credential.extensions.cred_props.as_ref().and_then(|p| p.rk) == Some(false) {
        return Err(PasskeyError::Refused);
    }
    let stored = webauthn
        .finish_passkey_registration(&credential, &ceremony)
        .map_err(|e| {
            tracing::warn!(error = %e, "page passkey registration rejected");
            PasskeyError::Refused
        })?;
    let who = requester(&state, ip, &headers);
    passkeys::insert(
        &pool,
        page,
        viewer.id,
        &stored,
        &rp_id,
        who.ip_hash.as_deref(),
        who.ua_hash.as_deref(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /access/passkey/login/start`. Discoverable, so no address is named.
pub async fn login_start(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<CeremonyStart>, PasskeyError> {
    let (page, _, pool) = passkey_page(&state, &headers).await?;
    let (webauthn, _) = page_webauthn(&state, &headers)?;
    let (options, ceremony) = webauthn.start_discoverable_authentication().map_err(|e| {
        tracing::warn!(error = %e, "page passkey sign-in did not start");
        PasskeyError::Internal
    })?;
    let handle = crate::storage::passkeys::generate_handle();
    passkeys::put_state(&pool, page, &handle, None, &ceremony).await?;
    Ok(Json(CeremonyStart {
        handle,
        options: serde_json::to_value(options).map_err(|_| PasskeyError::Internal)?,
    }))
}

/// `POST /access/passkey/login/finish`. The passkey names a viewer of this
/// page on this host, who must still be on the list and not blocked, and
/// whose email proof is not older than the owner's re-check period.
pub async fn login_finish(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    cookies: Cookies,
    Json(body): Json<CeremonyFinish>,
) -> Result<Json<SignedIn>, PasskeyError> {
    let (page, access, pool) = passkey_page(&state, &headers).await?;
    let (webauthn, rp_id) = page_webauthn(&state, &headers)?;
    let who = requester(&state, ip, &headers);
    let refuse = |reason: &'static str| {
        let pool = pool.clone();
        let who = &who;
        async move {
            refused_event(&pool, page, who, reason).await;
            PasskeyError::Refused
        }
    };
    let Some(taken) = passkeys::take_state(&pool, page, &body.handle).await? else {
        return Err(refuse("passkey_spent").await);
    };
    if taken.viewer.is_some() {
        return Err(refuse("registration_handle").await);
    }
    let ceremony: DiscoverableAuthentication = taken.decode()?;
    let credential: PublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| PasskeyError::Refused)?;
    let Ok((claimed, cred_id)) = webauthn.identify_discoverable_authentication(&credential) else {
        return Err(refuse("unidentifiable_credential").await);
    };
    let owner = passkeys::owner_of(&pool, page, cred_id, &rp_id).await?;
    if owner != Some(claimed) {
        return Err(refuse("not_this_page").await);
    }
    let Some(viewer) = viewers::get(&pool, page, claimed).await? else {
        return Err(refuse("no_viewer").await);
    };
    if viewer.blocked_at.is_some() {
        return Err(refuse("blocked").await);
    }
    if !access.admits(&viewer.email) {
        return Err(refuse("not_listed").await);
    }
    let held = passkeys::for_viewer(&pool, page, viewer.id, &rp_id).await?;
    let discoverable: Vec<DiscoverableKey> = held.iter().map(DiscoverableKey::from).collect();
    let result =
        match webauthn.finish_discoverable_authentication(&credential, ceremony, &discoverable) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "page passkey assertion rejected");
                return Err(refuse("assertion_rejected").await);
            }
        };
    // Checked once the passkey is proven, so the answer cannot be probed for.
    if let Some(days) = access.settings.recheck_days
        && viewer.email_verified_at < chrono::Utc::now() - chrono::Duration::days(i64::from(days))
    {
        refused_event(&pool, page, &who, "recheck_due").await;
        return Err(PasskeyError::Recheck);
    }
    if let Some(mut used) = held.into_iter().find(|p| p.cred_id() == result.cred_id()) {
        used.update_credential(&result);
        if let Err(e) = passkeys::record_use(&pool, page, &used).await {
            tracing::warn!(error = %e, "page passkey use not written back");
        }
    }
    open_session(
        &state,
        &headers,
        &cookies,
        page,
        &access,
        viewer.id,
        &who,
        AccessEventMethod::Passkey,
    )
    .await
    .map_err(|_| PasskeyError::Internal)?;
    events::record_quietly(
        &pool,
        NewEvent {
            viewer: Some(viewer.id),
            method: Some(AccessEventMethod::Passkey),
            ip_hash: who.ip_hash.as_deref(),
            user_agent_hash: who.ua_hash.as_deref(),
            ..NewEvent::new(page, AccessEventKind::SignedIn)
        },
    )
    .await;
    let home = status_home(&state, &headers);
    Ok(Json(SignedIn {
        redirect: safe_next(&body.next, home),
    }))
}
