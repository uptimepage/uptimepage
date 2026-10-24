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
use metrics::counter;

use crate::config::AppConfig;
use crate::domain::{OrgId, PageRef, StatusPageId};
use crate::error::public::PublicAppError;
use crate::metric_names;
use crate::request::RequestState;
use crate::request::is_health_path;

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

fn is_operator_label(slug: &str) -> bool {
    OPERATOR_LABELS.iter().any(|l| slug.eq_ignore_ascii_case(l))
}

/// Pure structural decomposition of a `Host` header against `{base_domain}`.
/// Single source of truth for shape rules (port strip, FQDN trailing dot,
/// case-insensitive suffix match, single-label slug). The host-isolation
/// surface and the tenant predicates (`extract_status_slug`,
/// `is_subdomain_public_request`, `request_origin`) build on this so they can
/// never drift on what "a tenant subdomain" means.
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
            // resolve. The scheme's default port goes, since a proxy that
            // forwards `Host` as `acme.example.com:443` names the same origin
            // and would otherwise put the port in every published link.
            // Anything but a valid port number is dropped rather than pasted
            // into a URL.
            let default_port = if scheme == "http" { 80 } else { 443 };
            let port = raw
                .split_once(':')
                .map(|(_, p)| p)
                .filter(|p| p.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|&p| p != 0 && p != default_port);
            let host = normalize_host(raw).to_ascii_lowercase();
            Some(match port {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            })
        }
        HostShape::Other => None,
    }
}

