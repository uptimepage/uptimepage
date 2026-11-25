//! Where a status page lives: the origin it is served from and the URLs the
//! operator UI, the API and the templates all have to agree on.

use std::fmt::Display;

/// The route parameter that names a page in a path-based URL.
pub const PAGE_PARAM: &str = "page";

/// Where a status page's public surface sits on the host serving it, for both
/// the links a page renders and the routes that answer them.
///
/// A page host (`{slug}.{base_domain}` or a custom domain) gives the page its
/// root. A path-based deploy shares the operator host, whose root is the
/// dashboard: every page lives under its own `/status/{slug}`, and `/status`
/// leads to the primary one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PagePaths {
    home: String,
    /// Prefix of what hangs off the page: archive, incidents, logo.
    status: String,
    /// Prefix of what a page host serves at its root: subscribe, sign-in and
    /// the public API.
    root: String,
}

impl PagePaths {
    pub fn host() -> Self {
        Self {
            home: "/".to_owned(),
            status: "/status".to_owned(),
            root: String::new(),
        }
    }

    /// Where the primary page's unscoped routes sit, beside the dashboard.
    pub fn primary() -> Self {
        Self {
            home: "/status".to_owned(),
            status: "/status".to_owned(),
            root: String::new(),
        }
    }

    pub fn scoped(slug: &str) -> Self {
        let base = format!("/status/{slug}");
        Self {
            home: base.clone(),
            status: base.clone(),
            root: base,
        }
    }

    /// [`Self::scoped`] as route patterns, the slug captured as [`PAGE_PARAM`].
    pub fn scoped_routes() -> Self {
        Self::scoped(&format!("{{{PAGE_PARAM}}}"))
    }

