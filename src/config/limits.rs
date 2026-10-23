//! Plan quotas, rate limits, abuse rules and the API's own knobs.

use serde::{Deserialize, Serialize};

/// `[quotas]`. Cache TTLs for plan/usage lookups, and the plan the boot-seeded
/// owner's account opens on.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QuotasConfig {
    /// Plans change rarely; a few minutes of staleness is acceptable.
    pub plan_cache_ttl_secs: u64,
    /// Usage counts move fast under bursty creates; short TTL only.
    pub usage_cache_ttl_secs: u64,
    /// Plan the boot-seeded owner org lands on. Only the unattended first-run
    /// path reads it, so hosted is untouched: its orgs come from signup and keep
    /// the schema's `free`. Defaults to the largest plan because `free`'s
    /// ceilings bound what one tenant costs a shared platform, which nobody pays
    /// for on their own hardware.
    pub default_plan: String,
    /// Off on self-host: the operator owns the `plans` rows, so a feature gate
    /// read from them would only gate the operator against themselves.
    pub enforce_plan_features: bool,
}

impl Default for QuotasConfig {
    fn default() -> Self {
        Self {
            plan_cache_ttl_secs: 300,
            usage_cache_ttl_secs: 10,
            default_plan: "team".to_string(),
            enforce_plan_features: false,
        }
    }
}

/// `[rate_limits]`. Most numbers come from the `plans` table; these are the
/// janitor cadence and the per-IP values Caddy enforces (kept here for
/// reference / parity). Validated `>= 1` at load (I6).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RateLimitsConfig {
    pub per_ip: PerIpRateLimits,
    pub janitor: RateLimitJanitorConfig,
}

/// Per-IP limits Caddy enforces. Mirrored here so docs/ops have one place to
/// read the numbers; the app does not key on the TCP peer.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PerIpRateLimits {
    pub public_pages_per_min: u32,
    pub auth_endpoints_per_min: u32,
    pub org_creations_per_day: u32,
}

impl Default for PerIpRateLimits {
    fn default() -> Self {
        Self {
            public_pages_per_min: 60,
            auth_endpoints_per_min: 10,
            org_creations_per_day: 3,
        }
    }
}

/// Idle-entry janitor cadence for the in-process rate-limit map.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RateLimitJanitorConfig {
    pub cleanup_interval_hours: u64,
    pub idle_threshold_hours: u64,
}

impl Default for RateLimitJanitorConfig {
    fn default() -> Self {
        Self {
            cleanup_interval_hours: 6,
            idle_threshold_hours: 24,
        }
    }
}

/// `[abuse]`. URL-pattern deny-list (regex, case-insensitive) plus the path
/// to the YAML domain deny-list. Patterns are validated at config load
/// (`AbuseGuard::validate`) so a bad regex is a clean startup error, not a
/// construction panic. When `hot_reload_enabled`, SIGHUP re-reads the
/// patterns and deny-list file and swaps them in atomically (a malformed
/// edit is rejected and the running rules stay). When it is `false` no
/// SIGHUP handler is installed, so a deny-list edit needs a restart **and**
/// SIGHUP keeps its OS default of terminating the process immediately
/// (no graceful drain) — use SIGTERM/SIGINT to stop the server in that
/// mode, not SIGHUP.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AbuseConfig {
    pub url_patterns_denied: Vec<String>,
    pub domain_denylist_path: String,
    /// Optional hosts-format reputation feed (e.g. a StevenBlack/hosts
    /// mirror). Empty = the reputation check is off. Read locally at
    /// startup and on the SIGHUP reload — never fetched on the check path.
    pub reputation_source_path: String,
    pub hot_reload_enabled: bool,
}

impl Default for AbuseConfig {
    fn default() -> Self {
        Self {
            url_patterns_denied: default_url_patterns(),
            domain_denylist_path: "config/abuse_denylist.yaml".into(),
            reputation_source_path: String::new(),
            hot_reload_enabled: false,
        }
    }
}

