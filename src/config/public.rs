//! What the public sees: tenancy, status pages and how long history is kept.

use serde::{Deserialize, Serialize};

/// `[site]`. The website that serves this deployment's marketing pages.
/// Empty when there is none.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SiteConfig {
    /// A bare origin, as `url::Origin` serialises it.
    pub origin: String,
}

impl SiteConfig {
    /// `None` unless the origin is written exactly as it serialises. Cookies
    /// ignore the port, so this is what a cookie domain is matched against.
    pub fn host(&self) -> Option<String> {
        let url = url::Url::parse(&self.origin).ok()?;
        let bare = matches!(url.scheme(), "http" | "https")
            && url.origin().ascii_serialization() == self.origin;
        url.host_str().filter(|_| bare).map(str::to_owned)
    }

    /// True when a `Host` value names the site or its `www` alias, which the
    /// site serves and the app never does. A `Host` without a port means the
    /// site scheme's default one.
    pub fn covers(&self, authority: &str) -> bool {
        let Ok(site) = url::Url::parse(&self.origin) else {
            return false;
        };
        let Some(site_host) = site.host_str() else {
            return false;
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                match port.parse::<u16>() {
                    Ok(port) => (host, Some(port)),
                    Err(_) => return false,
                }
            }
            _ => (authority, None),
        };
        if port != site.port() && port != site.port_or_known_default() {
            return false;
        }
        let host = host.strip_suffix('.').unwrap_or(host);
        let www_alias = host
            .get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("www."))
            .and_then(|_| host.get(4..))
            .is_some_and(|rest| rest.eq_ignore_ascii_case(site_host));
        host.eq_ignore_ascii_case(site_host) || www_alias
    }
}

/// `[legal]`. The operator's own policies, linked from the app. Each URL is
/// optional; a missing one is neither linked nor recorded as accepted.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LegalConfig {
    pub terms_url: String,
    pub privacy_url: String,
    pub cookies_url: String,
    pub refund_url: String,
    pub impressum_url: String,
    pub abuse_url: String,
    pub security_policy_url: String,
    /// Recorded on every account created while `terms_url` is set.
    pub terms_version: String,
    /// Recorded on every account created while `privacy_url` is set.
    pub privacy_version: String,
}

impl LegalConfig {
    /// `(key, footer label, url)` in footer order.
    pub fn policies(&self) -> [(&'static str, &'static str, &str); 7] {
        [
            ("terms_url", "Terms", &self.terms_url),
            ("privacy_url", "Privacy", &self.privacy_url),
            ("refund_url", "Refunds", &self.refund_url),
            ("cookies_url", "Cookies", &self.cookies_url),
            ("impressum_url", "Impressum", &self.impressum_url),
            ("abuse_url", "Abuse", &self.abuse_url),
            ("security_policy_url", "Security", &self.security_policy_url),
        ]
    }

