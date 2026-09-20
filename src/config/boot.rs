//! Invariants a deployment must hold before it serves a request. Each refuses
//! to boot rather than run with a silent tenant leak or a connector that
//! mints tokens nobody accepts.

use super::AppConfig;

impl AppConfig {
    /// Run unconditionally at boot after config parse. Encodes the per-org
    /// public-surface and cookie-scope invariants in code so a misconfig is
    /// loud and immediate, not a silent runtime data leak. The two functions it
    /// calls are kept separate so cookie-scope can be exercised in isolation by
    /// tests.
    pub fn assert_per_org_status(&self) {
        if self.tenancy.subdomain_public_routes {
            let bd = self.public_status.base_domain.as_str();
            if bd.is_empty() || !bd.contains('.') {
                panic!(
                    "public_status.base_domain = {bd:?} is empty or missing a dot; \
                     subdomain routing cannot work safely"
                );
            }
        }
        self.assert_cookie_scope_safe();
    }

    /// Refuses to boot when the MCP OAuth server is enabled but its identity URIs
    /// are missing or not HTTPS. Without this the AS would mint tokens whose
    /// audience is the empty/wrong resource and the resource server would then
    /// reject them — a silently-broken connector. Both URIs are also the OAuth
    /// `issuer` / `resource` identifiers, which MUST be absolute HTTPS in
    /// production. Loopback HTTP is allowed for local development.
    pub fn assert_mcp_oauth(&self) {
        if !self.mcp.oauth_enabled {
            return;
        }
        let check = |label: &str, raw: &str| {
            let url = url::Url::parse(raw).unwrap_or_else(|_| {
                panic!(
                    "{label} must be a valid absolute URL when mcp.oauth_enabled = true (got {raw:?})"
                )
            });
            if url.scheme() != "https" && !crate::net::is_loopback_http(&url) {
                panic!(
                    "{label} must be https (or http on loopback for dev) when \
                     mcp.oauth_enabled = true (got {raw:?})"
                );
            }
        };
        if self.mcp.resource_uri.trim().is_empty() {
            panic!("mcp.oauth_enabled = true requires mcp.resource_uri to be set");
        }
        if self.auth.public_base_url.trim().is_empty() {
            panic!(
                "mcp.oauth_enabled = true requires auth.public_base_url (the OAuth issuer) to be set"
            );
        }
        check("mcp.resource_uri", &self.mcp.resource_uri);
        check("auth.public_base_url", &self.auth.public_base_url);
    }

