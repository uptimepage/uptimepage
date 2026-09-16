//! Host-based org resolution for the public status surface.
//!
//! Two surfaces share one binary and one set of handlers:
//!  * path-based (self-host) — every request resolves to the default org;
//!  * subdomain (SaaS) — the org is taken from `{slug}.{base_domain}`.
//!
//! `extract_status_slug` is the pure host parser; [`StatusPageOrg`] is the
//! request extractor that picks the surface from config and yields the
//! `OrgId` the handler should serve. The host *determines the org*; the path
//! *determines the resource* (the JSON paths are identical across surfaces).

use axum::extract::{FromRef, FromRequestParts, Request, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::HOST;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::handlers::health::is_health_path;
use crate::api::public_error::PublicAppError;
use crate::api::subdomain_public_routes_enabled;
use crate::app::AppState;
use crate::config::AppConfig;
use crate::domain::{OrgId, PageRef, StatusPageId};

/// Subdomain labels that route to the operator surface (dashboard + auth +
/// API) instead of the per-org public page. These are NOT the
/// signup-collision list (`crate::domain::reserved_slugs`) — that list is
/// much broader (~75 entries) and exists to keep an org from claiming a
/// slug that *could* collide with any operator URL or namespace. Mixing
/// the two here would route every reserved label (e.g. `acme`,
/// `hetzner`, `gdpr`) to the operator login page, multiplying the
/// operator surface across dozens of hosts and leaking the reserved list
/// via response codes. Keep this set minimal — only labels actually
/// served by the operator front door. `mcp` is the connector host: it
/// reaches the app router like `app` but [`host_isolation`] narrows it to
/// the transport and its discovery documents.
const MCP_LABEL: &str = "mcp";
const OPERATOR_LABELS: &[&str] = &["app", MCP_LABEL];

/// Marketing labels — apex (empty) and `www` route to the marketing site,
/// not to a tenant. Kept tight (only `www`); deeper aliases would
/// multiply marketing's surface and shadow tenant slugs.
const MARKETING_LABELS: &[&str] = &["www"];

fn is_operator_label(slug: &str) -> bool {
    OPERATOR_LABELS.iter().any(|l| slug.eq_ignore_ascii_case(l))
}

/// One source of truth for the production wire format. Constructed once
/// at boot from `public_status.base_domain` and shared between
/// [`extract_status_slug`] and [`classify_host`] so they cannot drift.
/// Apex shape is flat (`{slug}.{base_domain}`) — there is no `.status.`
/// infix; the FLATTEN-STATUS-HOST change already removed it.
#[derive(Debug, Clone)]
pub struct HostScheme {
    pub base_domain: String,
    pub apex: String,
    pub www_host: String,
    pub app_host: String,
    /// `.{base_domain}` — what every tenant host ends with. Carries the
    /// leading dot so a `strip_suffix` accepts only `{slug}.{base}`, not
    /// the bare base.
    pub tenant_suffix: String,
}

impl HostScheme {
    /// Validates the base domain (non-empty, contains a dot) and derives
    /// the apex / `www` / `app` / tenant-suffix triplet. Returns a
    /// human-readable error suitable for a startup config error.
    pub fn from_base_domain(base: &str) -> std::result::Result<Self, String> {
        let trimmed = base.trim().to_ascii_lowercase();
        if trimmed.is_empty() {
            return Err("base_domain must not be empty".into());
        }
        if !trimmed.contains('.') {
            return Err(format!("base_domain {trimmed:?} must contain a dot"));
        }
        Ok(Self {
            apex: trimmed.clone(),
            www_host: format!("www.{trimmed}"),
            app_host: format!("app.{trimmed}"),
            tenant_suffix: format!(".{trimmed}"),
            base_domain: trimmed,
        })
    }
}

/// What kind of host arrived. The marketing dispatch seam routes
/// `Marketing` (and `Unknown`, so garbage cannot fall through to a
/// tenant) to the marketing router; `App` and `TenantPublic` go to the
/// existing app router which already does per-host org resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    Marketing,
    App,
    TenantPublic,
    Unknown,
}