    pub fn consent(&self) -> crate::domain::consent::Consent<'_> {
        fn shown<'a>(url: &str, version: &'a str) -> Option<&'a str> {
            (!url.is_empty()).then_some(version)
        }
        crate::domain::consent::Consent {
            terms: shown(&self.terms_url, &self.terms_version),
            privacy: shown(&self.privacy_url, &self.privacy_version),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TenancyConfig {
    /// Path-based public surface (`/status/<slug>`, `/api/public/v1/*` on the
    /// operator host). The slug always identifies the org; there is no
    /// ambient "default tenant".
    pub path_based_public_routes: bool,
    /// Wildcard subdomain public surface (`*.{public_status.base_domain}`).
    /// Requires a well-formed `public_status.base_domain`; a startup
    /// assertion refuses to boot otherwise.
    pub subdomain_public_routes: bool,
    /// Grace period before soft-deleted orgs *and users* are purged. Single
    /// source of truth for the recovery window: the daily retention job binds
    /// this, and the Privacy Policy's "recoverable for 30 days" line is
    /// asserted equal to it in tests.
    pub deletion_grace_period_days: u32,
}

impl TenancyConfig {
    /// Whether *any* public surface is mounted. The two are mutually exclusive
    /// in practice (the startup assertions forbid path-based + SaaS, and
    /// subdomain needs SaaS), so the shared handlers resolve the org via the
    /// host-aware extractor and only one surface is ever live per deployment.
    pub fn public_routes_active(&self) -> bool {
        self.path_based_public_routes || self.subdomain_public_routes
    }
}

impl Default for TenancyConfig {
    fn default() -> Self {
        Self {
            path_based_public_routes: true,
            subdomain_public_routes: false,
            deletion_grace_period_days: 30,
        }
    }
}

/// Raw check results and the 1-minute rollup: a ClickHouse TTL set by the
/// migrations rather than config, and quoted by the hosted Privacy Policy.
pub const CHECK_RESULTS_RAW_DAYS: u32 = 30;
/// The hourly rollup's ClickHouse TTL, which carries the history.
pub const CHECK_RESULTS_HISTORY_MONTHS: u32 = 13;
/// Flow failure evidence and heartbeat output: content from behind the
/// customer's own login, so it goes sooner than the signal.
pub const FLOW_EVIDENCE_DAYS: u32 = 7;

/// Long-horizon data-retention windows for the daily purge job. Every field
/// here is bound by `jobs::retention`; an unhonoured knob is worse than a
/// missing one, so `check_results_days` lives only in the ClickHouse
/// migration TTL (an env override here would have been silently ignored —
/// the TTL is baked at migration time, not re-issued as an ALTER on boot).
/// Other cadences live with their owner: OAuth-state and magic-link tokens
/// in their own short-cadence security jobs; expired invitations in the
/// invitations janitor; session idle/absolute timeouts in `[auth.session]`;
/// soft-deleted org/user grace in `tenancy.deletion_grace_period_days`;
/// server/app log retention in the Docker log driver.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// `login_attempts` rows older than this are deleted.
    pub login_attempts_days: u32,
    /// `quota_events` rows older than this are deleted.
    pub quota_events_days: u32,
    /// `org_audit_log` rows older than this are deleted.
    pub audit_log_days: u32,
    /// `mcp_audit` rows older than this are deleted. A row identifies what the
    /// tool acted on, which for a monitor means its name and address, so the
    /// trail gets a bounded life rather than outliving every other record.
    pub mcp_audit_days: u32,
    /// Days after an API token's `expires_at` before its row is
    /// hard-deleted. Live tokens never count against the per-user cap
    /// (`api_tokens::count_for_user` filters by expiry) so the only purpose
    /// of this window is to bound table growth and shrink the
    /// rotation-pattern leak from a compromised user reading their own
    /// `token_prefix` / `name` history.
    pub api_tokens_post_expiry_days: u32,
    /// Private status page sign-in history: `status_page_access_events`, the
    /// per-day visit rows, and viewer sessions left unused this long.
    pub page_access_events_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            login_attempts_days: 180,
            quota_events_days: 90,
            audit_log_days: 730,
            mcp_audit_days: 730,
            api_tokens_post_expiry_days: 30,
            page_access_events_days: 365,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PublicStatusConfig {
    /// Base domain for `*.{base_domain}` per-org status pages (apex-wildcard
    /// shape). Used only when `tenancy.subdomain_public_routes = true`. A
    /// startup assertion refuses to boot when this is empty or has no dot in
    /// that mode — without that, the strip-suffix parser collapses to a bare
    /// dot match and accepts arbitrary `Host` headers.
    pub base_domain: String,

    pub cache_max_orgs: u32,
    pub cache_ttl_secs: u64,
    /// Idle eviction caps memory when tenants churn faster than the purge
    /// worker can reach them.
    pub last_good_ttl_secs: u64,

    pub max_logo_size_bytes: u32,
    pub allowed_logo_mime_types: Vec<String>,
    pub max_logo_dimension_px: u32,

    pub default_brand_color: String,
    pub default_show_powered_by: bool,

    /// Second line of defence behind the Caddy-side limit.
    pub public_per_ip_rate_limit_per_min: u32,
}

impl Default for PublicStatusConfig {
    fn default() -> Self {
        Self {
            base_domain: String::new(),
            cache_max_orgs: 1000,
            cache_ttl_secs: 10,
            last_good_ttl_secs: 3600,
            max_logo_size_bytes: 1_048_576,
            allowed_logo_mime_types: vec![
                "image/png".into(),
                "image/jpeg".into(),
                "image/webp".into(),
            ],
            max_logo_dimension_px: 1200,
            default_brand_color: "#3b82f6".into(),
            default_show_powered_by: true,
            public_per_ip_rate_limit_per_min: 60,
        }
    }
}
