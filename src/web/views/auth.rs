//! Server-rendered authentication page: `/login`.
//!
//! The page does not perform any auth itself — the login button hands off to
//! `/auth/github/login`. It preserves `redirect_after` and `invitation` query
//! params so a bookmarked invitation link survives the OAuth dance.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::app::AppState;
use crate::auth::url::safe_redirect_target;
use crate::request::auth::Session;
use crate::templates::filters;

/// Sentinel matched against `nav` in base.html so the header doesn't render
/// "Dashboard" / "Targets" links on the bare login page.
const TAB_LOGIN: &str = "login";
const TAB_SETTINGS: &str = "settings";
const TAB_USAGE: &str = "usage";
const TAB_ACCOUNT: &str = "account";

#[derive(Debug, Default, Deserialize)]
pub struct LoginQuery {
    pub redirect_after: Option<String>,
    pub invitation: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/login.html")]
pub struct LoginPage {
    pub active_tab: &'static str,
    pub github_enabled: bool,
    pub github_url: String,
    pub google_enabled: bool,
    pub google_url: String,
    pub microsoft_enabled: bool,
    pub microsoft_url: String,
    pub gitlab_enabled: bool,
    pub gitlab_url: String,
    pub passkey_enabled: bool,
    pub magic_link_enabled: bool,
    /// With signup open the email form stops hedging about who gets a link.
    pub open_signup: bool,
    pub magic_link_expiry_minutes: u32,
    /// Marks the provider this browser used last (a returning-visitor cue, not
    /// an auth signal). At most one is true; the cookie carries the method.
    pub last_github: bool,
    pub last_google: bool,
    pub last_microsoft: bool,
    pub last_gitlab: bool,
    pub last_passkey: bool,
    pub last_magic: bool,
    pub invitation_hint: Option<String>,
    /// Cached, timeout-bounded `target_store.ping()` — same dependency check
    /// as `/readyz`, non-sensitive (no tenant scope). See [`login_ready`].
    pub ready: bool,
    /// Umami website id, or `None` on self-hosted and dev.
    pub analytics: Option<&'static str>,
}

/// Start-URL with the carried-through login params (redirect_after, invitation).
fn login_url(base: &str, params: &[(&str, String)]) -> String {
    let mut qs = String::new();
    for (k, v) in params {
        crate::auth::url::push_param(&mut qs, k, v);
    }
    if qs.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{qs}")
    }
}

pub async fn login(
    State(state): State<AppState>,
    Query(q): Query<LoginQuery>,
    cookies: tower_cookies::Cookies,
    session: Session,
) -> Response {
    // Someone signed in who named a destination is not here to sign in. Bare
    // `/login` still renders, so a second account is still reachable, and an
    // invitation keeps its own accept flow.
    if session.user_id().is_some()
        && q.invitation.is_none()
        && let Some(target) = q.redirect_after.as_deref().and_then(safe_redirect_target)
    {
        return Redirect::to(target).into_response();
    }

    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(r) = q.redirect_after.as_deref().and_then(safe_redirect_target) {
        params.push(("redirect_after", r.to_string()));
    }
    if let Some(inv) = q.invitation.as_deref() {
        params.push(("invitation", inv.to_string()));
    }

    use crate::auth::login_audit::LoginMethod;
    let last = crate::request::login_hint::get(&cookies);
    let last = last.as_deref();

    LoginPage {
        active_tab: TAB_LOGIN,
        github_enabled: state.cfg.auth.github_login_enabled(),
        github_url: login_url("/auth/github/login", &params),
        google_enabled: state.cfg.auth.google_login_enabled(),
        google_url: login_url("/auth/google/login", &params),
        microsoft_enabled: state.cfg.auth.microsoft_login_enabled(),
        microsoft_url: login_url("/auth/microsoft/login", &params),
        gitlab_enabled: state.cfg.auth.gitlab_login_enabled(),
        gitlab_url: login_url("/auth/gitlab/login", &params),
        // DB-gated too: without Postgres the request handler can only 500,
        // so a self-host/in-mem deployment must not render the form.
        // Same shape as magic link: the ceremony writes rows, so an in-memory
        // deployment must not offer a button that cannot finish.
        passkey_enabled: crate::auth::passkey::login_enabled(&state.cfg.auth) && state.db.is_some(),
        magic_link_enabled: state.cfg.auth.magic_link_enabled() && state.db.is_some(),
        open_signup: state.cfg.auth.open_signup_enabled() && state.db.is_some(),
        magic_link_expiry_minutes: state.cfg.auth.magic_link.expiry_minutes,
        last_github: last == Some(LoginMethod::GithubOauth.as_db_str()),
        last_google: last == Some(LoginMethod::GoogleOauth.as_db_str()),
        last_microsoft: last == Some(LoginMethod::MicrosoftOauth.as_db_str()),
        last_gitlab: last == Some(LoginMethod::GitlabOauth.as_db_str()),
        last_passkey: last == Some(LoginMethod::Passkey.as_db_str()),
        last_magic: last == Some(LoginMethod::MagicLink.as_db_str()),
        invitation_hint: q.invitation,
        ready: login_ready(&state).await,
        analytics: crate::analytics::website_id(&state.cfg.auth.public_base_url),
    }
    .into_response()
}

/// Process-global cached readiness for the (public, unauthenticated) login
/// pill. `/login` is internet-facing and unthrottled at the edge for an
/// anonymous visitor, so probing PG on *every* hit turns a cheap GET into a
/// pool-acquire amplification lever against a small connection pool. One PG
/// pool ⇒ readiness is process-wide, so a short TTL collapses a flood into
/// ≤1 probe per [`READINESS_TTL_MS`], and the probe is timeout-bounded so a
/// saturated/slow backend degrades the pill instead of hanging the page an
/// operator needs most exactly when the backend is unwell.
async fn login_ready(state: &AppState) -> bool {
    static CACHE: std::sync::OnceLock<ReadinessCache> = std::sync::OnceLock::new();
    static MONO_START: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    const READINESS_TTL_MS: u64 = 5_000;
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

    struct ReadinessCache {
        ready: std::sync::atomic::AtomicBool,
        // 0 = never probed (forces a first probe); else ms since MONO_START.
        checked_at_ms: std::sync::atomic::AtomicU64,
    }
    use std::sync::atomic::Ordering::Relaxed;

    let cache = CACHE.get_or_init(|| ReadinessCache {
        ready: std::sync::atomic::AtomicBool::new(false),
        checked_at_ms: std::sync::atomic::AtomicU64::new(0),
    });
    let now = (MONO_START.elapsed().as_millis() as u64).max(1);
    let last = cache.checked_at_ms.load(Relaxed);
    if last != 0 && now.saturating_sub(last) < READINESS_TTL_MS {
        return cache.ready.load(Relaxed);
    }

    // A few requests may race here at expiry and each probe once — bounded
    // by TTL, not per-request, which is the property that matters.
    let ready = tokio::time::timeout(PROBE_TIMEOUT, state.target_store.ping())
        .await
        .is_ok_and(|r| r.is_ok());
    cache.ready.store(ready, Relaxed);
    cache.checked_at_ms.store(now, Relaxed);
    ready
}

/// Renders the bare header: an account every other route treats as deleted has
/// no org context to hang a nav on.
const TAB_RECOVER: &str = "recover";

#[derive(Template, WebTemplate)]
#[template(path = "auth/restore.html")]
pub struct RestorePage {
    pub active_tab: &'static str,
    pub purge_at: chrono::DateTime<chrono::Utc>,
}