/// Pure structural decomposition of a `Host` header against `{base_domain}`.
/// Single source of truth for shape rules (port strip, FQDN trailing dot,
/// case-insensitive suffix match, single-label slug). Both the UI router
/// taxonomy (`classify_host`) and the dispatch-policy predicates
/// (`extract_status_slug`, `is_subdomain_public_request`) build on this so
/// they can never drift on what "a tenant subdomain" means.
///
/// Borrows the slug out of the input — no allocation on the hot path.
/// Case is preserved; consumers use `eq_ignore_ascii_case` for label
/// comparisons.
#[derive(Debug, PartialEq, Eq)]
pub enum HostShape<'a> {
    /// Host equals `base_domain` itself (the apex).
    Apex,
    /// Host matches `{slug}.{base_domain}` with a single, non-empty label.
    Subdomain(&'a str),
    /// Anything else — unrelated host, deeper subdomain, empty slug, or
    /// malformed `base_domain`.
    Other,
}

/// Strip port + FQDN trailing dot, then classify against `base_domain`
/// with a case-insensitive byte compare. Returns borrowed views — no
/// per-request allocation. Rejects an empty / single-label base domain
/// so a misconfigured rig can't widen the suffix match.
pub fn parse_host_shape<'a>(host: &'a str, base_domain: &str) -> HostShape<'a> {
    if base_domain.is_empty() || !base_domain.contains('.') {
        return HostShape::Other;
    }
    let host = normalize_host(host);
    if host.eq_ignore_ascii_case(base_domain) {
        return HostShape::Apex;
    }
    // Manual case-insensitive `.{base_domain}` suffix strip without
    // allocating a lowercased copy.
    let dot_base_len = base_domain.len() + 1;
    if host.len() <= dot_base_len {
        return HostShape::Other;
    }
    let (head, tail) = host.split_at(host.len() - dot_base_len);
    if !tail.starts_with('.') || !tail[1..].eq_ignore_ascii_case(base_domain) {
        return HostShape::Other;
    }
    if head.is_empty() || head.contains('.') {
        return HostShape::Other;
    }
    HostShape::Subdomain(head)
}

/// Strip the port and the FQDN trailing dot. Hosts compare case-insensitively,
/// so a caller building a URL out of the result has to lower-case it too.
fn normalize_host(host: &str) -> &str {
    let host = host.split(':').next().unwrap_or(host);
    host.strip_suffix('.').unwrap_or(host)
}

/// Absolute origin the request arrived on, or `None` when the `Host` is
/// neither this deployment's apex nor one of its tenant subdomains. Links we
/// publish outlive the request that produced them, so a forged `Host` must
/// never reach one; callers fall back to their configured public URL.
pub fn request_origin(headers: &HeaderMap, base_domain: &str) -> Option<String> {
    let raw = headers.get(HOST).and_then(|v| v.to_str().ok())?;
    match parse_host_shape(raw, base_domain) {
        HostShape::Apex | HostShape::Subdomain(_) => {
            let scheme = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .filter(|s| matches!(*s, "http" | "https"))
                .unwrap_or("https");
            // The port stays: a deploy off 443 still has to publish links that
            // resolve. Anything but digits is not one, so it is dropped rather
            // than pasted into a URL.
            let port = raw
                .split_once(':')
                .map(|(_, p)| p)
                .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
            let host = normalize_host(raw).to_ascii_lowercase();
            Some(match port {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            })
        }
        HostShape::Other => None,
    }
}

/// Classify a request's `Host` header against the production wire
/// format. Builds on [`parse_host_shape`] so shape rules stay in one
/// place; this layer only maps `(Apex | Subdomain | Other)` plus the
/// label sets to the router taxonomy.
pub fn classify_host(host: &str, scheme: &HostScheme) -> HostClass {
    match parse_host_shape(host, &scheme.base_domain) {
        HostShape::Apex => HostClass::Marketing,
        HostShape::Other => HostClass::Unknown,
        HostShape::Subdomain(slug) => {
            if MARKETING_LABELS
                .iter()
                .any(|l| slug.eq_ignore_ascii_case(l))
            {
                HostClass::Marketing
            } else if is_operator_label(slug) {
                HostClass::App
            } else {
                HostClass::TenantPublic
            }
        }
    }
}

/// Parsed `{slug}.{base_domain}` host. Borrows the slug out of the `Host`
/// header so the common path allocates nothing.
#[derive(Debug, PartialEq, Eq)]
pub struct StatusPageHost<'a> {
    pub slug: &'a str,
}