/// Origin a resolved page publishes links on: activated custom domain, else
/// the request origin, else the page's own subdomain.
///
/// [`request_origin`] returns `None` off the base domain, so a custom-domain
/// visitor would otherwise get the operator origin, which does not serve this
/// page. Activated rather than verified, for the reason on
/// `PAGE_CUSTOM_DOMAIN_PUBLISHED`.
pub fn published_page_origin(
    state: &RequestState,
    headers: &HeaderMap,
    page: StatusPageId,
) -> Option<String> {
    if let Some(domain) = state.custom_domains.published(page) {
        return Some(format!("https://{domain}"));
    }
    request_origin(headers, &state.cfg.public_status.base_domain).or_else(|| {
        state
            .custom_domains
            .subdomain(page)
            .map(|host| format!("https://{host}"))
    })
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
/// [`find_public_status_page_by_slug`] (filters `enabled = true`), or, for a
/// host off this deployment's base domain, the custom-domain snapshot. A
/// missing/garbled host or a disabled/unknown page is a 404 — the public
/// surface never confirms which pages exist. Path-based / self-host: the lone
/// live org's default (first enabled) page.
///
/// [`find_public_status_page_by_slug`]: crate::storage::orgs::find_public_status_page_by_slug
pub async fn resolve_status_page(
    state: &RequestState,
    headers: &HeaderMap,
) -> Result<PageRef, PublicAppError> {
    if !state.cfg.tenancy.subdomain_public_routes {
        return resolve_default_page(state).await;
    }

    let host = headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .ok_or(PublicAppError::NotFound)?;

    // A `{slug}.{base}` host never consults the snapshot, so a row naming a
    // tenant subdomain cannot shadow that tenant.
    let Some(parsed) = extract_status_slug(host, &state.cfg.public_status.base_domain) else {
        return state
            .custom_domains
            .lookup(host)
            .ok_or(PublicAppError::NotFound);
    };

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
async fn resolve_default_page(state: &RequestState) -> Result<PageRef, PublicAppError> {
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
    RequestState: FromRef<S>,
{
    type Rejection = PublicAppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = RequestState::from_ref(state);
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
pub fn is_subdomain_public_request(state: &RequestState, headers: &HeaderMap) -> bool {
    host_surface(state, headers) == HostSurface::Tenant
}

/// The slice of the app router a request's `Host` may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostSurface {
    /// Dashboard, auth, API: `app.{base}`, the apex, and every host once the
    /// SaaS subdomain surface is off, except the site's own.
    Operator,
    /// The public-status allow-list only.
    Tenant,
    /// The MCP transport and its discovery documents only.
    Mcp,
    /// Nothing at all.
    Unknown,
}

/// Tenant hosts exist only with the SaaS subdomain surface on; the MCP host
/// is narrowed whenever it is distinct from the app host ([`is_mcp_host`]).
///
/// On SaaS an unrecognised host reaches nothing. On a path-based self-host
/// deploy every host is legitimately the operator, so the default stands,
/// except on the site's own hosts
/// ([`SiteConfig::covers`](crate::config::SiteConfig::covers)).
fn host_surface(state: &RequestState, headers: &HeaderMap) -> HostSurface {
    let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else {
        // Caddy talks HTTP/1.1 upstream, which cannot omit `Host`, and the app
        // port is not exposed past the docker network.
        return HostSurface::Operator;
    };
    if is_mcp_host(&state.cfg, host) {
        return HostSurface::Mcp;
    }
    // The site runs apart from the app; answering its host would put the
    // login on the site's name the moment the proxy misroutes one request.
    if state.cfg.site.covers(host) {
        return HostSurface::Unknown;
    }
    let subdomain_routes = state.cfg.tenancy.subdomain_public_routes;
    // Slug-shaped non-operator hosts (`www` included) belong on the public
    // dispatcher. Routing them to the operator `/` dashboard would expose the
    // login surface on attacker-controlled labels (`admin.{base}`,
    // `www.{base}`).
    match parse_host_shape(host, &state.cfg.public_status.base_domain) {
        HostShape::Subdomain(slug) if subdomain_routes && !is_operator_label(slug) => {
            HostSurface::Tenant
        }
        HostShape::Other => match state.custom_domains.lookup(host) {
            Some(_) => HostSurface::Tenant,
            None if subdomain_routes => HostSurface::Unknown,
            // Path-based self-host answers on whatever host it is reached by.
            None => HostSurface::Operator,
        },
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
/// here too. Mirrors the [`HEALTH_PATHS`] pattern. Spelled out rather
/// than pulled from `security::disclosure::PATH` on purpose: this list
/// is audited by eye, and an opaque const hides a host's exposed
/// surface from the person reading it.
const PUBLIC_TENANT_EXACT: &[&str] = &["/", "/status", "/subscribe", "/.well-known/security.txt"];

/// Path prefixes the public tenant surface is allowed to expose. No
/// `/robots.txt` or `/sitemap.xml` by design — tenant pages aren't
/// crawl targets, and the public-status template emits its own meta.
/// No general `/.well-known/*` — only `security.txt` is allow-listed,
/// ACME challenges run on the apex/`app.{base}` host. `/m/` answers only
/// for the host's own page's detail links (`web::views::share`).
const PUBLIC_TENANT_PREFIXES: &[&str] = &[
    "/access/",
    "/status/",
    "/subscribe/",
    "/api/public/v1/",
    "/static/",
    "/m/",
];

fn is_public_tenant_path(path: &str) -> bool {
    PUBLIC_TENANT_EXACT.contains(&path)
        || PUBLIC_TENANT_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// A request for a page drawn in the operator app's chrome, never a public one.
pub fn renders_app_chrome(state: &RequestState, headers: &HeaderMap, path: &str) -> bool {
    host_surface(state, headers) == HostSurface::Operator && is_app_path(path)
}

/// `/` is the dashboard on the operator host; the other public tenant paths
/// are served there too.
fn is_app_path(path: &str) -> bool {
    path == "/" || !is_public_tenant_path(path)
}

/// The transport itself plus its discovery documents: the RFC 9728 / RFC 8414
/// metadata and the server card are all `/.well-known/` files, public by
/// definition.
fn is_mcp_host_path(path: &str) -> bool {
    path == crate::request::MCP_PATH || path.starts_with("/.well-known/")
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
pub async fn host_isolation(
    State(state): State<RequestState>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if !is_health_path(path) {
        let surface = host_surface(&state, req.headers());
        let allowed = match surface {
            HostSurface::Operator => true,
            HostSurface::Tenant => is_public_tenant_path(path),
            HostSurface::Mcp => is_mcp_host_path(path),
            HostSurface::Unknown => false,
        };
        if !allowed {
            if surface == HostSurface::Unknown {
                counter!(metric_names::UNRECOGNISED_HOST_REQUESTS).increment(1);
            }
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allow-list spells the path out for auditability, so this is
    /// what stops it drifting from the const the routers mount.
    #[test]
    fn tenant_allow_list_matches_the_mounted_security_txt_path() {
        assert!(
            PUBLIC_TENANT_EXACT.contains(&crate::security::disclosure::PATH),
            "security.txt is mounted at a path no tenant host allows"
        );
    }

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
    fn public_paths_never_count_as_app_pages() {
        for path in ["/", "/targets", "/maintenance/new", "/settings/account"] {
            assert!(is_app_path(path), "{path}");
        }
        for path in [
            "/status",
            "/status/incidents/1",
            "/subscribe/confirm",
            "/api/public/v1/status",
            "/static/css/app.css",
            "/.well-known/security.txt",
            "/m/token",
        ] {
            assert!(!is_app_path(path), "{path}");
        }
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
        // can never match the base. Must classify Other, never Subdomain.
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
        for bogus in [
            "acme.example.com:",
            "acme.example.com:80a",
            "acme.example.com:+8080",
            "acme.example.com:0",
            "acme.example.com:65536",
        ] {
            assert_eq!(
                request_origin(&headers(&[("host", bogus)]), "example.com").as_deref(),
                Some("https://acme.example.com"),
                "{bogus}"
            );
        }
    }

    #[test]
    fn request_origin_drops_the_schemes_default_port() {
        // A proxy that forwards Host with the default port names the same
        // origin; keeping it would put `:443` in every canonical, og:url and
        // feed link the page publishes.
        for host in ["acme.example.com:443", "acme.example.com:0443"] {
            assert_eq!(
                request_origin(&headers(&[("host", host)]), "example.com").as_deref(),
                Some("https://acme.example.com"),
                "{host}"
            );
        }
        assert_eq!(
            request_origin(
                &headers(&[
                    ("host", "acme.example.com:80"),
                    ("x-forwarded-proto", "http")
                ]),
                "example.com"
            )
            .as_deref(),
            Some("http://acme.example.com")
        );
        // Only the default for the scheme in use: 443 under http is a real port.
        assert_eq!(
            request_origin(
                &headers(&[
                    ("host", "acme.example.com:443"),
                    ("x-forwarded-proto", "http")
                ]),
                "example.com"
            )
            .as_deref(),
            Some("http://acme.example.com:443")
        );
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