/// Where a sign-in on a soft-deleted account lands. Cancelling the deletion is
/// a second, deliberate click; the alternative is a deletion nobody can
/// complete, because the only way back in undoes it.
pub async fn restore_page(
    State(state): State<AppState>,
    pending: Option<crate::request::PendingDeletionUser>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(pending) = pending else {
        return axum::response::Redirect::to("/login").into_response();
    };
    let grace = i64::from(state.cfg.tenancy.deletion_grace_period_days);
    RestorePage {
        active_tab: TAB_RECOVER,
        purge_at: pending.deleted_at + chrono::Duration::days(grace),
    }
    .into_response()
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/deleted.html")]
pub struct DeletedPage {
    pub active_tab: &'static str,
    /// `None` once the receipt cookie expires; the copy drops the date rather
    /// than inventing one.
    pub purge_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Signed-out confirmation that the deletion landed, so the flow ends
/// somewhere other than a login form.
pub async fn deleted_page(
    State(state): State<AppState>,
    cookies: tower_cookies::Cookies,
) -> DeletedPage {
    DeletedPage {
        active_tab: TAB_RECOVER,
        purge_at: crate::request::deletion_receipt::take(
            &cookies,
            &state.cfg.auth.session.cookie_domain,
        ),
    }
}

pub mod settings {
    use askama::Template;
    use askama_web::WebTemplate;
    use axum::extract::State;
    use axum::response::{IntoResponse, Redirect, Response};

    use crate::app::AppState;
    use crate::auth::{account, api_tokens, session as session_store};
    use crate::error::AppError;
    use crate::request::auth::{CurrentOrg, Session};
    use crate::storage::orgs::list_orgs_for_user;
    use crate::templates::filters;
    use crate::web::error::WebResult;
    use crate::web::views::resolve_org;

    use super::{TAB_ACCOUNT, TAB_SETTINGS, TAB_USAGE};

    #[derive(Template, WebTemplate)]
    #[template(path = "settings/sessions.html")]
    pub struct SessionsPage {
        pub active_tab: &'static str,
    }

    pub struct OrgOption {
        pub slug: String,
        pub name: String,
        pub selected: bool,
    }

    #[derive(Template, WebTemplate)]
    #[template(path = "settings/api_tokens.html")]
    pub struct ApiTokensPage {
        pub active_tab: &'static str,
        pub orgs: Vec<OrgOption>,
    }

    pub struct SessionRow {
        /// SHA-256 hex of the cookie. Surfaced into the revoke form URL —
        /// safe because the cookie's 256-bit pre-image can't be derived.
        pub id_hash: String,
        pub created: chrono::DateTime<chrono::Utc>,
        pub last_used: chrono::DateTime<chrono::Utc>,
        pub expires: chrono::DateTime<chrono::Utc>,
        pub ip_short: String,
        pub is_current: bool,
    }

    pub struct TokenRow {
        pub id: String,
        pub name: String,
        pub prefix: String,
        pub created: chrono::DateTime<chrono::Utc>,
        pub last_used: Option<chrono::DateTime<chrono::Utc>>,
        pub expires: Option<chrono::DateTime<chrono::Utc>>,
        pub expired: bool,
        pub access: &'static str,
        /// Sorted scope list for the access-cell tooltip.
        pub scopes: String,
        pub org: Option<String>,
    }

    #[derive(Template, WebTemplate)]
    #[template(path = "settings/sessions_partial.html")]
    pub struct SessionsPartial {
        pub sessions: Vec<SessionRow>,
    }

    #[derive(Template, WebTemplate)]
    #[template(path = "settings/api_tokens_partial.html")]
    pub struct TokensPartial {
        pub tokens: Vec<TokenRow>,
    }

    pub async fn sessions_page(session: Session) -> Response {
        if session.user.is_none() {
            return crate::request::auth::login_redirect("/settings/sessions").into_response();
        }
        SessionsPage {
            active_tab: TAB_SETTINGS,
        }
        .into_response()
    }

    pub async fn api_tokens_page(
        State(state): State<AppState>,
        session: Session,
    ) -> WebResult<Response> {
        let Some(user) = session.user.as_ref() else {
            return Ok(crate::request::auth::login_redirect("/settings/api-tokens").into_response());
        };
        let pool = state.require_db()?;
        let mut orgs: Vec<OrgOption> = list_orgs_for_user(pool, user.id)
            .await?
            .into_iter()
            .map(|o| OrgOption {
                selected: Some(o.org.id) == session.active_org_id,
                slug: o.org.slug,
                name: o.org.name,
            })
            .collect();
        // Bind-by-default: if the active org didn't match (or none is set), pin
        // the dropdown to the first org rather than letting the browser pick it.
        if !orgs.iter().any(|o| o.selected)
            && let Some(first) = orgs.first_mut()
        {
            first.selected = true;
        }
        Ok(ApiTokensPage {
            active_tab: TAB_SETTINGS,
            orgs,
        }
        .into_response())
    }

    pub struct IdentityRow {
        pub provider: &'static str,
        pub label: &'static str,
        pub provider_user_id: String,
        pub username: Option<String>,
        pub added: chrono::DateTime<chrono::Utc>,
        pub last_login: chrono::DateTime<chrono::Utc>,
        /// False when removing it would leave nobody able to sign in.
        pub removable: bool,
        /// Starts a dance for a second account at this same vendor.
        pub add_another_url: String,
    }

    pub struct PasskeyRow {
        pub id: String,
        pub nickname: Option<String>,
        pub added: chrono::DateTime<chrono::Utc>,
        pub last_used: chrono::DateTime<chrono::Utc>,
        /// False when removing it would leave nobody able to sign in.
        pub removable: bool,
        /// A dead credential still lists: a row nobody can explain is worse
        /// than a labelled one.
        pub usable: bool,
    }

    /// A provider this deployment offers that the account has not linked yet.
    pub struct LinkOption {
        pub label: &'static str,
        pub url: String,
    }

    #[derive(Template, WebTemplate)]
    #[template(path = "settings/account.html")]
    pub struct AccountPage {
        pub active_tab: &'static str,
        pub email: String,
        pub identities: Vec<IdentityRow>,
        pub passkeys: Vec<PasskeyRow>,
        pub passkeys_enabled: bool,
        pub linkable: Vec<LinkOption>,
        pub linked: Option<&'static str>,
        /// The provider account offered already opens somebody else's account.
        pub taken: bool,
        /// That provider was already on the account.
        pub already_linked: bool,
        /// The provider could not be reached, so nothing changed.
        pub link_failed: bool,
        pub joined: Option<chrono::DateTime<chrono::Utc>>,
        pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
        pub theme: String,
        pub time_format: String,
        pub grace_days: u32,
    }

    /// `GET /settings/account`. An unauthenticated hit redirects to login
    /// (matching the other settings pages); a DB error surfaces as the 5xx
    /// page. Export and delete are driven from the page against the API —
    /// this handler only renders the chrome plus the read-only overview.
    pub async fn account_page(
        State(state): State<AppState>,
        session: Session,
        cookies: tower_cookies::Cookies,
    ) -> WebResult<Response> {
        let Some(user) = session.user.clone() else {
            return Ok(crate::request::auth::login_redirect("/settings/account").into_response());
        };
        let pool = state.require_db()?;
        // No data dependency between the three, so they go together.
        let (facts, linked, prefs, stored_passkeys) = tokio::try_join!(
            account::account_facts(pool, user.id),
            crate::storage::oauth_identities::list_for_user(pool, user.id),
            crate::storage::users::get_display_prefs(pool, user.id),
            crate::storage::passkeys::list_for_user(pool, user.id),
        )?;
        let (joined, last_seen) = match facts {
            Some(f) => (Some(f.created_at), f.last_seen_at),
            None => (None, None),
        };
        // The same question the API will ask. Looser offers a button that
        // 400s; stricter hides one from a user whose only provider is
        // compromised.
        let ways_in = crate::auth::ways_in(&state.cfg);
        // A dead credential must not hold a removal open.
        let rp_id = crate::auth::passkey::relying_party_id(&state.cfg.auth.public_base_url).ok();
        let usable_passkeys = rp_id
            .as_deref()
            .filter(|_| crate::auth::passkey::login_enabled(&state.cfg.auth))
            .map_or(0, |rp| {
                stored_passkeys
                    .iter()
                    .filter(|row| row.usable_from(rp))
                    .count()
            });
        let identities: Vec<IdentityRow> = linked
            .iter()
            .filter_map(|row| {
                let p = crate::domain::OauthProvider::from_db_str(&row.provider)?;
                Some(IdentityRow {
                    provider: p.as_db_str(),
                    label: p.label(),
                    provider_user_id: row.provider_user_id.clone(),
                    username: row.provider_username.clone(),
                    added: row.created_at,
                    last_login: row.last_login_at,
                    removable: ways_in.removable(row, &linked, usable_passkeys),
                    add_another_url: format!("/auth/{}/link", p.as_db_str()),
                })
            })
            .collect();
        let passkeys: Vec<PasskeyRow> = stored_passkeys
            .iter()
            .map(|row| {
                let usable = rp_id.as_deref().is_some_and(|rp| row.usable_from(rp));
                PasskeyRow {
                    id: row.id.to_string(),
                    nickname: row.nickname.clone(),
                    added: row.created_at,
                    last_used: row.last_used_at,
                    // The question the API will ask. A row that was never a
                    // way in does not take one away by leaving.
                    removable: ways_in.passkey_removable(
                        &linked,
                        if usable {
                            usable_passkeys.saturating_sub(1)
                        } else {
                            usable_passkeys
                        },
                    ),
                    usable,
                }
            })
            .collect();
        let linkable = link_options(&state, &identities);
        let flash = take_link_flash(&cookies, &state);
        Ok(AccountPage {
            active_tab: TAB_ACCOUNT,
            email: user.email,
            identities,
            passkeys,
            passkeys_enabled: crate::auth::passkey::login_enabled(&state.cfg.auth),
            linkable,
            linked: flash.identity_linked.map(|p| p.label()),
            taken: flash.identity_taken,
            already_linked: flash.identity_already_linked,
            link_failed: flash.link_failed,
            joined,
            last_seen,
            theme: prefs.theme.as_str().to_string(),
            time_format: prefs.time_format.as_str().to_string(),
            grace_days: state.cfg.tenancy.deletion_grace_period_days,
        }
        .into_response())
    }

    /// `flash::take` clears the whole cookie, so anything this page does not
    /// render gets staged again — otherwise a `restored` banner bound for the
    /// dashboard dies on a detour through account settings.
    fn take_link_flash(
        cookies: &tower_cookies::Cookies,
        state: &AppState,
    ) -> crate::request::flash::Flash {
        let domain = &state.cfg.auth.session.cookie_domain;
        let flash = crate::request::flash::take(cookies, domain);
        let carried = crate::request::flash::Flash {
            restored: flash.restored,
            invite_missed: flash.invite_missed,
            ..Default::default()
        };
        crate::request::flash::set(
            cookies,
            &carried,
            state.cfg.auth.session.cookie_secure,
            domain,
        );
        flash
    }

    /// Only providers with nothing linked yet. A second account at the same
    /// vendor is reachable too, from that vendor's own row — filtering here
    /// without that would make it UI-unreachable.
    fn link_options(state: &AppState, linked: &[IdentityRow]) -> Vec<LinkOption> {
        state
            .cfg
            .auth
            .enabled_login_providers()
            .into_iter()
            .filter(|p| !linked.iter().any(|row| row.provider == p.as_db_str()))
            .map(|p| LinkOption {
                label: p.label(),
                url: format!("/auth/{}/link", p.as_db_str()),
            })
            .collect()
    }

    pub async fn sessions_partial(
        State(state): State<AppState>,
        session: Session,
    ) -> WebResult<Response> {
        let Some(user) = session.user.as_ref() else {
            return Ok(Redirect::to("/login").into_response());
        };
        let pool = state.require_db()?;
        let rows = session_store::list_for_user(pool, user.id).await?;
        let current = session.session_id_hash.as_deref();
        let sessions = rows
            .into_iter()
            .map(|r| SessionRow {
                ip_short: short_hash(r.ip_hash.as_deref()),
                created: r.created_at,
                last_used: r.last_used_at,
                expires: r.expires_at,
                is_current: Some(r.id_hash.as_str()) == current,
                id_hash: r.id_hash,
            })
            .collect();
        Ok(SessionsPartial { sessions }.into_response())
    }

    pub async fn api_tokens_partial(
        State(state): State<AppState>,
        session: Session,
    ) -> WebResult<Response> {
        let Some(user) = session.user.as_ref() else {
            return Ok(Redirect::to("/login").into_response());
        };
        let pool = state.require_db()?;
        let now = chrono::Utc::now();
        let rows = api_tokens::list_for_user(pool, user.id).await?;
        let tokens = rows
            .into_iter()
            .map(|r| TokenRow {
                id: r.id.to_string(),
                name: r.name,
                prefix: r.token_prefix,
                created: r.created_at,
                last_used: r.last_used_at,
                expired: r.expires_at.is_some_and(|e| e <= now),
                expires: r.expires_at,
                access: access_label(&r.scopes.0),
                scopes: {
                    let mut s = r.scopes.0.clone();
                    s.sort();
                    s.join(", ")
                },
                org: r.org_slug,
            })
            .collect();
        Ok(TokensPartial { tokens }.into_response())
    }

    /// Only an exact all-resources `:read` / `:write` set earns a preset word;
    /// any narrower Advanced grant is `custom` (exact scopes in the tooltip).
    fn access_label(scopes: &[String]) -> &'static str {
        use crate::auth::scope::Scope;
        use std::collections::HashSet;

        if scopes.iter().any(|s| s == Scope::FullAccess.as_str()) {
            return "full access";
        }
        const READ_PRESET: [&str; 5] = [
            Scope::TargetsRead.as_str(),
            Scope::ChannelsRead.as_str(),
            Scope::IncidentsRead.as_str(),
            Scope::MaintenanceRead.as_str(),
            Scope::StatusPageRead.as_str(),
        ];
        const WRITE_PRESET: [&str; 5] = [
            Scope::TargetsWrite.as_str(),
            Scope::ChannelsWrite.as_str(),
            Scope::IncidentsWrite.as_str(),
            Scope::MaintenanceWrite.as_str(),
            Scope::StatusPageWrite.as_str(),
        ];
        let have: HashSet<&str> = scopes.iter().map(String::as_str).collect();
        let is_preset =
            |preset: &[&str]| have.len() == preset.len() && preset.iter().all(|s| have.contains(s));

        if is_preset(&READ_PRESET) {
            "read-only"
        } else if is_preset(&WRITE_PRESET) {
            "read & write"
        } else {
            "custom"
        }
    }

    /// Compact binary-unit label for help text (e.g. `1 MB`, `512 KB`).
    /// Floors to the largest whole unit; operator-set limits are expected
    /// round, so the floor is exact in practice.
    fn human_bytes(b: u64) -> String {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * KB;
        if b >= MB {
            format!("{} MB", b / MB)
        } else if b >= KB {
            format!("{} KB", b / KB)
        } else {
            format!("{b} B")
        }
    }

    /// Hard cap on rendered quota cells. One cell per unit up to this; a
    /// larger plan falls back to a proportional `CELL_CAP`-wide meter so the
    /// row stays legible instead of rendering hundreds of hairline cells.
    const CELL_CAP: i64 = 64;

    /// `cells[i] == true` for the first `round(pct% × total)` cells — the
    /// proportion-filled flags the segmented meter iterates.
    fn filled_flags(pct: i64, total: i64) -> Vec<bool> {
        let on = ((pct * total + 50) / 100).clamp(0, total);
        (0..total).map(|i| i < on).collect()
    }

    /// One cell per unit of `limit`, the first `current` filled — so the
    /// user can literally count cells against the cap. Falls back to a
    /// proportional `CELL_CAP`-wide meter once the limit exceeds the cap.
    fn quota_cells(current: i64, limit: i64, pct: i64) -> Vec<bool> {
        if limit <= 0 {
            Vec::new()
        } else if limit <= CELL_CAP {
            let on = current.clamp(0, limit);
            (0..limit).map(|i| i < on).collect()
        } else {
            filled_flags(pct, CELL_CAP)
        }
    }

    /// One quota row. `pct` is pre-clamped 0–100 in Rust so the template
    /// stays logic-free; `limit_display` shows ∞ for the synthetic unlimited
    /// (self-host) plan instead of a meaningless 2.1-billion. `fill_class`
    /// maps pct → ok/warn/bad CSS variant (`<80`, `80–94`, `≥95`).
    /// `unlimited` swaps the segmented meter for an open-ended ∞ rail;
    /// `cells` is the per-segment filled state (empty when unlimited).
    pub struct UsageBar {
        pub label: &'static str,
        pub current: i64,
        pub limit_display: String,
        pub pct: i64,
        pub fill_class: &'static str,
        pub unlimited: bool,
        pub cells: Vec<bool>,
    }

    impl UsageBar {
        fn new(label: &'static str, current: i64, limit: i32) -> Self {
            let unlimited = limit == i32::MAX;
            let limit = i64::from(limit);
            let pct = if unlimited || limit <= 0 {
                0
            } else {
                (current * 100 / limit).clamp(0, 100)
            };
            let fill_class = if unlimited {
                "ok"
            } else {
                match pct {
                    100 => "bad",
                    p if p >= 80 => "warn",
                    _ => "ok",
                }
            };
            Self {
                label,
                current,
                limit_display: if unlimited {
                    "∞".to_string()
                } else {
                    limit.to_string()
                },
                pct,
                fill_class,
                unlimited,
                cells: if unlimited {
                    Vec::new()
                } else {
                    quota_cells(current, limit, pct)
                },
            }
        }
    }

    /// One monitor or page in the keep picker.
    #[derive(Clone)]
    pub struct HoldPick {
        pub id: String,
        pub name: String,
        pub held: bool,
        pub keep: bool,
        /// Answers to the flow cap as well, which the legend counts apart.
        pub flow: bool,
    }

    /// One resource's worth of picker: the pool to choose from and the seats
    /// the plan sells. Flows carry a second, smaller budget inside the first,
    /// since a flow monitor spends a slot in both.
    pub struct HoldSet {
        pub label: &'static str,
        /// The request field this list is saved under, which is what keeps a
        /// picker showing one resource from clearing the other's choice.
        pub field: &'static str,
        pub seats: usize,
        pub kept: usize,
        pub flow_seats: usize,
        pub flow_kept: usize,
        pub has_flows: bool,
        pub held: usize,
        pub rows: Vec<HoldPick>,
    }

    #[derive(Template, WebTemplate)]
    #[template(path = "settings/usage.html")]
    pub struct UsagePage {
        pub active_tab: &'static str,
        pub plan_name: String,
        /// How much the plan is holding, which is what decides whether the
        /// panel appears at all.
        pub held_total: usize,
        /// One per resource that actually has something held. Empty for a
        /// caller who may look but not choose.
        pub holds: Vec<HoldSet>,
        /// The caller is in the account but does not own it, so the choice is
        /// not theirs to make. Saying so beats a form that can only 403.
        pub holds_locked: bool,
        /// Too many rows to render a checkbox each. A cut list cannot express
        /// a pick, since every row it left out would read as declined.
        pub holds_too_large: bool,
        pub bars: Vec<UsageBar>,
        pub min_check_interval_secs: i32,
        pub retention_days: i32,
        pub max_logo_size_label: String,
        pub max_api_tokens_per_user: i32,
        pub api_writes_per_minute: i32,
        pub api_reads_per_minute: i32,
        pub bulk_ops_per_minute: i32,
        pub test_now_per_minute: i32,
        pub check_now_per_minute: i32,
    }

    /// `GET /settings/usage`. Auth/redirect behaviour mirrors
    /// `/settings/status-page`: only an *unauthenticated* hit bounces to
    /// login; the org resolves exactly as the API does, so the page and
    /// `GET /api/v1/orgs/{id}/usage` always show the same numbers.
    pub async fn usage_page(
        State(state): State<AppState>,
        org: Result<CurrentOrg, AppError>,
        user: Result<crate::request::CurrentUser, AppError>,
    ) -> WebResult<Response> {
        let org = match resolve_org(org, "/settings/usage") {
            Ok(o) => o,
            Err(resp) => return Ok(*resp),
        };
        let u = state.quotas.account_usage(org).await?;
        let p = &u.plan;
        let holds = match state.db.as_ref() {
            Some(pool) => holds_panel(pool, org, user.ok().map(|c| c.0), p).await?,
            None => HoldsPanel::default(),
        };
        Ok(UsagePage {
            active_tab: TAB_USAGE,
            plan_name: p.name.clone(),
            held_total: holds.held_total,
            holds: holds.sets,
            holds_locked: holds.locked,
            holds_too_large: holds.too_large,
            bars: vec![
                UsageBar::new("organisations", u.orgs, p.max_orgs),
                UsageBar::new("targets", u.targets, p.max_targets),
                UsageBar::new("members", u.members, p.max_members),
                UsageBar::new(
                    "public-components",
                    u.public_components,
                    p.max_public_components,
                ),
                UsageBar::new(
                    "pending-invitations",
                    u.pending_invitations,
                    p.max_pending_invitations,
                ),
                UsageBar::new(
                    "maintenance-windows",
                    u.maintenance_windows,
                    p.max_maintenance_windows,
                ),
                UsageBar::new(
                    "notification-channels",
                    u.notification_channels,
                    p.max_notification_channels,
                ),
            ],
            min_check_interval_secs: p.min_check_interval_secs,
            retention_days: p.retention_days,
            max_logo_size_label: human_bytes(u64::try_from(p.max_logo_size_bytes).unwrap_or(0)),
            max_api_tokens_per_user: p.max_api_tokens_per_user,
            api_writes_per_minute: p.api_writes_per_minute,
            api_reads_per_minute: p.api_reads_per_minute,
            bulk_ops_per_minute: p.bulk_ops_per_minute,
            test_now_per_minute: p.test_now_per_minute,
            check_now_per_minute: p.check_now_per_minute,
        }
        .into_response())
    }

    #[derive(Default)]
    struct HoldsPanel {
        held_total: usize,
        sets: Vec<HoldSet>,
        locked: bool,
        too_large: bool,
    }

    /// The account's pool, shaped for the picker. Read only when something is
    /// actually held, so the panel and its whole-pool scan cost nothing on the
    /// overwhelming majority of accounts that fit their plan.
    ///
    /// The pool spans every org the account owns, because that is what the
    /// caps are pooled across — and monitor names from a sibling org are not
    /// something an ordinary member of this one may read. Only the account
    /// owner is shown the rows, which is the same line
    /// `/api/v1/account/holds` draws.
    async fn holds_panel(
        pool: &sqlx::PgPool,
        org: crate::domain::OrgId,
        user: Option<crate::domain::UserId>,
        plan: &crate::domain::Plan,
    ) -> Result<HoldsPanel, AppError> {
        let account = crate::storage::accounts::account_for_org(pool, org).await?;
        if !crate::quotas::holds::holds_anything(pool, account).await? {
            return Ok(HoldsPanel::default());
        }
        let owner = match user {
            Some(u) => crate::storage::accounts::account_for_user(pool, u).await? == Some(account),
            None => false,
        };
        let (targets, pages) = crate::quotas::holds::list_pool(pool, account).await?;
        let held_total =
            targets.iter().filter(|r| r.held).count() + pages.iter().filter(|r| r.held).count();
        if !owner {
            return Ok(HoldsPanel {
                held_total,
                locked: true,
                ..HoldsPanel::default()
            });
        }
        if targets.len() > crate::quotas::holds::MAX_PICKER_ROWS
            || pages.len() > crate::quotas::holds::MAX_PICKER_ROWS
        {
            return Ok(HoldsPanel {
                held_total,
                too_large: true,
                ..HoldsPanel::default()
            });
        }

        let mut sets = Vec::new();
        for (label, field, cap, flow_cap, rows) in [
            (
                "monitors",
                "keep_monitors",
                plan.max_targets,
                plan.max_flow_checks,
                targets,
            ),
            (
                "status pages",
                "keep_status_pages",
                plan.max_status_pages,
                0,
                pages,
            ),
        ] {
            // A resource the plan still covers has nothing to decide, and a
            // full pool of ticked boxes beside the one that does only invites
            // an accidental save.
            if !rows.iter().any(|r| r.held) {
                continue;
            }
            sets.push(HoldSet::new(label, field, cap, flow_cap, rows));
        }
        Ok(HoldsPanel {
            held_total,
            sets,
            ..HoldsPanel::default()
        })
    }

    impl HoldSet {
        fn new(
            label: &'static str,
            field: &'static str,
            cap: i32,
            flow_cap: i32,
            rows: Vec<crate::quotas::holds::PoolRow>,
        ) -> Self {
            // Whether the customer has answered at all. Before they do, the
            // running set is the answer; after, their own choice is, including
            // the parts of it the plan could not honour — showing the running
            // set instead would drop those on the next save.
            let picked = rows.iter().any(|r| r.kept);
            let rows: Vec<HoldPick> = rows
                .into_iter()
                .map(|r| HoldPick {
                    id: r.id.to_string(),
                    name: r.name,
                    held: r.held,
                    keep: if picked { r.kept } else { !r.held },
                    flow: r.is_flow,
                })
                .collect();
            let flows = rows.iter().filter(|r| r.flow).count();
            Self {
                label,
                field,
                // Never more seats than there are rows to fill them: a pool
                // smaller than the cap asks for everything, not the impossible.
                seats: usize::try_from(cap).unwrap_or(0).min(rows.len()),
                kept: rows.iter().filter(|r| r.keep).count(),
                flow_seats: usize::try_from(flow_cap).unwrap_or(0).min(flows),
                flow_kept: rows.iter().filter(|r| r.flow && r.keep).count(),
                has_flows: flows > 0,
                held: rows.iter().filter(|r| r.held).count(),
                rows,
            }
        }
    }

    fn short_hash(h: Option<&str>) -> String {
        match h {
            Some(s) if s.len() >= 12 => s[..12].to_string(),
            Some(s) => s.to_string(),
            None => "—".to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn set(
            label: &'static str,
            field: &'static str,
            seats: usize,
            rows: Vec<HoldPick>,
        ) -> HoldSet {
            HoldSet {
                label,
                field,
                seats,
                kept: rows.iter().filter(|r| r.keep).count(),
                flow_seats: 0,
                flow_kept: 0,
                has_flows: false,
                held: rows.iter().filter(|r| r.held).count(),
                rows,
            }
        }

        fn usage_html(sets: Vec<HoldSet>) -> String {
            let held_total = sets.iter().map(|s| s.held).sum();
            UsagePage {
                active_tab: super::super::TAB_USAGE,
                plan_name: "Free".into(),
                held_total,
                holds: sets,
                holds_locked: false,
                holds_too_large: false,
                bars: vec![UsageBar::new("Targets", 7, 10)],
                min_check_interval_secs: 60,
                retention_days: 90,
                max_logo_size_label: "1 MB".into(),
                max_api_tokens_per_user: 5,
                api_writes_per_minute: 600,
                api_reads_per_minute: 6000,
                bulk_ops_per_minute: 30,
                test_now_per_minute: 60,
                check_now_per_minute: 60,
            }
            .render()
            .expect("render")
        }

        fn pick(name: &str, held: bool) -> HoldPick {
            HoldPick {
                id: "44444444-4444-4444-4444-444444444444".into(),
                name: name.into(),
                held,
                keep: !held,
                flow: false,
            }
        }

        #[test]
        fn an_account_that_fits_its_plan_is_shown_no_holds_panel() {
            let html = usage_html(Vec::new());
            assert!(!html.contains("held by plan"));
            assert!(
                !html.contains("holds_form.js"),
                "the picker's script is not even fetched when there is nothing to pick"
            );
        }

        #[test]
        fn a_held_monitor_puts_the_picker_on_the_page() {
            let html = usage_html(vec![set(
                "monitors",
                "keep_monitors",
                10,
                vec![pick("api", true), pick("web", false)],
            )]);
            assert!(html.contains("held by plan"));
            assert!(html.contains("holds_form.js"));
            // The picker lists the whole pool, not only what is held: keeping a
            // held monitor means giving up a running one, so both must be
            // visible and tickable together.
            assert!(html.contains("api") && html.contains("web"));
            assert!(html.contains("save what to keep"));
        }

        #[test]
        fn each_resource_saves_under_its_own_field() {
            // A picker showing only status pages must not send an empty
            // monitor list, which would clear a pick made in an earlier
            // shortage.
            let html = usage_html(vec![set(
                "status pages",
                "keep_status_pages",
                1,
                vec![pick("status", true), pick("ops", false)],
            )]);
            assert!(html.contains(r#"data-holds-field="keep_status_pages""#));
            assert!(
                !html.contains("keep_monitors"),
                "a resource with nothing held gets no list and sends no answer"
            );
        }

        #[test]
        fn the_picker_counts_the_seats_the_plan_sells() {
            let html = usage_html(vec![set(
                "monitors",
                "keep_monitors",
                1,
                vec![pick("api", true), pick("web", false)],
            )]);
            assert!(
                html.contains("1 of 1 kept"),
                "the ticked count and the seats both belong in the legend: {html}"
            );
        }

        #[test]
        fn a_member_who_does_not_own_the_account_is_told_rather_than_shown() {
            let html = UsagePage {
                active_tab: super::super::TAB_USAGE,
                plan_name: "Free".into(),
                held_total: 3,
                holds: Vec::new(),
                holds_locked: true,
                holds_too_large: false,
                bars: vec![UsageBar::new("Targets", 7, 10)],
                min_check_interval_secs: 60,
                retention_days: 90,
                max_logo_size_label: "1 MB".into(),
                max_api_tokens_per_user: 5,
                api_writes_per_minute: 600,
                api_reads_per_minute: 6000,
                bulk_ops_per_minute: 30,
                test_now_per_minute: 60,
                check_now_per_minute: 60,
            }
            .render()
            .expect("render");
            assert!(html.contains("held by plan"), "the count is not a secret");
            assert!(
                !html.contains("data-holds-form") && !html.contains("holds_form.js"),
                "a sibling org's monitor names stay out of a non-owner's page"
            );
        }

        #[test]
        fn usage_page_renders_progress_bars_and_contact_link() {
            let html = UsagePage {
                active_tab: super::super::TAB_USAGE,
                plan_name: "Free".into(),
                held_total: 0,
                holds: Vec::new(),
                holds_locked: false,
                holds_too_large: false,
                bars: vec![
                    UsageBar::new("Targets", 7, 10),
                    UsageBar::new("Members", 1, i32::MAX),
                ],
                min_check_interval_secs: 60,
                retention_days: 90,
                max_logo_size_label: "1 MB".into(),
                max_api_tokens_per_user: 5,
                api_writes_per_minute: 600,
                api_reads_per_minute: 6000,
                bulk_ops_per_minute: 30,
                test_now_per_minute: 60,
                check_now_per_minute: 60,
            }
            .render()
            .unwrap();
            assert!(html.starts_with("<!doctype html>"));
            // Plan rendered as an ink token in the header.
            assert!(html.contains(r#"class="usage-plan">plan:free"#));
            // Bounded quota: the figures, the progressbar value, and at
            // least one filled cell in the segmented meter.
            assert!(html.contains(r#"usage-grid__fig">7</span>"#));
            assert!(html.contains(r#"usage-grid__fig">10</span>"#));
            assert!(html.contains(r#"aria-valuenow="70""#));
            assert!(html.contains("usage-cells__c--on-"));
            // Unlimited (self-host) cap renders ∞ + a single empty cell.
            assert!(html.contains(r#"usage-grid__fig">∞</span>"#));
            assert!(html.contains(r#"aria-label="unlimited""#));
            assert!(html.contains("60s"));
            assert!(html.contains(r#"href="mailto:support@uptimepage.dev""#));
        }

        fn account_page(identities: Vec<IdentityRow>, linkable: Vec<LinkOption>) -> AccountPage {
            account_page_with(identities, linkable, Vec::new())
        }

        fn account_page_with(
            identities: Vec<IdentityRow>,
            linkable: Vec<LinkOption>,
            passkeys: Vec<PasskeyRow>,
        ) -> AccountPage {
            AccountPage {
                active_tab: super::super::TAB_ACCOUNT,
                email: "alice@example.com".into(),
                identities,
                passkeys,
                passkeys_enabled: true,
                linkable,
                linked: None,
                taken: false,
                already_linked: false,
                link_failed: false,
                joined: Some("2026-02-14T09:00:00Z".parse().unwrap()),
                last_seen: Some("2026-05-16T12:00:00Z".parse().unwrap()),
                theme: "default".into(),
                time_format: "auto".into(),
                grace_days: 30,
            }
        }

        fn passkey(nickname: &str, removable: bool, usable: bool) -> PasskeyRow {
            PasskeyRow {
                id: "0198f000-0000-7000-8000-000000000001".into(),
                nickname: Some(nickname.into()),
                added: "2026-02-14T09:00:00Z".parse().unwrap(),
                last_used: "2026-05-16T12:00:00Z".parse().unwrap(),
                removable,
                usable,
            }
        }

        #[test]
        fn a_passkey_lists_with_a_way_to_take_it_back() {
            let html = account_page_with(vec![], vec![], vec![passkey("laptop", true, true)])
                .render()
                .unwrap();
            assert!(html.contains("Passkey"), "the row is named");
            assert!(html.contains("laptop"), "the nickname shows");
            assert!(html.contains("data-passkey-remove"), "and can be removed");
        }

        #[test]
        fn the_only_passkey_offers_no_remove_button() {
            let html = account_page_with(vec![], vec![], vec![passkey("phone", false, true)])
                .render()
                .unwrap();
            assert!(!html.contains("data-passkey-remove"), "nothing to press");
            assert!(html.contains("needed to sign in"));
        }

        #[test]
        fn a_credential_for_another_host_says_so_and_still_goes() {
            // Listing it silently reads as a working way in; hiding it leaves
            // a row nobody can account for.
            let html = account_page_with(vec![], vec![], vec![passkey("old", true, false)])
                .render()
                .unwrap();
            assert!(html.contains("cannot sign in"), "labelled as dead");
            assert!(html.contains("data-passkey-remove"), "and removable");
        }

        #[test]
        fn a_switched_off_deployment_draws_no_remove_button() {
            // The button rides a script this page only loads while passkeys
            // are on, so drawing it there gives someone a dead control.
            let mut page = account_page_with(vec![], vec![], vec![passkey("laptop", true, true)]);
            page.passkeys_enabled = false;
            let html = page.render().unwrap();
            assert!(html.contains("laptop"), "the row still lists");
            assert!(
                !html.contains("data-passkey-remove"),
                "but offers no control"
            );
            assert!(
                !html.contains("needed to sign in"),
                "and does not claim it is a way in"
            );
            assert!(html.contains("switched off here"));
        }

        fn identity(provider: &'static str, label: &'static str, removable: bool) -> IdentityRow {
            IdentityRow {
                provider,
                label,
                provider_user_id: format!("{provider}-1"),
                username: Some("alice".into()),
                added: "2026-02-14T09:00:00Z".parse().unwrap(),
                last_login: "2026-05-16T12:00:00Z".parse().unwrap(),
                removable,
                add_another_url: format!("/auth/{provider}/link"),
            }
        }

        #[test]
        fn account_page_lists_every_method_that_opens_the_account() {
            let html = account_page(
                vec![
                    identity("github", "GitHub", true),
                    identity("gitlab", "GitLab", true),
                ],
                vec![LinkOption {
                    label: "Google",
                    url: "/auth/google/link".into(),
                }],
            )
            .render()
            .unwrap();
            assert!(html.contains("sign-in methods"));
            assert!(html.contains(r#"data-provider="github""#));
            assert!(html.contains(r#"data-provider="gitlab""#));
            // POST-driven, so the URL rides a data attribute, not an href.
            assert!(html.contains(r#"data-link-url="/auth/google/link""#));
            assert!(html.contains("add Google"));
        }

        #[test]
        fn removal_warns_about_the_sign_out_it_causes() {
            // Removing a method revokes the account's other sessions. That is
            // the point when the provider is compromised and a surprise when it
            // is not, so the dialog has to say so before the click.
            let html = account_page(vec![identity("github", "GitHub", true)], Vec::new())
                .render()
                .unwrap();
            assert!(html.contains("signed out on your other devices"));
        }

        #[test]
        fn account_page_loads_the_helper_its_script_calls() {
            // smApiErrorMessage lives in api_form.js, which is loaded per page,
            // not app-wide — without it the error path throws a TypeError and
            // the real message never reaches the toast.
            let html = account_page(vec![identity("github", "GitHub", true)], Vec::new())
                .render()
                .unwrap();
            assert!(html.contains("js/ui/api_form"));
            assert!(html.contains("js/ui/sign_in_methods"));
        }

        #[test]
        fn account_page_offers_removal_of_a_lone_method_when_email_is_a_way_back() {
            // The API allows it when magic link is on; a UI that hides the
            // button would force a user whose only provider is compromised to
            // grant a second one a credential on the account first.
            let html = account_page(vec![identity("github", "GitHub", true)], Vec::new())
                .render()
                .unwrap();
            assert!(html.contains("data-identity-remove"));
            assert!(!html.contains("only method"));
        }

        #[test]
        fn account_page_offers_no_removal_of_the_last_method() {
            let html = account_page(vec![identity("github", "GitHub", false)], Vec::new())
                .render()
                .unwrap();
            assert!(html.contains("needed to sign in"));
            assert!(!html.contains("data-identity-remove"));
        }

        #[test]
        fn account_page_renders_privacy_and_sessions_sections() {
            let html = account_page(vec![identity("github", "GitHub", false)], Vec::new())
                .render()
                .unwrap();
            assert!(html.starts_with("<!doctype html>"));
            assert!(html.contains("alice@example.com"));
            assert!(html.contains("GitHub"));
            assert!(html.contains("2026-02-14 09:00 UTC"));
            // Export is a real download link to the API, not an HTMX swap.
            assert!(html.contains(r#"href="/api/v1/me/data-export""#));
            assert!(html.contains("download"));
            // Delete drives the modal, which DELETEs /api/v1/me.
            assert!(html.contains(r#"id="delete-modal""#));
            assert!(html.contains(r#"hx-delete="/api/v1/me""#));
            // Sessions section reuses the shared partial loader (no dup logic).
            assert!(html.contains(r#"hx-get="/web/partials/settings/sessions""#));
            assert!(html.contains("logout-all"));
        }

        #[test]
        fn sessions_page_renders_chrome_and_partial_hook() {
            let html = SessionsPage {
                active_tab: super::super::TAB_SETTINGS,
            }
            .render()
            .unwrap();
            assert!(html.starts_with("<!doctype html>"));
            assert!(html.contains("active sessions"));
            assert!(html.contains(r#"hx-get="/web/partials/settings/sessions""#));
            assert!(html.contains("logout-all"));
        }

        #[test]
        fn api_tokens_page_renders_create_form_and_partial_hook() {
            let html = ApiTokensPage {
                active_tab: super::super::TAB_SETTINGS,
                orgs: vec![OrgOption {
                    slug: "acme".into(),
                    name: "Acme".into(),
                    selected: true,
                }],
            }
            .render()
            .unwrap();
            assert!(html.contains("API tokens"));
            assert!(html.contains(r#"id="new-token-form""#));
            assert!(html.contains("/api/v1/me/api-tokens"));
            assert!(html.contains(r#"<option value="acme" selected>Acme</option>"#));
            assert!(html.contains(r#"hx-get="/web/partials/settings/api-tokens""#));
        }

        #[test]
        fn sessions_partial_renders_empty_state() {
            let html = SessionsPartial { sessions: vec![] }.render().unwrap();
            assert!(html.contains("# no active sessions"));
            // Partial must not include the page chrome — it's swapped in via HTMX.
            assert!(!html.contains("<!doctype html>"));
        }

        #[test]
        fn sessions_partial_marks_current_session() {
            let html = SessionsPartial {
                sessions: vec![SessionRow {
                    id_hash: "abc".into(),
                    created: "2026-05-16T12:00:00Z".parse().unwrap(),
                    last_used: "2026-05-16T12:00:00Z".parse().unwrap(),
                    expires: "2026-06-16T12:00:00Z".parse().unwrap(),
                    ip_short: "deadbeefcafe".into(),
                    is_current: true,
                }],
            }
            .render()
            .unwrap();
            assert!(html.contains("this device"));
            assert!(!html.contains("hx-delete"));
        }

        #[test]
        fn tokens_partial_renders_revoke_when_present() {
            let html = TokensPartial {
                tokens: vec![TokenRow {
                    id: "tok-1".into(),
                    name: "CI".into(),
                    prefix: "sm_live_aaaaaaaa".into(),
                    created: "2026-05-16T12:00:00Z".parse().unwrap(),
                    last_used: None,
                    expires: None,
                    expired: false,
                    access: "read-only",
                    scopes: "targets:read".into(),
                    org: Some("acme".into()),
                }],
            }
            .render()
            .unwrap();
            assert!(html.contains("CI"));
            assert!(html.contains("sm_live_aaaaaaaa"));
            assert!(html.contains("read-only"));
            assert!(html.contains("acme"));
            assert!(html.contains(r#"hx-delete="/api/v1/me/api-tokens/tok-1""#));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn login_page(
        github: bool,
        google: bool,
        microsoft: bool,
        gitlab: bool,
        passkey: bool,
        magic: bool,
    ) -> LoginPage {
        LoginPage {
            active_tab: TAB_LOGIN,
            github_enabled: github,
            github_url: "/auth/github/login".into(),
            google_enabled: google,
            google_url: "/auth/google/login".into(),
            microsoft_enabled: microsoft,
            microsoft_url: "/auth/microsoft/login".into(),
            gitlab_enabled: gitlab,
            gitlab_url: "/auth/gitlab/login".into(),
            passkey_enabled: passkey,
            magic_link_enabled: magic,
            open_signup: true,
            magic_link_expiry_minutes: 15,
            last_github: false,
            last_google: false,
            last_microsoft: false,
            last_gitlab: false,
            last_passkey: false,
            last_magic: false,
            invitation_hint: None,
            ready: true,
            analytics: None,
        }
    }

    #[test]
    fn login_page_renders_all_methods_when_enabled() {
        let html = login_page(true, true, true, true, true, true)
            .render()
            .unwrap();
        assert!(html.contains("continue with a passkey"));
        assert!(html.contains("continue with github"));
        assert!(html.contains(r#"href="/auth/github/login""#));
        assert!(html.contains("continue with google"));
        assert!(html.contains(r#"href="/auth/google/login""#));
        assert!(html.contains("continue with microsoft"));
        assert!(html.contains(r#"href="/auth/microsoft/login""#));
        assert!(html.contains("continue with gitlab"));
        assert!(html.contains(r#"href="/auth/gitlab/login""#));
        assert!(html.contains(r#"id="magic-link-form""#));
        assert!(html.contains("login_magic_link.js"));
        assert!(!html.contains("No sign-in method is configured"));
        // Login page suppresses the user-area nav so a not-yet-authenticated
        // visitor doesn't see broken "Settings"/"Log out" controls.
        assert!(!html.contains("Log out"));
    }

    #[test]
    fn login_page_tags_every_method_for_the_funnel() {
        let mut page = login_page(true, true, true, true, true, true);
        page.last_google = true;
        let html = page.render().unwrap();
        assert!(html.contains(r#"data-umami-event-method="github""#));
        assert!(html.contains(r#"data-umami-event-method="google""#));
        assert!(html.contains(r#"data-umami-event-method="microsoft""#));
        assert!(html.contains(r#"data-umami-event-method="gitlab""#));
        assert!(html.contains(r#"data-umami-event-method="passkey""#));
        // The email form has no attribute event; its script reads this instead.
        assert!(html.contains(r#"data-last-used="false""#));
        assert!(html.contains(r#"data-umami-event-last-used="true""#));
    }

    #[test]
    fn login_page_loads_the_tracker_only_when_analytics_is_on() {
        assert!(
            !login_page(true, true, true, true, false, true)
                .render()
                .unwrap()
                .contains("analytics.uptimepage.dev")
        );
        let mut page = login_page(true, true, true, true, false, true);
        page.analytics = Some("website-id");
        let html = page.render().unwrap();
        assert!(html.contains(r#"data-website-id="website-id""#));
        assert!(html.contains(r#"data-domains="app.uptimepage.dev""#));
        // /login?invitation=<token> must not reach the analytics database.
        assert!(html.contains(r#"data-before-send="smScrubAuthUrl""#));
    }

    #[test]
    fn the_last_used_badge_rides_the_email_button() {
        // Absolutely positioned, so it lands on its `relative` ancestor.
        let mut page = login_page(true, false, false, false, false, true);
        page.last_magic = true;
        let html = page.render().unwrap();
        let badge = html.find("last used").expect("badge renders");
        let button = html.find("continue with email").expect("button renders");
        assert!(badge > button, "inside the button, after its label");
    }

    #[test]
    fn login_page_marks_last_used_method_only() {
        let mut page = login_page(true, true, true, true, false, true);
        page.last_google = true;
        let html = page.render().unwrap();
        assert_eq!(html.matches("last used").count(), 1);
        let google_at = html.find("continue with google").unwrap();
        let badge_at = html.find("last used").unwrap();
        let github_at = html.find("continue with github").unwrap();
        // Badge sits in the google button block, after github's.
        assert!(badge_at > github_at && badge_at > google_at);
    }

    #[test]
    fn login_page_offers_a_passkey_hidden_until_the_device_answers() {
        let mut page = login_page(true, false, false, false, true, false);
        page.last_passkey = true;
        let html = page.render().unwrap();
        assert!(html.contains("continue with a passkey"));
        // Hidden at render; the script reveals it once it knows the browser
        // speaks WebAuthn.
        assert!(
            html.contains(r#"id="sm-passkey-signin" hidden"#),
            "the button starts hidden"
        );
        assert!(
            html.contains("js/ui/passkey_login.js"),
            "and the script that reveals it is loaded"
        );
    }

    #[test]
    fn the_passkey_button_stands_without_a_magic_link_beside_it() {
        let html = login_page(true, false, false, false, true, false)
            .render()
            .unwrap();
        assert!(html.contains("continue with a passkey"));
        assert!(!html.contains("magic-link-email"));
    }

    #[test]
    fn email_is_only_hidden_where_it_cannot_open_an_account() {
        let open = login_page(true, false, false, false, false, true)
            .render()
            .unwrap();
        assert!(!open.contains("<details"), "no disclosure at all");
        assert!(open.contains("or with email"));
        assert!(
            !open.contains(r#"<p class="relative text-center"#),
            "the badge belongs on the control, not the divider"
        );
        assert!(open.contains(r#"id="magic-link-email""#));

        let mut page = login_page(true, false, false, false, false, true);
        page.open_signup = false;
        let closed = page.render().unwrap();
        assert!(closed.contains("<details"), "invite-only still hides it");
        assert!(closed.contains("Already registered?"));

        let mut alone = login_page(false, false, false, false, false, true);
        alone.open_signup = false;
        assert!(!alone.render().unwrap().contains("<details"));
    }

    #[test]
    fn the_email_button_matches_the_others_where_it_can_deliver() {
        let open = login_page(true, false, false, false, false, true)
            .render()
            .unwrap();
        assert!(open.contains("continue with email"));

        let mut page = login_page(true, false, false, false, false, true);
        page.open_signup = false;
        let closed = page.render().unwrap();
        assert!(!closed.contains("continue with email"));
        assert!(closed.contains("email me a sign-in link"));
    }

    #[test]
    fn the_email_field_caps_where_the_server_does() {
        // A client rule stricter than the server would reject addresses the
        // server accepts.
        let html = login_page(true, false, false, false, false, true)
            .render()
            .unwrap();
        assert!(html.contains(&format!(
            r#"maxlength="{}""#,
            crate::auth::email_norm::MAX_EMAIL_LEN
        )));
    }

    #[test]
    fn the_code_submits_as_a_plain_form_post() {
        // Revealing the panel needs the script; submitting must not.
        let html = login_page(true, false, false, false, false, true)
            .render()
            .unwrap();
        assert!(html.contains(r#"action="/auth/magic-link/code""#));
        assert!(html.contains(r#"id="magic-link-code""#));
        assert!(html.contains(r#"autocomplete="one-time-code""#));
        assert!(html.contains(r#"placeholder="4KP9RT""#));
        assert_eq!("4KP9RT".len(), crate::auth::magic_link::CODE_LEN);
        assert!(
            html.contains("The link in the same email still works"),
            "one try is only fair if the fallback is named"
        );
        assert!(html.contains(r#"id="magic-link-again""#));
    }

    #[test]
    fn the_email_field_makes_no_passkey_offer() {
        // A `webauthn` token here arms conditional mediation, which mints a
        // ceremony row on every view of the page.
        for passkey in [true, false] {
            let html = login_page(true, false, false, false, passkey, true)
                .render()
                .unwrap();
            assert!(html.contains(r#"autocomplete="email""#));
            assert!(!html.contains("webauthn"), "passkey_enabled={passkey}");
        }
    }

    #[test]
    fn the_signup_page_shows_what_signup_records() {
        let html = login_page(true, false, false, false, false, true)
            .render()
            .unwrap();
        assert!(html.contains(r#"href="/terms""#));
        assert!(html.contains(r#"href="/privacy""#));
    }

    #[test]
    fn login_page_omits_the_passkey_button_where_it_is_off() {
        let html = login_page(true, false, false, false, false, false)
            .render()
            .unwrap();
        assert!(!html.contains("continue with a passkey"));
        assert!(!html.contains("js/ui/passkey_login.js"));
    }

    #[test]
    fn a_passkey_alone_is_still_a_configured_method() {
        // The "nothing is configured" warning counts passkeys, or a deployment
        // offering only them claims it offers nothing.
        let html = login_page(false, false, false, false, true, false)
            .render()
            .unwrap();
        assert!(!html.contains("No sign-in method is configured"));
    }

    #[test]
    fn login_page_shows_no_badge_for_first_visit() {
        let html = login_page(true, true, true, true, false, true)
            .render()
            .unwrap();
        assert!(!html.contains("last used"));
    }

    #[test]
    fn login_page_hides_disabled_methods() {
        let html = login_page(true, false, false, false, false, false)
            .render()
            .unwrap();
        assert!(html.contains("continue with github"));
        assert!(!html.contains("continue with google"));
        assert!(!html.contains(r#"id="magic-link-form""#));
    }

    #[test]
    fn login_page_renders_microsoft_on_its_own() {
        let html = login_page(false, false, true, false, false, false)
            .render()
            .unwrap();
        assert!(html.contains("continue with microsoft"));
        assert!(!html.contains("No sign-in method is configured"));
    }

    #[test]
    fn login_page_renders_gitlab_on_its_own() {
        let html = login_page(false, false, false, true, false, false)
            .render()
            .unwrap();
        assert!(html.contains("continue with gitlab"));
        assert!(html.contains(r#"href="/auth/gitlab/login""#));
        assert!(!html.contains("No sign-in method is configured"));
    }

    #[test]
    fn login_page_renders_each_provider_on_its_own() {
        // The four button blocks are near-identical, so a copy-paste that left
        // the wrong flag or URL behind only shows up when each renders alone.
        const BUTTONS: [(&str, &str); 4] = [
            ("continue with github", "/auth/github/login"),
            ("continue with google", "/auth/google/login"),
            ("continue with microsoft", "/auth/microsoft/login"),
            ("continue with gitlab", "/auth/gitlab/login"),
        ];
        for (i, (label, url)) in BUTTONS.iter().enumerate() {
            let html = login_page(i == 0, i == 1, i == 2, i == 3, false, false)
                .render()
                .unwrap();
            assert!(html.contains(label), "{label} missing when enabled alone");
            assert!(html.contains(url), "{url} missing when enabled alone");
            for (j, (other, _)) in BUTTONS.iter().enumerate() {
                if i != j {
                    assert!(
                        !html.contains(other),
                        "{other} leaked into the {label} case"
                    );
                }
            }
        }
    }

    #[test]
    fn login_page_never_offers_the_link_dance() {
        // Adding a method is a signed-in action from settings, and its route is
        // POST-only. A start URL rendered as a link here would be dead on click
        // and an invitation to confuse the two flows.
        let html = login_page(true, true, true, true, false, true)
            .render()
            .unwrap();
        assert!(!html.contains("/link"), "no link-dance URL belongs here");
    }

    #[test]
    fn login_page_warns_only_when_no_method_available() {
        let html = login_page(false, false, false, false, false, false)
            .render()
            .unwrap();
        assert!(html.contains("No sign-in method is configured"));
        assert!(!html.contains("continue with github"));
        assert!(!html.contains("continue with google"));

        let html = login_page(false, false, false, false, false, true)
            .render()
            .unwrap();
        assert!(!html.contains("No sign-in method is configured"));
        assert!(html.contains(r#"id="magic-link-form""#));
        // No oauth button → no "or" divider above the form.
        assert!(!html.contains(">or<"));
    }

    #[test]
    fn login_page_shows_invitation_hint() {
        let mut page = login_page(true, false, false, false, false, false);
        page.github_url = "/auth/github/login?invitation=abc".into();
        page.invitation_hint = Some("abc".into());
        let html = page.render().unwrap();
        assert!(html.contains("After signing in"));
        assert!(html.contains("abc"));
    }
}
