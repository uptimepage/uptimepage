//! What the public sees: marketing, tenancy, status pages and how long history is kept.

use serde::{Deserialize, Serialize};

/// `[marketing]`. Optional apex/`www` marketing site + blog served from
/// the same binary. Hard-isolated module — see `src/marketing/`. Disabled
/// by default; when enabled, the dispatch seam routes the apex and `www`
/// hosts to the marketing router and leaves every other host on the app
/// router unchanged. Boot invariants live in `AppConfig::validate_marketing`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MarketingConfig {
    pub enabled: bool,
    /// CTA + login link target on every marketing page. The marketing
    /// module never imports app code — this is the only handle it has on
    /// the app surface, so the extracted service points anywhere with one
    /// config change.
    pub app_url: String,
    /// Fully-qualified canonical origin (scheme + host, no trailing
    /// slash). Used for `<link rel="canonical">`, OG / JSON-LD absolute
    /// URLs, and the sitemap.
    pub canonical_origin: String,
    /// Belt-and-braces guard for subdomain labels that must never alias a
    /// tenant slug (`www`, `app`). The dispatch seam already routes
    /// apex/`www`/`app` explicitly; this list is asserted to be a subset
    /// of `domain::reserved_slugs::RESERVED` at boot so the two lists
    /// can't drift.
    pub reserved_subdomains: Vec<String>,
    pub blog_enabled: bool,
}

impl Default for MarketingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_url: String::new(),
            canonical_origin: String::new(),
            reserved_subdomains: vec!["www".into(), "app".into()],
            blog_enabled: true,
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
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            login_attempts_days: 180,
            quota_events_days: 90,
            audit_log_days: 730,
            mcp_audit_days: 730,
            api_tokens_post_expiry_days: 30,
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
