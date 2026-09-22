//! The process itself: server, runtime, checker, HTTP client, DNS, security and scheduling.

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use super::{empty_secret, secret_str};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub api_bind: String,
    pub metrics_bind: String,
    /// Where Caddy's on-demand TLS `ask` reaches this process. Empty starts no
    /// listener. Internal network only.
    #[serde(default)]
    pub custom_domain_ask_bind: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimeConfig {
    pub worker_threads: usize,
    pub max_blocking_threads: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CheckerConfig {
    pub max_concurrent_checks: usize,
    pub default_timeout_ms: u64,
    pub connect_timeout_ms: u64,
    pub default_check_interval_secs: u64,
    /// Per-(org, host, port) in-flight cap. Tenant-scoped, fail-fast.
    #[serde(default = "default_per_host_max_inflight")]
    pub per_host_max_inflight: usize,
    /// Process-wide RDAP concurrency cap (per TLD).
    #[serde(default = "default_rdap_max_inflight")]
    pub rdap_max_inflight: usize,
}

fn default_per_host_max_inflight() -> usize {
    2
}

fn default_rdap_max_inflight() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpClientConfig {
    /// TCP keep-alive for the in-flight connection. Checks connect fresh each
    /// run (no pool), so this only spans one request's body read.
    pub tcp_keepalive_secs: u64,
    /// Identifiable so site owners allowlist our probes instead of blocking them.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
}

/// The crawler convention (`Googlebot` and friends use the same shape). The
/// `Mozilla/5.0 (compatible; …)` prefix is what CDNs parse before deciding
/// whether to compress a response — without it some origins answer a probe with
/// megabytes of uncompressed HTML — while `compatible` and the product token
/// keep the claim honest: no browser engine is named.
pub(super) fn default_user_agent() -> String {
    concat!(
        "Mozilla/5.0 (compatible; uptimepage/",
        env!("CARGO_PKG_VERSION"),
        "; +https://uptimepage.dev/bot)"
    )
    .to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DnsConfig {
    pub cache_size: usize,
    pub positive_ttl_secs: u64,
    pub negative_ttl_secs: u64,
    pub servers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityConfig {
    pub allow_private_targets: bool,
    #[serde(default = "empty_secret", with = "secret_str")]
    pub credentials_kek_base64: SecretString,
    /// CIDR ranges whose `X-Forwarded-For` header is honoured for client-IP
    /// extraction. The TCP peer's address is checked against this list; if
    /// it matches, the rightmost untrusted hop in XFF wins. Anything else
    /// falls back to the TCP peer (no spoofable header). Empty by default
    /// — operators behind a reverse proxy (Caddy / nginx / a CDN) MUST set
    /// this, otherwise every `ip_hash` written to the database collapses to
    /// the proxy's address and IP-keyed abuse/audit signals are useless.
    #[serde(default)]
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

impl SecurityConfig {
    /// Returns Some(trimmed KEK string) if a non-empty value is configured, None otherwise.
    pub fn kek(&self) -> Option<&str> {
        let t = self.credentials_kek_base64.expose_secret().trim();
        (!t.is_empty()).then_some(t)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    pub success_threshold: u32,
    pub open_duration_secs: u64,
    pub half_open_max_calls: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    /// Off = this process probes nothing in-process (pure dashboard/brain);
    /// agents do all probing. On = the in-process scheduler probes `region`.
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
    pub target_refresh_interval_secs: u64,
    /// This control plane's own region id. Its scheduler runs the targets
    /// assigned to this region and stamps results with it — the same query an
    /// agent pulls for its region. Boot reconciles the row into `regions`.
    #[serde(default = "default_region_id")]
    pub region: String,
    /// Region assigned to newly-created targets. Empty falls back to `region`.
    #[serde(default)]
    pub default_region: String,
}

fn default_region_id() -> String {
    "default".to_string()
}

fn default_scheduler_enabled() -> bool {
    true
}

impl SchedulerConfig {
    /// Region new targets are assigned to: explicit `default_region`, else the
    /// control plane's own `region`.
    pub fn effective_default_region(&self) -> &str {
        if self.default_region.trim().is_empty() {
            &self.region
        } else {
            &self.default_region
        }
    }
}