/// Conservative reconnaissance / pen-test URL patterns. These match URLs that
/// are virtually always attack probes, never legitimate monitoring targets.
fn default_url_patterns() -> Vec<String> {
    [
        r"/\.git(/|$)",
        r"/\.env(/|$)",
        r"/\.svn(/|$)",
        r"/\.hg(/|$)",
        r"/\.(DS_Store|htaccess|htpasswd|npmrc|bash_history)(/|$)",
        r"/\.aws/",
        r"/\.ssh/",
        r"/phpmyadmin",
        r"/(adminer|dbadmin)(\.php)?",
        r"/wp-admin",
        r"/wp-login",
        r"/wp-config",
        r"/xmlrpc\.php",
        r"/cgi-bin/",
        r"/vendor/phpunit",
        r"/(phpinfo|info)\.php",
        r"/server-(status|info)",
        r"/actuator(/|$)",
        r"/manager/html",
        r"/solr/",
        r"/\.well-known/security\.txt",
        r"/autodiscover/autodiscover\.xml",
        r"\.(php|jsp|asp|aspx)\?",
        r"\.(sql|bak|old|swp|dump)(/|$|\?)",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ApiConfig {
    // Per-IP API rate limiting moved to Caddy (it sees the real peer); the
    // in-process limiter is now per-org / per-user via [rate_limits] and the
    // plans table. The old `api.rate_limit` (PeerIpKeyExtractor) layer is
    // gone — behind a proxy it collapsed to one global bucket.
    #[serde(default)]
    pub cors: CorsConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CorsConfig {
    pub enabled: bool,
    /// Origins allowed when `allow_any_origin` is false. Each entry must be a
    /// full origin (`https://app.example.com`) — wildcards are not parsed here.
    pub allowed_origins: Vec<String>,
    /// HTTP methods returned in `Access-Control-Allow-Methods`.
    pub allowed_methods: Vec<String>,
    /// When true, returns `Access-Control-Allow-Origin: *`. Mutually exclusive
    /// with `allowed_origins`.
    pub allow_any_origin: bool,
}

/// `[email_policy]`. Signup is a judgement call the operator owns, so it
/// defaults to `flag` and a list false-positive cannot lock out a paying
/// customer; the surfaces that mail an address always refuse.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EmailPolicyConfig {
    /// Off leaves both signals inert and starts no refresh job.
    pub enabled: bool,
    /// Union is taken, so a domain listed by any source counts.
    pub sources: Vec<String>,
    pub refresh_interval_hours: u64,
    /// `flag`, `block`, or `allow`.
    pub signup_policy: String,
    /// A bare A/AAAA counts, per RFC 5321. Fails open on resolver error: a
    /// DNS outage must not close signups.
    pub require_mx: bool,
    /// A list outside these bounds is rejected and the previous corpus stays
    /// live. Catches an upstream that breaks or truncates.
    pub min_domains: usize,
    pub max_domains: usize,
    pub max_shrink_pct: u8,
}

impl Default for EmailPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sources: vec![
                "https://raw.githubusercontent.com/disposable/disposable-email-domains/master/domains.txt".into(),
                "https://raw.githubusercontent.com/disposable-email-domains/disposable-email-domains/main/disposable_email_blocklist.conf".into(),
            ],
            refresh_interval_hours: 24,
            signup_policy: "flag".into(),
            require_mx: true,
            min_domains: 5_000,
            max_domains: 500_000,
            max_shrink_pct: 20,
        }
    }
}

/// Parsed at config validation, so a typo is a startup error rather than a
/// silently-permissive runtime default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignupPolicy {
    Allow,
    Flag,
    Block,
}

impl EmailPolicyConfig {
    pub fn signup_policy(&self) -> Option<SignupPolicy> {
        match self.signup_policy.trim() {
            "allow" => Some(SignupPolicy::Allow),
            "flag" => Some(SignupPolicy::Flag),
            "block" => Some(SignupPolicy::Block),
            _ => None,
        }
    }

    pub fn refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.refresh_interval_hours.max(1) * 3600)
    }
}