    /// Refuses to boot when `auth.session.cookie_domain` overlaps the per-org
    /// status subdomain. Without this, a single config edit on the operator
    /// host can leak `_sm_session` to every tenant's status page.
    pub fn assert_cookie_scope_safe(&self) {
        let cookie_domain = self.auth.session.cookie_domain.as_str();
        if cookie_domain.is_empty() {
            return;
        }
        if !self.tenancy.subdomain_public_routes {
            return;
        }
        let base = self.public_status.base_domain.as_str();
        let cd = cookie_domain.trim_start_matches('.');
        if base == cd || base.ends_with(&format!(".{cd}")) {
            panic!(
                "auth.session.cookie_domain={cookie_domain:?} overlaps the \
                 status-page wildcard *.{base}. Operator session cookies would \
                 leak to every tenant's status page. Either unset cookie_domain, \
                 or move the status surface to a different parent zone."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::AppConfig;

    /// Run `f` with the default panic hook muted (so the expected-panic
    /// cases don't spam the log with backtraces) and assert it unwound with
    /// a message containing `expect`. Matching the message stops a test
    /// passing because it tripped a *different* boot assertion than intended.
    fn assert_panics(expect: &str, f: impl FnOnce() + std::panic::UnwindSafe) {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(f);
        std::panic::set_hook(prev);
        let payload = outcome.expect_err("expected a boot-refusing panic");
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        assert!(
            msg.contains(expect),
            "panicked, but on the wrong assertion: got {msg:?}, expected it to contain {expect:?}"
        );
    }

    /// A valid SaaS-subdomain baseline: subdomain routes on, path-based off,
    /// a two-label base domain, host-only cookies. Every field the assertions
    /// read is set explicitly so env/toml overrides can't make the tests
    /// non-deterministic. Each test then flips exactly the field under test
    /// off this safe starting point.
    fn saas_subdomain_cfg() -> AppConfig {
        let mut cfg = AppConfig::load().expect("config");
        cfg.tenancy.subdomain_public_routes = true;
        cfg.tenancy.path_based_public_routes = false;
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = String::new();
        cfg
    }

    fn oauth_on_cfg() -> AppConfig {
        let mut cfg = AppConfig::load().expect("config");
        cfg.mcp.oauth_enabled = true;
        cfg.mcp.resource_uri = "https://mcp.example.com/mcp".into();
        cfg.auth.public_base_url = "https://app.example.com".into();
        cfg
    }

    #[test]
    fn valid_saas_subdomain_config_passes() {
        saas_subdomain_cfg().assert_per_org_status();
    }

    #[test]
    fn empty_base_domain_with_subdomain_routes_panics() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = String::new();
        assert_panics("empty or missing a dot", move || {
            cfg.assert_per_org_status()
        });
    }

    #[test]
    fn single_label_base_domain_panics() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "local".into();
        assert_panics("empty or missing a dot", move || {
            cfg.assert_per_org_status()
        });
    }

    #[test]
    fn cookie_domain_overlapping_status_wildcard_panics() {
        // `.example.com` is also sent to `*.example.com`, so the operator
        // session would ride along to every tenant's page.
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = ".example.com".into();
        assert_panics("overlaps the", move || cfg.assert_cookie_scope_safe());
    }

    #[test]
    fn cookie_domain_equal_to_base_panics() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = "example.com".into();
        assert_panics("overlaps the", move || cfg.assert_cookie_scope_safe());
    }

    #[test]
    fn host_only_cookie_is_always_safe() {
        // Empty cookie_domain ⇒ browser scopes to the exact host; no overlap
        // is possible even with an otherwise dangerous base domain.
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = String::new();
        cfg.assert_cookie_scope_safe();
    }

    #[test]
    fn disjoint_cookie_domain_is_safe() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = ".other-zone.net".into();
        cfg.assert_cookie_scope_safe();
    }

    #[test]
    fn cookie_scope_unchecked_when_subdomain_routes_off() {
        // No public subdomains exist, so an overlapping cookie_domain has no
        // cross-tenant surface to leak onto.
        let mut cfg = saas_subdomain_cfg();
        cfg.tenancy.subdomain_public_routes = false;
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = ".example.com".into();
        cfg.assert_cookie_scope_safe();
    }

    #[test]
    fn oauth_config_valid_https_passes() {
        oauth_on_cfg().assert_mcp_oauth();
    }

    #[test]
    fn oauth_disabled_skips_all_checks() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.oauth_enabled = false;
        cfg.mcp.resource_uri = String::new();
        cfg.auth.public_base_url = String::new();
        cfg.assert_mcp_oauth();
    }

    #[test]
    fn oauth_on_with_empty_resource_panics() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.resource_uri = String::new();
        assert_panics("requires mcp.resource_uri", move || cfg.assert_mcp_oauth());
    }

    #[test]
    fn oauth_on_with_non_https_resource_panics() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.resource_uri = "http://mcp.example.com/mcp".into();
        assert_panics("must be https", move || cfg.assert_mcp_oauth());
    }

    #[test]
    fn oauth_on_with_loopback_http_issuer_passes() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.resource_uri = "http://localhost:9000/mcp".into();
        cfg.auth.public_base_url = "http://localhost:8080".into();
        cfg.assert_mcp_oauth();
    }
}