    /// Where mail, the editor and the API send readers, having no request to
    /// follow: a page host, or the page's own path on a path-based deploy.
    pub fn published(subdomain_routes: bool, slug: &str) -> Self {
        if subdomain_routes {
            Self::host()
        } else {
            Self::scoped(slug)
        }
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    pub fn archive(&self) -> String {
        format!("{}/incidents", self.status)
    }

    pub fn incident(&self, id: impl Display) -> String {
        format!("{}/incidents/{id}", self.status)
    }

    pub fn logo(&self) -> String {
        format!("{}/branding/logo", self.status)
    }

    pub fn subscribe(&self) -> String {
        format!("{}/subscribe", self.root)
    }

    /// Prefix of the sign-in routes, which their short-lived cookies are
    /// scoped to.
    pub fn access_root(&self) -> String {
        format!("{}/access", self.root)
    }

    pub fn access(&self, action: &str) -> String {
        format!("{}/{action}", self.access_root())
    }

    pub fn api(&self) -> String {
        format!("{}/api/public/v1", self.root)
    }

    pub fn rss(&self) -> String {
        format!("{}/incidents.rss", self.api())
    }

    pub fn badge(&self) -> String {
        format!("{}/badge.svg", self.api())
    }

    /// The path that sets this page apart from others on its host; empty
    /// where the page has the host to itself or is the primary one.
    pub fn scope(&self) -> &str {
        &self.root
    }

    /// Scope of the page's sign-in cookie, so two pages sharing a host keep
    /// their sessions apart.
    pub fn cookie_path(&self) -> &str {
        if self.root.is_empty() {
            "/"
        } else {
            &self.root
        }
    }

    /// The page's absolute URL on `origin`; a page host's is the bare origin.
    pub fn url(&self, origin: &str) -> String {
        if self.home == "/" {
            origin.to_owned()
        } else {
            format!("{origin}{}", self.home)
        }
    }
}

/// What `path` asks of the page it is scoped to: `/subscribe` for
/// `/status/acme/subscribe`.
pub fn within_scoped_page(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/status/")?;
    rest.find('/').map(|at| &rest[at..])
}

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

/// Where a page's links point when no request says how it was reached.
pub fn published_paths(cfg: &crate::config::AppConfig, slug: &str) -> PagePaths {
    PagePaths::published(cfg.tenancy.subdomain_public_routes, slug)
}

/// Public page URL from an origin: the apex in subdomain mode,
/// `{origin}/status/{slug}` in path mode.
pub fn public_status_url(cfg: &crate::config::AppConfig, origin: &str, slug: &str) -> String {
    published_paths(cfg, slug).url(origin)
}

/// Logo URL stamped with the asset's content hash (cache-buster), or `None`
/// when no public surface is mounted.
pub fn public_logo_url(base: Option<&str>, paths: &PagePaths, hash: &str) -> Option<String> {
    base.map(|origin| format!("{origin}{}?v={hash}", paths.logo()))
}

/// `.{base_domain}` slug-preview suffix in subdomain mode; `None` in path mode.
pub fn public_host_suffix(cfg: &crate::config::AppConfig) -> Option<String> {
    cfg.tenancy
        .subdomain_public_routes
        .then(|| format!(".{}", cfg.public_status.base_domain.trim_start_matches('.')))
}

/// Origin a status page publishes links on: its activated custom domain, else
/// its `{slug}.{base_domain}` subdomain where pages have hosts of their own
/// (`subdomain_base`), else `public_base_url` for path-based/self-host deploys.
pub fn page_origin(
    subdomain_base: Option<&str>,
    public_base_url: &str,
    slug: &str,
    custom_domain: Option<&str>,
    custom_domain_published: bool,
) -> String {
    if let Some(domain) = custom_domain.filter(|_| custom_domain_published) {
        return format!("https://{domain}");
    }
    if let Some(base) = subdomain_base.map(str::trim).filter(|b| !b.is_empty()) {
        return format!("https://{slug}.{base}");
    }
    public_base_url.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_host_keeps_the_page_at_its_root_and_the_rest_under_status() {
        let p = PagePaths::host();
        assert_eq!(p.home(), "/");
        assert_eq!(p.archive(), "/status/incidents");
        assert_eq!(p.incident(7), "/status/incidents/7");
        assert_eq!(p.logo(), "/status/branding/logo");
        assert_eq!(p.subscribe(), "/subscribe");
        assert_eq!(p.access("code"), "/access/code");
        assert_eq!(p.rss(), "/api/public/v1/incidents.rss");
        assert_eq!(p.cookie_path(), "/");
        assert_eq!(
            p.url("https://acme.example.com"),
            "https://acme.example.com"
        );
    }

    #[test]
    fn the_primary_page_answers_at_status_beside_the_dashboard() {
        let p = PagePaths::primary();
        assert_eq!(p.home(), "/status");
        assert_eq!(p.archive(), "/status/incidents");
        assert_eq!(p.subscribe(), "/subscribe");
        assert_eq!(p.badge(), "/api/public/v1/badge.svg");
        assert_eq!(p.cookie_path(), "/");
        assert_eq!(p.url("https://ops.test"), "https://ops.test/status");
    }

    #[test]
    fn a_scoped_page_keeps_its_whole_surface_under_its_slug() {
        let p = PagePaths::scoped("two");
        assert_eq!(p.home(), "/status/two");
        assert_eq!(p.archive(), "/status/two/incidents");
        assert_eq!(p.incident(7), "/status/two/incidents/7");
        assert_eq!(p.logo(), "/status/two/branding/logo");
        assert_eq!(p.subscribe(), "/status/two/subscribe");
        assert_eq!(p.access("sign-out"), "/status/two/access/sign-out");
        assert_eq!(p.rss(), "/status/two/api/public/v1/incidents.rss");
        assert_eq!(p.cookie_path(), "/status/two");
        assert_eq!(p.scope(), "/status/two");
        assert_eq!(p.url("https://ops.test"), "https://ops.test/status/two");
    }

    #[test]
    fn scoped_routes_capture_the_slug() {
        let p = PagePaths::scoped_routes();
        assert_eq!(p.home(), "/status/{page}");
        assert_eq!(p.incident("{id}"), "/status/{page}/incidents/{id}");
        assert_eq!(p.api(), "/status/{page}/api/public/v1");
    }

    #[test]
    fn published_links_name_the_page_on_a_path_based_deploy() {
        assert_eq!(PagePaths::published(true, "two"), PagePaths::host());
        assert_eq!(PagePaths::published(false, "two"), PagePaths::scoped("two"));
    }

    #[test]
    fn a_path_based_deploy_links_its_own_host_whatever_the_base_domain() {
        assert_eq!(
            page_origin(None, "https://ops.test/", "two", None, false),
            "https://ops.test"
        );
        assert_eq!(
            page_origin(Some("example.com"), "https://ops.test", "two", None, false),
            "https://two.example.com"
        );
    }

    #[test]
    fn within_scoped_page_strips_the_page() {
        assert_eq!(
            within_scoped_page("/status/two/subscribe"),
            Some("/subscribe")
        );
        assert_eq!(
            within_scoped_page("/status/two/access/code"),
            Some("/access/code")
        );
        assert_eq!(within_scoped_page("/status/two"), None);
        assert_eq!(within_scoped_page("/subscribe"), None);
    }
}
