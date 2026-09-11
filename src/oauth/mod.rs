//! OAuth 2.1 Authorization Server backing the one-click MCP connector.
//!
//! Topology: this AS runs on the **app** host (where the user's session cookie
//! lives, so `/oauth/authorize` can authenticate them); the protected resource
//! is `/mcp` on its own host. The flow is the standard MCP one:
//!
//! ```text
//!  /mcp 401 + WWW-Authenticate(resource_metadata)
//!    → GET .well-known/oauth-protected-resource   (RFC 9728, mcp module)
//!    → GET .well-known/oauth-authorization-server  (RFC 8414, here)
//!    → POST /oauth/register                         (RFC 7591 DCR)
//!    → GET  /oauth/authorize  → login + consent     (PKCE S256, RFC 8707 resource)
//!    → POST /oauth/token      → audience-bound sm_live_ token
//! ```
//!
//! The access token is the existing read-only, org-bound, expiring scoped token,
//! now stamped with this MCP endpoint as its `audience`. Nothing here mints
//! write scopes.

mod authorize;
mod error;
mod metadata;
mod pkce;
mod register;
mod store;
mod token;

use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::app::AppState;
use crate::auth::scope::{Scope, ScopeSet};
use crate::config::AppConfig;

/// How often the sweeper purges expired authorization codes + refresh tokens.
const SWEEP_INTERVAL_SECS: u64 = 3600;

/// Scopes a connector gets by default (no `scope` requested) — read-only.
const DEFAULT_SCOPES: &[Scope] = &[
    Scope::TargetsRead,
    Scope::StatusPageRead,
    Scope::IncidentsRead,
];

/// Every scope a connector MAY request. Write scopes are opt-in: granted only
/// when the client explicitly asks for them, and surfaced distinctly on the
/// consent screen. The write tools are each still scope-gated AND elicitation-
/// confirmed per action, so a granted write scope is necessary but not
/// sufficient to mutate anything.
const GRANTABLE_SCOPES: &[Scope] = &[
    Scope::TargetsRead,
    Scope::StatusPageRead,
    Scope::IncidentsRead,
    // Not in the defaults: a connector that never touches alerting should not
    // be handed the channel inventory.
    Scope::ChannelsRead,
    Scope::TargetsWrite,
    Scope::TargetsExecute,
    Scope::IncidentsWrite,
    Scope::StatusPageWrite,
    // Keys only; the values are never read on this path.
    Scope::VariablesRead,
];

/// Authorization-code lifetime. Short — it's redeemed immediately.
const CODE_TTL_SECS: i64 = 60;

/// Default + ceiling for the user-chosen access-token lifetime on the consent
/// screen. No "never" option: an automated, leak-prone connector token with no
/// expiry (and no refresh) is the one lifetime worth disallowing.
const DEFAULT_TOKEN_TTL_DAYS: u32 = 90;
const MAX_TOKEN_TTL_DAYS: u32 = 365;

/// Canonical OAuth URLs derived from config. `issuer` is the app origin; the
/// metadata + endpoint URLs hang off it. `resource` is the MCP audience.
struct OAuthUrls {
    issuer: String,
    resource: String,
}

impl OAuthUrls {
    fn from_cfg(cfg: &AppConfig) -> Self {
        Self {
            issuer: cfg.auth.public_base_url.trim_end_matches('/').to_string(),
            resource: cfg.mcp.resource_uri.trim_end_matches('/').to_string(),
        }
    }
    fn authorize_endpoint(&self) -> String {
        format!("{}/oauth/authorize", self.issuer)
    }
    fn token_endpoint(&self) -> String {
        format!("{}/oauth/token", self.issuer)
    }
    fn registration_endpoint(&self) -> String {
        format!("{}/oauth/register", self.issuer)
    }
    /// URL of the protected-resource metadata, for the `WWW-Authenticate`
    /// `resource_metadata` hint. Anchored on the resource's own origin.
    fn resource_metadata_url(&self) -> String {
        // The resource URI ends in `/mcp`; RFC 9728 puts the doc at the
        // origin root + `/.well-known/oauth-protected-resource`.
        match resource_origin(&self.resource) {
            Some(origin) => format!("{origin}/.well-known/oauth-protected-resource"),
            None => format!("{}/.well-known/oauth-protected-resource", self.issuer),
        }
    }
}

/// Loopback per RFC 8252 §7.3, on the typed host so `[::1]` counts.
pub(crate) fn is_loopback_http(u: &url::Url) -> bool {
    use url::Host;
    u.scheme() == "http"
        && match u.host() {
            Some(Host::Domain(d)) => d == "localhost",
            Some(Host::Ipv4(v4)) => v4.is_loopback(),
            Some(Host::Ipv6(v6)) => v6.is_loopback(),
            None => false,
        }
}