/// Pull the org slug out of a status-page host, or `None` if `host` is not a
/// well-formed `{slug}.{base_domain}`.
///
/// `base_domain` must be non-empty and contain a dot. Without that guard an
/// empty/misconfigured base domain collapses the match suffix to a single
/// dot, and any host ending in `.` would parse as a valid slug. The boot-time
/// config assertion normally prevents an empty base domain reaching here;
/// this is the second layer of defence so a misconfigured dev/test rig can't
/// extract slugs from arbitrary hosts.
pub fn extract_status_slug<'a>(host: &'a str, base_domain: &str) -> Option<StatusPageHost<'a>> {
    match parse_host_shape(host, base_domain) {
        HostShape::Subdomain(slug) => Some(StatusPageHost { slug }),
        _ => None,
    }
}

/// The org a public-status request should be served from. Construct only via
/// the extractor — never by hand — so the host→org provenance stays at the
/// type level.
///
/// Resolution is surface-aware:
///  * subdomain surface live → parse the `Host` header and resolve the slug
///    through [`find_public_status_org_by_slug`], which filters
///    `public_status_enabled = true`. A missing/garbled host or a
///    not-opted-in org is a 404 — the public surface never confirms which
///    orgs exist.
///  * otherwise (path-based / self-host) → the boot-time default org.
///
/// [`find_public_status_org_by_slug`]: crate::storage::orgs::find_public_status_org_by_slug
#[derive(Debug, Clone, Copy)]
pub struct ResolvedStatusPage(pub PageRef);

/// Shared resolver behind the [`ResolvedStatusPage`] extractor. Exposed so the
/// server-rendered handlers can map a failure to the styled HTML error page
/// instead of the JSON envelope an extractor rejection would produce.
///
/// Subdomain surface: parse the `Host` header, resolve the slug through
/// [`find_public_status_page_by_slug`] (filters `enabled = true`). A
/// missing/garbled host or a disabled/unknown page is a 404 — the public
/// surface never confirms which pages exist. Path-based / self-host: the lone
/// live org's default (first enabled) page.
///
/// [`find_public_status_page_by_slug`]: crate::storage::orgs::find_public_status_page_by_slug
pub async fn resolve_status_page(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<PageRef, PublicAppError> {
    if !subdomain_public_routes_enabled(&state.cfg) {
        return resolve_default_page(state).await;
    }

    let host = headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .ok_or(PublicAppError::NotFound)?;
    let parsed = extract_status_slug(host, &state.cfg.public_status.base_domain)
        .ok_or(PublicAppError::NotFound)?;

    let pool = state.db.as_ref().ok_or_else(|| {
        PublicAppError::Internal(anyhow::anyhow!(
            "subdomain public routes enabled but no database handle"
        ))
    })?;

    // Lowercase before the lookup: stored slugs are canonical lowercase, but a
    // crafted client can send mixed case. Matches `parse_host_shape`'s own
    // case-insensitivity so a legitimate tenant on `ACME.{base}` still resolves.
    let slug = parsed.slug.to_ascii_lowercase();

    // One indexed point lookup (partial index on slug WHERE enabled) per
    // subdomain request. Filters `enabled = true` and returns the
    // `ResolvedPublicPage` newtype the operator path can't accept.
    let page = crate::storage::orgs::find_public_status_page_by_slug(pool, &slug)
        .await?
        .ok_or(PublicAppError::NotFound)?;
    Ok(page.0)
}

/// Path-based fallback: the lone live org's default (first enabled) page, or
/// 404 if none. Self-host single-tenant deploys hit this path; SaaS disables
/// path-based public routes so this never fires there.
///
/// **Contract:** `state.db.is_some()` in every production code path. The `None`
/// branch is reachable ONLY from in-memory test fixtures, where `PublicSource`
/// is mocked and ignores the returned ids. The nil sentinel keeps those
/// fixtures rendering as before.
async fn resolve_default_page(state: &AppState) -> Result<PageRef, PublicAppError> {
    let Some(pool) = state.db.as_ref() else {
        return Ok(PageRef {
            page: StatusPageId(uuid::Uuid::nil()),
            org: OrgId(uuid::Uuid::nil()),
        });
    };
    crate::storage::orgs::resolve_default_page_for_lone_org(pool)
        .await
        .map_err(|e| PublicAppError::Internal(anyhow::anyhow!("resolve_default_page: {e}")))?
        .ok_or(PublicAppError::NotFound)
}

impl<S> FromRequestParts<S> for ResolvedStatusPage
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = PublicAppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        let page = resolve_status_page(&app_state, &parts.headers).await?;
        Ok(ResolvedStatusPage(page))
    }
}

