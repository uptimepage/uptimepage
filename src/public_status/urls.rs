//! Where a status page lives: the origin it is served from and the URLs the
//! operator UI, the API and the templates all have to agree on.

/// Single source of truth for the logo path — referenced by both the route
/// registration and the URL the template emits so they cannot drift.
pub const LOGO_ROUTE: &str = "/status/branding/logo";

/// Origin a page is served from: an absolute host in subdomain mode, an empty
/// string (same operator host) in path mode, or `None` when no public surface
/// is mounted. Single source for both the API view and the settings editor so
/// their URLs can't diverge.
pub fn public_base(cfg: &crate::config::AppConfig, slug: &str) -> Option<String> {
    if cfg.tenancy.subdomain_public_routes {
        return Some(format!("https://{slug}.{}", cfg.public_status.base_domain));
    }
    if cfg.tenancy.path_based_public_routes {
        return Some(String::new());
    }
    None
}

/// Public page URL from an origin: the apex in subdomain mode, `{origin}/status`
/// in path mode.
pub fn public_status_url(cfg: &crate::config::AppConfig, origin: &str) -> String {
    status_url_for(cfg.tenancy.subdomain_public_routes, origin)
}

/// Same rule for callers that carry the tenancy flag instead of the whole
/// config. One predicate: a subdomain deploy gives the page a host of its own,
/// a path deploy shares the operator host, whose root is the dashboard.
pub fn status_url_for(subdomain_routes: bool, origin: &str) -> String {
    if subdomain_routes {
        origin.to_owned()
    } else {
        format!("{origin}/status")
    }
}

/// Logo URL stamped with the asset's content hash (cache-buster), or `None`
/// when no public surface is mounted.
pub fn public_logo_url(base: Option<&str>, hash: &str) -> Option<String> {
    base.map(|origin| format!("{origin}{LOGO_ROUTE}?v={hash}"))
}

/// `.{base_domain}` slug-preview suffix in subdomain mode; `None` in path mode.
pub fn public_host_suffix(cfg: &crate::config::AppConfig) -> Option<String> {
    cfg.tenancy
        .subdomain_public_routes
        .then(|| format!(".{}", cfg.public_status.base_domain.trim_start_matches('.')))
}

/// Origin a status page publishes links on: its activated custom domain, else
/// its `{slug}.{base_domain}` subdomain, else `public_base_url` for
/// path-based/self-host deploys.
pub fn page_origin(
    base_domain: &str,
    public_base_url: &str,
    slug: &str,
    custom_domain: Option<&str>,
    custom_domain_published: bool,
) -> String {
    if let Some(domain) = custom_domain.filter(|_| custom_domain_published) {
        return format!("https://{domain}");
    }
    let base = base_domain.trim();
    if !base.is_empty() {
        return format!("https://{slug}.{base}");
    }
    public_base_url.trim_end_matches('/').to_string()
}