/// Native clients admitted by scheme name. RFC 8252 §7.1 wants a reverse-DNS
/// scheme, which is admitted by shape below; this list is for the editors
/// that use a bare name instead, each added once seen on the wire.
const NATIVE_SCHEMES: &[&str] = &["cursor"];

/// Acceptable redirect-URI shapes for registration + authorize. HTTPS for web
/// connectors, loopback HTTP for local tooling, and a native app's own scheme.
/// An allowlist, not a denylist: registration is open and the authorize error
/// path 303s to the registered URI before any login, so every admitted scheme
/// is one the app origin may send a browser to with attacker-chosen query
/// bytes. A scheme hijack on the user's machine still gets no PKCE verifier.
/// Matching is always exact-string elsewhere; this only gates *registration*.
fn is_acceptable_redirect_uri(uri: &str) -> bool {
    let Ok(u) = url::Url::parse(uri) else {
        return false;
    };
    // No fragment (RFC 6749 §3.1.2) and no userinfo — `https://attacker@host/`
    // confuses the displayed authority and serves no legitimate purpose here.
    if u.fragment().is_some() || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    match u.scheme() {
        "https" => u.host_str().is_some(),
        "http" => is_loopback_http(&u),
        scheme => scheme.contains('.') || NATIVE_SCHEMES.contains(&scheme),
    }
}

/// Scheme+host(+port) of a URL, no path. Used to anchor metadata on the
/// resource origin.
fn resource_origin(uri: &str) -> Option<String> {
    let u = url::Url::parse(uri).ok()?;
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

fn scope_list_string(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every scope the AS will advertise as grantable (for discovery metadata).
fn grantable_scope_string() -> String {
    scope_list_string(GRANTABLE_SCOPES)
}

/// The baseline read scopes (the `scope` hint in `WWW-Authenticate`).
fn default_scope_string() -> String {
    scope_list_string(DEFAULT_SCOPES)
}

/// Resolve the granted scope from a requested `scope`: requested ∩ grantable,
/// de-duplicated, preserving the grantable ordering. An empty/garbage request
/// grants the read-only default set — write scopes are NEVER granted unless
/// explicitly requested. Output is a canonical space-delimited string.
fn grant_scope(requested: Option<&str>) -> String {
    let Some(req) = requested else {
        return default_scope_string();
    };
    let asked: ScopeSet = ScopeSet::from_strs(req.split_whitespace());
    let granted: Vec<&'static str> = GRANTABLE_SCOPES
        .iter()
        .filter(|s| asked.allows(**s))
        .map(|s| s.as_str())
        .collect();
    if granted.is_empty() {
        return default_scope_string();
    }
    granted.join(" ")
}

/// Periodic cleanup of dead OAuth rows: authorization codes past their (~60s)
/// TTL and refresh tokens past their family deadline. Bounds table growth from
/// abandoned consents + rotation churn. Like the rate-limit janitor, it is
/// bound to the shutdown token so it can't outlive the process.
/// Expired refresh rows are safe to drop wholesale: once `expires_at` passes,
/// the whole family is dead, so removing its (used + current) rows costs no
/// replay-detection coverage.
pub fn spawn_sweeper(pool: PgPool, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => sweep(&pool).await,
            }
        }
    });
}

async fn sweep(pool: &PgPool) {
    for sql in [
        "DELETE FROM oauth_authorization_codes WHERE expires_at < now()",
        "DELETE FROM oauth_refresh_tokens WHERE expires_at < now()",
    ] {
        if let Err(e) = sqlx::query(sql).execute(pool).await {
            tracing::warn!(target: "oauth", error = %e, "oauth sweep failed");
        }
    }
}

/// The `WWW-Authenticate: Bearer …` value the MCP resource server returns on a
/// 401, pointing clients at the RFC 9728 resource metadata so they can discover
/// the AS. `None` when OAuth isn't configured (caller falls back to plain
/// `Bearer`).
pub fn www_authenticate_value(cfg: &AppConfig) -> Option<String> {
    if !cfg.mcp.oauth_enabled || cfg.mcp.resource_uri.is_empty() {
        return None;
    }
    let urls = OAuthUrls::from_cfg(cfg);
    Some(format!(
        "Bearer resource_metadata=\"{}\", scope=\"{}\"",
        urls.resource_metadata_url(),
        default_scope_string()
    ))
}