/// True when the SaaS subdomain surface is live AND the request's `Host`
/// parses as a non-operator `{slug}.{base_domain}`. The `/` dispatcher
/// uses this to choose between the operator dashboard and the per-org
/// public page. Only the operator label set ([`OPERATOR_LABELS`]) falls
/// through to the dashboard; any other label — including signup-reserved
/// ones like `acme` or `hetzner` — keeps the public dispatcher, which
/// then 404s through [`StatusPageOrg`] when no opted-in org owns that
/// slug. Routing the broader reserved list here would multiply the
/// operator surface (login, API, settings) across dozens of hosts and
/// leak the reserved set via response-code fingerprints.
pub fn is_subdomain_public_request(state: &AppState, headers: &HeaderMap) -> bool {
    host_surface(state, headers) == HostSurface::Tenant
}

/// The slice of the app router a request's `Host` may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostSurface {
    /// Dashboard, auth, API: `app.{base}`, and every host once the SaaS
    /// subdomain surface is off.
    Operator,
    /// The public-status allow-list only.
    Tenant,
    /// The MCP transport and its discovery documents only.
    Mcp,
}

/// Tenant hosts exist only with the SaaS subdomain surface on; the MCP host
/// is narrowed whenever it is distinct from the app host ([`is_mcp_host`]).
fn host_surface(state: &AppState, headers: &HeaderMap) -> HostSurface {
    let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else {
        return HostSurface::Operator;
    };
    if is_mcp_host(&state.cfg, host) {
        return HostSurface::Mcp;
    }
    // Slug-shaped non-operator hosts (including `www` and any marketing
    // label) belong on the public dispatcher — even when the upstream
    // marketing router isn't wired up. Routing them to the operator `/`
    // dashboard would expose the login surface on attacker-controlled
    // labels (`admin.{base}`, `www.{base}`). When marketing IS enabled,
    // `RouteByHost` intercepts those hosts before this code runs.
    match parse_host_shape(host, &state.cfg.public_status.base_domain) {
        HostShape::Subdomain(slug)
            if subdomain_public_routes_enabled(&state.cfg) && !is_operator_label(slug) =>
        {
            HostSurface::Tenant
        }
        _ => HostSurface::Operator,
    }
}

/// The host of `mcp.resource_uri` is the connector host, unless it is the
/// app host itself (dev serves both on one origin, and narrowing it would
/// 404 the dashboard). The `mcp` label under the base domain is narrowed
/// regardless: the wildcard record and the Caddy site block answer for it
/// whether or not the connector is on.
fn is_mcp_host(cfg: &AppConfig, host: &str) -> bool {
    if matches!(
        parse_host_shape(host, &cfg.public_status.base_domain),
        HostShape::Subdomain(slug) if slug.eq_ignore_ascii_case(MCP_LABEL)
    ) {
        return true;
    }
    let Some(resource) = url_host(&cfg.mcp.resource_uri) else {
        return false;
    };
    let host = normalize_host(host);
    host.eq_ignore_ascii_case(resource)
        && !url_host(&cfg.auth.public_base_url).is_some_and(|app| app.eq_ignore_ascii_case(host))
}

/// Host of an absolute URL, port and trailing dot stripped; `None` when the
/// value is empty or has no authority.
fn url_host(url: &str) -> Option<&str> {
    let authority = url.trim().split_once("://")?.1;
    let authority = authority.split(['/', '?', '#']).next()?;
    let host = normalize_host(authority);
    (!host.is_empty()).then_some(host)
}

/// Exact paths the public tenant surface (`{slug}.{base_domain}`) is
/// allowed to expose. Combined with [`PUBLIC_TENANT_PREFIXES`] this is
/// the complete allow-list — every grep-able place a tenant-host route
/// is whitelisted, so adding a new public-tenant route forces a visit
/// here too. Mirrors the `HEALTH_PATHS` pattern in `api::handlers::health`.
const PUBLIC_TENANT_EXACT: &[&str] = &["/", "/status", "/subscribe", "/.well-known/security.txt"];

/// Path prefixes the public tenant surface is allowed to expose. No
/// `/robots.txt` or `/sitemap.xml` by design — tenant pages aren't
/// crawl targets, and the public-status template emits its own meta.
/// No general `/.well-known/*` — only `security.txt` is allow-listed,
/// ACME challenges run on the apex/`app.{base}` host.
const PUBLIC_TENANT_PREFIXES: &[&str] = &["/status/", "/subscribe/", "/api/public/v1/", "/static/"];