/// Mount the OAuth endpoints. The RFC 9728 protected-resource metadata is
/// served whenever the MCP resource is configured; the AS endpoints require
/// `oauth_enabled`. Returns unstated routes to merge into the web router so
/// they inherit cookies + CSRF + the cross-cutting layers.
pub fn routes(cfg: &AppConfig) -> Router<AppState> {
    let mut r = Router::new();
    if cfg.mcp.enabled && !cfg.mcp.resource_uri.is_empty() {
        r = r
            .route(
                "/.well-known/oauth-protected-resource",
                get(metadata::protected_resource),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(metadata::protected_resource),
            );
    }
    if cfg.mcp.oauth_enabled {
        r = r
            .route(
                "/.well-known/oauth-authorization-server",
                get(metadata::authorization_server),
            )
            .route("/oauth/authorize", get(authorize::authorize_page))
            .route("/oauth/authorize/decision", post(authorize::decision))
            .route("/oauth/token", post(token::token))
            .route("/oauth/register", post(register::register));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_scope_defaults_to_full_read_set() {
        assert_eq!(
            grant_scope(None),
            "targets:read status_page:read incidents:read"
        );
        assert_eq!(
            grant_scope(Some("")),
            "targets:read status_page:read incidents:read"
        );
    }

    #[test]
    fn grant_scope_intersects_with_grantable() {
        assert_eq!(grant_scope(Some("targets:read")), "targets:read");
        assert_eq!(
            grant_scope(Some("status_page:read targets:read")),
            "targets:read status_page:read"
        );
        // Unknown scopes are dropped → fall back to the read-only default.
        assert_eq!(
            grant_scope(Some("bogus")),
            "targets:read status_page:read incidents:read"
        );
    }

    #[test]
    fn grant_scope_grants_write_only_when_requested() {
        // Default + read requests never include write scopes.
        assert!(!grant_scope(None).contains("write"));
        assert!(!grant_scope(Some("targets:read")).contains(":write"));
        // Explicit write request is granted (write implies its read).
        assert_eq!(
            grant_scope(Some("targets:write")),
            "targets:read targets:write"
        );
        assert_eq!(grant_scope(Some("targets:execute")), "targets:execute");
        assert_eq!(
            grant_scope(Some("incidents:write")),
            "incidents:read incidents:write"
        );
        // Full write connector.
        assert_eq!(
            grant_scope(Some("targets:write targets:execute incidents:write")),
            "targets:read incidents:read targets:write targets:execute incidents:write"
        );
    }

    #[test]
    fn redirect_uri_acceptance() {
        // HTTPS with a host — accepted.
        assert!(is_acceptable_redirect_uri(
            "https://claude.ai/api/mcp/callback"
        ));
        // Loopback HTTP (mcp-remote, VS Code) — accepted, IPv6 included.
        assert!(is_acceptable_redirect_uri("http://localhost:8976/callback"));
        assert!(is_acceptable_redirect_uri("http://127.0.0.1:5000/cb"));
        assert!(is_acceptable_redirect_uri("http://127.0.0.1:33418"));
        assert!(is_acceptable_redirect_uri("http://[::1]:8976/cb"));
        // Non-loopback HTTP — rejected (no cleartext over the network).
        assert!(!is_acceptable_redirect_uri("http://evil.example.com/cb"));
        assert!(!is_acceptable_redirect_uri("http://10.0.0.5/cb"));
        // Native clients: a named editor scheme or the RFC 8252 reverse-DNS shape.
        assert!(is_acceptable_redirect_uri(
            "cursor://anysphere.cursor-retrieval/oauth/user-1/callback"
        ));
        assert!(is_acceptable_redirect_uri("com.example.app:/cb"));
        // Every other scheme is a protocol handler the app origin must not
        // launch: OS handlers, browser-executed, cleartext transports.
        assert!(!is_acceptable_redirect_uri("ms-msdt:/id PCWDiagnostic"));
        assert!(!is_acceptable_redirect_uri("javascript:alert(1)"));
        assert!(!is_acceptable_redirect_uri("data:text/html,hi"));
        assert!(!is_acceptable_redirect_uri("file:///etc/passwd"));
        assert!(!is_acceptable_redirect_uri("ftp://evil.example.com/cb"));
        assert!(!is_acceptable_redirect_uri("ws://evil.example.com/cb"));
        assert!(!is_acceptable_redirect_uri("windsurf://cb"));
        // Userinfo on a native scheme is no better than on https.
        assert!(!is_acceptable_redirect_uri("cursor://u@host/cb"));
        // Userinfo in the authority — rejected (authority-confusion).
        assert!(!is_acceptable_redirect_uri(
            "https://attacker@victim.example/cb"
        ));
        assert!(!is_acceptable_redirect_uri("https://u:p@victim.example/cb"));
        // Fragment — rejected (RFC 6749 forbids it on redirect URIs).
        assert!(!is_acceptable_redirect_uri("https://claude.ai/cb#frag"));
        // Garbage — rejected.
        assert!(!is_acceptable_redirect_uri("not a url"));
        assert!(!is_acceptable_redirect_uri("https://"));
    }

    #[test]
    fn resource_origin_strips_path() {
        assert_eq!(
            resource_origin("https://mcp.example.com/mcp").as_deref(),
            Some("https://mcp.example.com")
        );
        assert_eq!(
            resource_origin("http://localhost:8080/mcp").as_deref(),
            Some("http://localhost:8080")
        );
    }
}