/// Public origin (scheme + host, no trailing slash) a status page is served
/// from: its verified custom domain, else its `{slug}.{base_domain}` subdomain,
/// else the app's `public_base_url` for path-based/self-host deploys. Subscriber
/// confirm/unsubscribe links use this so a white-labelled page never leaks the
/// app host.
pub fn page_origin(
    base_domain: &str,
    public_base_url: &str,
    slug: &str,
    custom_domain: Option<&str>,
    custom_domain_verified: bool,
) -> String {
    if let Some(domain) = custom_domain.filter(|_| custom_domain_verified) {
        return format!("https://{domain}");
    }
    let base = base_domain.trim();
    if !base.is_empty() {
        return format!("https://{slug}.{base}");
    }
    public_base_url.trim_end_matches('/').to_string()
}

fn is_public_tenant_path(path: &str) -> bool {
    PUBLIC_TENANT_EXACT.contains(&path)
        || PUBLIC_TENANT_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// The transport itself plus its discovery documents: the RFC 9728 / RFC 8414
/// metadata and the server card are all `/.well-known/` files, public by
/// definition.
fn is_mcp_host_path(path: &str) -> bool {
    path == crate::mcp::MCP_PATH || path.starts_with("/.well-known/")
}

/// Default-deny middleware on the non-operator hosts. A tenant subdomain
/// (`{slug}.{base}`) serves only the public-tenant allow-list (see
/// [`PUBLIC_TENANT_EXACT`] + [`PUBLIC_TENANT_PREFIXES`]) and the MCP host
/// only the transport plus discovery ([`is_mcp_host_path`]); everything
/// else returns 404 — otherwise `acme.{base}/login` or `mcp.{base}/login`
/// would render the operator login form (a phishing-friendly clone),
/// `/api/v1/*` would reveal 401s a probe can map, `/settings/*` would 302
/// to `/login` and leak the route's existence, and every per-IP edge limit
/// declared on `app.{base}` could be sidestepped by switching the `Host`.
/// The session cookie is host-scoped, so this is not a credential-hijack
/// vector; the gate exists to keep the operator *surface* single-host.
///
/// Applies to the merged app router so it covers both the web UI and
/// the JSON API. Health probes bypass unconditionally so a misrouted
/// probe doesn't take the upstream down. The tenant gate is a no-op when
/// SaaS subdomain mode is off; the MCP gate needs only a base domain, so
/// a self-host that turns the connector on gets it too.
pub async fn host_isolation(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let allowed = is_health_path(path)
        || match host_surface(&state, req.headers()) {
            HostSurface::Operator => true,
            HostSurface::Tenant => is_public_tenant_path(path),
            HostSurface::Mcp => is_mcp_host_path(path),
        };
    if !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_host_follows_the_resource_uri_and_the_mcp_label() {
        let mut cfg = AppConfig::load().expect("config");
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.public_base_url = "https://app.example.com".into();
        cfg.mcp.resource_uri = "https://connector.example.net/mcp".into();
        assert!(is_mcp_host(&cfg, "connector.example.net"));
        assert!(is_mcp_host(&cfg, "Connector.Example.NET:443"));
        assert!(is_mcp_host(&cfg, "mcp.example.com"));
        assert!(!is_mcp_host(&cfg, "app.example.com"));
        assert!(!is_mcp_host(&cfg, "acme.example.com"));
        assert!(!is_mcp_host(&cfg, "example.net"));

        // Dev serves the transport on the app origin: never narrow that one.
        cfg.mcp.resource_uri = "http://app.lvh.me:8080/mcp".into();
        cfg.auth.public_base_url = "http://app.lvh.me:8080".into();
        assert!(!is_mcp_host(&cfg, "app.lvh.me:8080"));

        cfg.mcp.resource_uri = String::new();
        cfg.public_status.base_domain = String::new();
        assert!(!is_mcp_host(&cfg, "mcp.example.com"));
    }

    #[test]
    fn mcp_host_serves_transport_and_discovery_only() {
        assert!(is_mcp_host_path("/mcp"));
        assert!(is_mcp_host_path("/.well-known/oauth-protected-resource"));
        assert!(is_mcp_host_path("/.well-known/mcp/server-card.json"));
        for path in [
            "/",
            "/mcp/",
            "/mcpx",
            "/login",
            "/oauth/register",
            "/api/v1/targets",
        ] {
            assert!(!is_mcp_host_path(path), "{path} must stay off the MCP host");
        }
    }

    #[test]
    fn extracts_slug_from_well_formed_host() {
        assert_eq!(
            extract_status_slug("acme.example.com", "example.com"),
            Some(StatusPageHost { slug: "acme" })
        );
    }

    #[test]
    fn rejects_bare_base_domain() {
        // The base domain itself (no slug label) must not parse as a slug.
        assert_eq!(extract_status_slug("example.com", "example.com"), None);
    }

    #[test]
    fn rejects_empty_slug() {
        assert_eq!(extract_status_slug(".example.com", "example.com"), None);
    }

    #[test]
    fn rejects_non_matching_host() {
        assert_eq!(extract_status_slug("acme.other.com", "example.com"), None);
    }

    #[test]
    fn rejects_deeper_subdomain() {
        assert_eq!(extract_status_slug("a.b.example.com", "example.com"), None);
    }

    #[test]
    fn empty_base_domain_matches_nothing() {
        // An empty base domain must not collapse the suffix into a wildcard
        // that accepts attacker-supplied hosts.
        assert_eq!(extract_status_slug("foo.", ""), None);
        assert_eq!(extract_status_slug("evil.", ""), None);
    }

    #[test]
    fn single_label_base_domain_rejected() {
        // `local` has no dot — refuse so `foo.local` can't resolve in a
        // misconfigured dev rig.
        assert_eq!(extract_status_slug("foo.local", "local"), None);
    }

    #[test]
    fn extract_strips_port() {
        // `parse_host_shape` handles port stripping centrally, so the
        // status-slug extractor inherits that and `acme.example.com:443`
        // parses identically to the bare host.
        assert_eq!(
            extract_status_slug("acme.example.com:443", "example.com"),
            Some(StatusPageHost { slug: "acme" })
        );
    }

    #[test]
    fn extract_is_case_insensitive_on_base_domain() {
        // RFC: hostnames are case-insensitive. `parse_host_shape`
        // compares the base-domain suffix with `eq_ignore_ascii_case`,
        // so `ACME.EXAMPLE.COM` still parses. Slug case is preserved
        // (downstream DB lookup decides how to fold it).
        assert_eq!(
            extract_status_slug("ACME.EXAMPLE.COM", "example.com"),
            Some(StatusPageHost { slug: "ACME" })
        );
    }

    #[test]
    fn accepts_fqdn_trailing_dot() {
        // RFC 1034 FQDN form; a crafted client can send the trailing dot.
        // Without normalisation the suffix match misses and the request
        // would otherwise fall through to the operator dashboard.
        assert_eq!(
            extract_status_slug("acme.example.com.", "example.com"),
            Some(StatusPageHost { slug: "acme" })
        );
    }

    #[test]
    fn fqdn_bare_base_still_rejected() {
        // Trailing dot on the base domain alone — must NOT collapse into a
        // valid (empty-slug) parse.
        assert_eq!(extract_status_slug("example.com.", "example.com"), None);
    }

    #[test]
    fn host_shape_apex() {
        assert_eq!(
            parse_host_shape("example.com", "example.com"),
            HostShape::Apex
        );
        // Trailing dot + port + uppercase all collapse to the same Apex
        // verdict — that's the property both consumers depend on.
        assert_eq!(
            parse_host_shape("Example.COM.:443", "example.com"),
            HostShape::Apex
        );
    }

    #[test]
    fn host_shape_subdomain_borrows_slug() {
        assert_eq!(
            parse_host_shape("acme.example.com", "example.com"),
            HostShape::Subdomain("acme")
        );
    }

    #[test]
    fn host_shape_other_for_garbage() {
        assert_eq!(parse_host_shape("", "example.com"), HostShape::Other);
        assert_eq!(
            parse_host_shape("acme.other.com", "example.com"),
            HostShape::Other
        );
        assert_eq!(
            parse_host_shape("a.b.example.com", "example.com"),
            HostShape::Other
        );
        assert_eq!(
            parse_host_shape(".example.com", "example.com"),
            HostShape::Other
        );
    }

    #[test]
    fn host_shape_rejects_malformed_base_domain() {
        // Empty / single-label bases must never widen the suffix match.
        assert_eq!(parse_host_shape("foo.local", "local"), HostShape::Other);
        assert_eq!(parse_host_shape("foo", ""), HostShape::Other);
    }

    // ──────────────────────────────────────────────────────────────────
    // Edge-case / adversarial input coverage. The parser is *shape-only*:
    // it returns whatever non-empty, dot-free label sits in the slug
    // position. Slug content (charset, length, reserved-ness) is the
    // domain layer's job (`domain::org::validate_slug`) and the DB
    // lookup's job. Documenting that boundary here so a future
    // "harden the parser" instinct doesn't conflate the two layers.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn host_shape_passes_through_unusual_slug_chars() {
        // The extractor does not enforce the `[a-z0-9-]` slug shape;
        // rejection happens at signup (`validate_slug`), not here. The
        // parser's job is to not panic, not confuse the slug with the
        // base-domain suffix, and not silently rewrite the input.
        for slug in ["acme!", "acme$", "acme_under", "-leadhyphen", "1leaddigit"] {
            let host = format!("{slug}.example.com");
            assert_eq!(
                parse_host_shape(&host, "example.com"),
                HostShape::Subdomain(slug),
                "weird slug `{slug}` must still parse as Subdomain"
            );
        }
    }

    #[test]
    fn host_shape_passes_through_punycode() {
        // IDN-encoded hosts are pure ASCII and parse normally. They will
        // not match any stored slug (signup forbids `xn--…`), so the
        // outcome is a 404 — but the parser must not bail out earlier.
        assert_eq!(
            parse_host_shape("xn--acme-1bb.example.com", "example.com"),
            HostShape::Subdomain("xn--acme-1bb")
        );
    }

    #[test]
    fn host_shape_ipv6_bracketed_is_other() {
        // `Host: [::1]:443` — `split(':').next()` lands on `"["`, which
        // can never match the base. Must classify Other (→ marketing 404
        // via the dispatcher), never Subdomain.
        assert_eq!(
            parse_host_shape("[::1]:443", "example.com"),
            HostShape::Other
        );
        assert_eq!(
            parse_host_shape("[2001:db8::1]:8080", "example.com"),
            HostShape::Other
        );
    }

    #[test]
    fn host_shape_double_colon_keeps_first_authority() {
        // `acme.example.com:443:80` is malformed but a crafted client may
        // send it. `split(':').next()` returns `acme.example.com`, so the
        // parser silently normalises to the valid form — same tenant the
        // well-formed host would hit, no privilege escalation. Pinning
        // the behaviour here so a future "stricter parser" change is a
        // conscious decision, not an accident.
        assert_eq!(
            parse_host_shape("acme.example.com:443:80", "example.com"),
            HostShape::Subdomain("acme")
        );
    }

    #[test]
    fn host_shape_combines_all_normalisations_for_subdomain() {
        // Triple-stack: trailing dot + port + uppercase. `host_shape_apex`
        // pins the Apex branch; this pins the Subdomain branch — different
        // code paths through `parse_host_shape`, both must yield a slug
        // free of case/dot/port artefacts so a downstream lowercase
        // compare cannot 404 a real tenant.
        assert_eq!(
            parse_host_shape("ACME.EXAMPLE.COM.:443", "example.com"),
            HostShape::Subdomain("ACME")
        );
    }

    #[test]
    fn classify_phishing_like_slug_is_tenant_not_operator() {
        // `app-google.{base}`, `admin.{base}`, etc. LOOK operator-y but
        // are tenant-shaped slugs. They must NOT alias the operator
        // surface — they route to the public dispatcher and 404 unless
        // an org owns them (signup blocks the reserved ones at create
        // time). Mirrors the OPERATOR_LABELS-vs-reserved_slugs invariant
        // (host.rs:32-36 docstring) at the test layer.
        let s = scheme();
        for slug in ["app-google", "admin", "support", "login"] {
            let host = format!("{slug}.example.com");
            assert_eq!(
                classify_host(&host, &s),
                HostClass::TenantPublic,
                "{slug} must classify as tenant-public, not App"
            );
        }
    }

    fn scheme() -> HostScheme {
        HostScheme::from_base_domain("example.com").unwrap()
    }

    #[test]
    fn classify_apex_is_marketing() {
        assert_eq!(
            classify_host("example.com", &scheme()),
            HostClass::Marketing
        );
    }

    #[test]
    fn classify_www_is_marketing() {
        assert_eq!(
            classify_host("www.example.com", &scheme()),
            HostClass::Marketing
        );
    }

    #[test]
    fn classify_app_is_app() {
        assert_eq!(classify_host("app.example.com", &scheme()), HostClass::App);
    }

    #[test]
    fn classify_tenant_slug_is_tenant_public() {
        assert_eq!(
            classify_host("acme.example.com", &scheme()),
            HostClass::TenantPublic
        );
    }

    #[test]
    fn classify_strips_port() {
        assert_eq!(
            classify_host("example.com:8080", &scheme()),
            HostClass::Marketing
        );
        assert_eq!(
            classify_host("acme.example.com:443", &scheme()),
            HostClass::TenantPublic
        );
    }

    #[test]
    fn classify_strips_trailing_dot() {
        // FQDN form: a crafted curl can send `Host: acme.example.com.`.
        // Must classify identically — otherwise the dispatcher would
        // 404 a real tenant.
        assert_eq!(
            classify_host("acme.example.com.", &scheme()),
            HostClass::TenantPublic
        );
        assert_eq!(classify_host("app.example.com.", &scheme()), HostClass::App);
    }

    #[test]
    fn classify_is_case_insensitive() {
        assert_eq!(
            classify_host("ACME.Example.COM", &scheme()),
            HostClass::TenantPublic
        );
    }

    #[test]
    fn classify_deeper_subdomain_is_unknown() {
        // `a.b.example.com` must NOT alias a tenant slug; the dispatcher
        // hands `Unknown` to marketing 404, never to a tenant.
        assert_eq!(
            classify_host("a.b.example.com", &scheme()),
            HostClass::Unknown
        );
    }

    #[test]
    fn classify_unrelated_host_is_unknown() {
        assert_eq!(
            classify_host("acme.other.com", &scheme()),
            HostClass::Unknown
        );
        assert_eq!(classify_host("garbage", &scheme()), HostClass::Unknown);
    }

    #[test]
    fn classify_empty_host_is_unknown() {
        assert_eq!(classify_host("", &scheme()), HostClass::Unknown);
    }

    #[test]
    fn host_scheme_rejects_empty_or_single_label() {
        assert!(HostScheme::from_base_domain("").is_err());
        assert!(HostScheme::from_base_domain("   ").is_err());
        assert!(HostScheme::from_base_domain("local").is_err());
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn request_origin_accepts_this_deployments_hosts() {
        assert_eq!(
            request_origin(&headers(&[("host", "acme.example.com")]), "example.com").as_deref(),
            Some("https://acme.example.com")
        );
        assert_eq!(
            request_origin(&headers(&[("host", "example.com")]), "example.com").as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn request_origin_normalizes_the_host() {
        // Casing and the FQDN trailing dot name the same host; each would
        // otherwise publish its own spelling of every link.
        for host in ["ACME.Example.com", "acme.example.com."] {
            assert_eq!(
                request_origin(&headers(&[("host", host)]), "example.com").as_deref(),
                Some("https://acme.example.com"),
                "{host}"
            );
        }
    }

    #[test]
    fn request_origin_keeps_a_real_port_and_drops_a_bogus_one() {
        // A dev or self-host deploy off 443 publishes links that have to
        // resolve, so the port survives — but only when it is one.
        assert_eq!(
            request_origin(
                &headers(&[("host", "acme.example.com:8080")]),
                "example.com"
            )
            .as_deref(),
            Some("https://acme.example.com:8080")
        );
        for bogus in ["acme.example.com:", "acme.example.com:80a"] {
            assert_eq!(
                request_origin(&headers(&[("host", bogus)]), "example.com").as_deref(),
                Some("https://acme.example.com"),
                "{bogus}"
            );
        }
    }

    #[test]
    fn request_origin_rejects_a_host_this_deployment_does_not_serve() {
        // A forged Host would otherwise rewrite every link in a feed a reader
        // keeps, so anything off this deployment yields no origin at all.
        for host in ["evil.test", "a.b.example.com", ".example.com", ""] {
            assert_eq!(
                request_origin(&headers(&[("host", host)]), "example.com"),
                None,
                "{host}"
            );
        }
        assert_eq!(request_origin(&HeaderMap::new(), "example.com"), None);
    }

    #[test]
    fn request_origin_follows_the_proxys_scheme_but_only_a_real_one() {
        assert_eq!(
            request_origin(
                &headers(&[("host", "acme.example.com"), ("x-forwarded-proto", "http")]),
                "example.com"
            )
            .as_deref(),
            Some("http://acme.example.com")
        );
        assert_eq!(
            request_origin(
                &headers(&[
                    ("host", "acme.example.com"),
                    ("x-forwarded-proto", "javascript"),
                ]),
                "example.com"
            )
            .as_deref(),
            Some("https://acme.example.com")
        );
    }
}
