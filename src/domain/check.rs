use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum CheckSpec {
    Http(HttpCheck),
    Tcp(TcpCheck),
    Ping(PingCheck),
    Heartbeat(HeartbeatCheck),
    TlsCert(TlsCertCheck),
    DomainExpiry(DomainExpiryCheck),
    Dns(DnsCheck),
    Flow(FlowCheck),
}

impl CheckSpec {
    /// Every kind string `kind()` can return. Bounded set — safe as a metric
    /// label and lets inventory emit a 0 for kinds with no enabled monitors.
    pub const ALL_KINDS: [&'static str; 8] = [
        "http",
        "tcp",
        "ping",
        "heartbeat",
        "dns",
        "tls_cert",
        "domain_expiry",
        "flow",
    ];

    pub fn kind(&self) -> &'static str {
        match self {
            CheckSpec::Http(_) => "http",
            CheckSpec::Tcp(_) => "tcp",
            CheckSpec::Ping(_) => "ping",
            CheckSpec::Heartbeat(_) => "heartbeat",
            CheckSpec::Dns(_) => "dns",
            CheckSpec::TlsCert(_) => "tls_cert",
            CheckSpec::DomainExpiry(_) => "domain_expiry",
            CheckSpec::Flow(_) => "flow",
        }
    }

    /// A passive kind evaluates in-memory state instead of probing the
    /// network: no circuit breaker, no host throttle, never runs on agents.
    pub fn is_passive(&self) -> bool {
        matches!(self, CheckSpec::Heartbeat(_))
    }

    pub fn as_heartbeat(&self) -> Option<&HeartbeatCheck> {
        match self {
            CheckSpec::Heartbeat(h) => Some(h),
            _ => None,
        }
    }

    /// The host this check concerns, IPv6 brackets stripped. `None` for
    /// heartbeat, which is inbound-only. Distinct from
    /// [`crate::worker::host_throttle::host_port_raw`], which keys a *network endpoint* and
    /// so excludes dns/domain_expiry: their subject is a name, not a socket.
    pub fn primary_host(&self) -> Option<&str> {
        match self {
            CheckSpec::Http(h) => h.url.host_str().map(unbracket),
            CheckSpec::Tcp(t) => Some(unbracket(&t.host)),
            CheckSpec::Ping(p) => Some(unbracket(&p.host)),
            CheckSpec::TlsCert(c) => Some(unbracket(&c.host)),
            CheckSpec::DomainExpiry(d) => Some(d.domain.as_str()),
            CheckSpec::Dns(d) => Some(d.domain.as_str()),
            CheckSpec::Flow(f) => f.start_url.host_str().map(unbracket),
            CheckSpec::Heartbeat(_) => None,
        }
    }
}

/// Per-kind check-interval floor. Expiry state moves slowly, so certificates
/// floor at an hour and registrations at twelve. Heartbeat's interval is its
/// evaluation cadence, which can't be finer than the grace it judges, so a
/// minute floor.
pub fn min_interval_secs_for_kind(kind: &str) -> u64 {
    match kind {
        // RDAP rate-limits by source address, so this floor guards the probe IPs.
        "domain_expiry" => 43_200,
        "tls_cert" => 3_600,
        // A headless-browser run is far heavier than a single probe.
        "flow" => 300,
        "heartbeat" => EVALUATION_CADENCE_MIN_SECS,
        _ => 10,
    }
}

/// Smallest preset the picker offers and where a new monitor opens. Both sit
/// at or above [`min_interval_secs_for_kind`], which stays the hard limit so a
/// monitor already running faster keeps saving.
pub struct IntervalHints {
    pub min: u64,
    pub default: u64,
}

/// Most people never touch the cadence, so the suggestion is what they run.
pub fn interval_hints_for_kind(kind: &str) -> IntervalHints {
    match kind {
        "domain_expiry" => IntervalHints {
            min: 43_200,
            default: 86_400,
        },
        // A wrong certificate mid-cycle is already caught by the HTTPS monitor.
        "tls_cert" => IntervalHints {
            min: 21_600,
            default: 43_200,
        },
        // Under the record's TTL a caching resolver just re-reads its cache.
        "dns" => IntervalHints {
            min: 60,
            default: 300,
        },
        "flow" => IntervalHints {
            min: 300,
            default: 900,
        },
        "heartbeat" => IntervalHints {
            min: 60,
            default: 300,
        },
        other => IntervalHints {
            min: min_interval_secs_for_kind(other),
            default: 60,
        },
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ExpectedStatus {
    Exact(u16),
    Range {
        #[schema(minimum = 100, maximum = 599)]
        min: u16,
        #[schema(minimum = 100, maximum = 599)]
        max: u16,
    },
    OneOf(Vec<u16>),
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HttpCheck {
    #[schema(value_type = String, format = "uri", example = "https://example.com/healthz")]
    pub url: Url,
    pub method: HttpMethod,
    /// Request timeout in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, minimum = 100, maximum = 60000, example = 10000)]
    pub timeout: Duration,
    pub follow_redirects: bool,
    #[schema(maximum = 10)]
    pub max_redirects: u8,
    pub expected_status: ExpectedStatus,
    #[schema(nullable = true)]
    pub expected_body_contains: Option<String>,
    pub headers: HashMap<String, String>,
    #[schema(nullable = true)]
    pub body: Option<String>,
    pub verify_tls: bool,
    /// On read, returns `["***","***"]` if set. On write, send real values or omit the field.
    #[schema(value_type = Option<[String; 2]>, nullable = true)]
    pub basic_auth: Option<(String, String)>,
    /// On read, returns `"***"` if set. On write, send real value or omit the field.
    #[schema(nullable = true)]
    pub bearer_token: Option<String>,
}

impl HttpCheck {
    /// Redirect-hop ceiling: rejected above this on write, clamped to it on probe.
    pub const MAX_REDIRECTS: u8 = 10;
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TcpCheck {
    #[schema(example = "db.example.com")]
    pub host: String,
    #[serde(default, deserialize_with = "deserialize_port")]
    #[schema(minimum = 1, maximum = 65535, example = 5432)]
    pub port: u16,
    /// Connect timeout in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, minimum = 100, maximum = 60000, example = 3000)]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PingCheck {
    #[schema(example = "gateway.example.com")]
    pub host: String,
    /// Echo-reply timeout in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, minimum = 100, maximum = 60000, example = 3000)]
    pub timeout: Duration,
}

/// Inbound dead-man's-switch. Silence past `period + grace` opens an incident,
/// and so do the job's own signals: a reported failure, or a run that outlives
/// `max_runtime`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HeartbeatCheck {
    /// Expected ping cadence in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, minimum = 60000, example = 300000)]
    pub period: Duration,
    /// Extra allowance past `period` before the monitor counts as down,
    /// in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, example = 60000)]
    pub grace: Duration,
    /// Cap on one run's `/start`→finish time, in milliseconds. `None` leaves
    /// the run bounded only by `period + grace`.
    #[serde(
        default,
        with = "duration_ms_opt",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<u64>, example = 900000)]
    pub max_runtime: Option<Duration>,
}

impl HeartbeatCheck {
    /// The evaluator's two instants for an anchor. Saturating, so a period
    /// beyond chrono's range cannot wrap into the past.
    pub fn window(
        &self,
        anchor: chrono::DateTime<chrono::Utc>,
    ) -> (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
        let span = |d: Duration| chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX);
        let due = anchor + span(self.period);
        (due, due + span(self.grace))
    }

    /// How often the silence rule is judged: a tenth of `period + grace`,
    /// held between a minute and five. A coarser stored interval is lowered
    /// to this on load, since a tick that lands long after the window closes
    /// only delays the alarm, and the judgement itself costs nothing.
    pub fn evaluation_cadence(&self) -> Duration {
        let window = self.period.as_secs().saturating_add(self.grace.as_secs());
        Duration::from_secs(
            window
                .div_ceil(10)
                .clamp(EVALUATION_CADENCE_MIN_SECS, EVALUATION_CADENCE_MAX_SECS),
        )
    }
}

pub const EVALUATION_CADENCE_MIN_SECS: u64 = 60;
pub const EVALUATION_CADENCE_MAX_SECS: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TlsCertCheck {
    pub host: String,
    #[serde(default, deserialize_with = "deserialize_port")]
    #[schema(minimum = 1, maximum = 65535)]
    pub port: u16,
    /// SNI to send if different from `host` (e.g. when the cert is served
    /// against a virtual host name).
    #[serde(default)]
    #[schema(nullable = true)]
    pub server_name: Option<String>,
    pub warn_days: u32,
    pub critical_days: u32,
    /// Connect timeout in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64)]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DomainExpiryCheck {
    pub domain: String,
    pub warn_days: u32,
    pub critical_days: u32,
    /// Query timeout in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64)]
    pub timeout: Duration,
}

impl DomainExpiryCheck {
    /// Registrable domain to surface in the UI, only when it actually reduces
    /// the input (`app.example.co.uk` → `example.co.uk`); an apex yields `None`.
    pub fn reduced_domain_hint(&self) -> Option<String> {
        reduced_domain_hint(&self.domain)
    }
}

/// Registrable domain (public suffix + one label) for the RDAP query. An
/// unrecognised suffix falls through normalised so the registry returns a
/// precise error instead of a silent wrong lookup.
pub fn registered_domain(domain: &str) -> String {
    resolve_registrable(&normalize_domain(domain))
}

/// Registrable domain only when it differs from the normalised input — the
/// signal that a real subdomain was reduced, for UI hints (mixed-case or a
/// trailing dot alone is not a reduction).
pub fn reduced_domain_hint(domain: &str) -> Option<String> {
    let normalized = normalize_domain(domain);
    let registered = resolve_registrable(&normalized);
    (registered != normalized).then_some(registered)
}

fn normalize_domain(domain: &str) -> String {
    domain.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn resolve_registrable(normalized: &str) -> String {
    psl::domain_str(normalized)
        .map(str::to_owned)
        .unwrap_or_else(|| normalized.to_owned())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum DnsRecordType {
    A,
    Aaaa,
    Cname,
    Mx,
    Ns,
    Txt,
    Soa,
    Ptr,
    Caa,
    Srv,
}

impl DnsRecordType {
    pub fn as_str(self) -> &'static str {
        match self {
            DnsRecordType::A => "A",
            DnsRecordType::Aaaa => "AAAA",
            DnsRecordType::Cname => "CNAME",
            DnsRecordType::Mx => "MX",
            DnsRecordType::Ns => "NS",
            DnsRecordType::Txt => "TXT",
            DnsRecordType::Soa => "SOA",
            DnsRecordType::Ptr => "PTR",
            DnsRecordType::Caa => "CAA",
            DnsRecordType::Srv => "SRV",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DnsCheck {
    /// Name to resolve (FQDN; trailing dot tolerated).
    #[schema(example = "api.example.com")]
    pub domain: String,
    pub record_type: DnsRecordType,
    /// Optional custom resolver as `ip` or `ip:port` (e.g. `1.1.1.1`,
    /// `8.8.8.8:53`). `None` uses the process default resolver.
    #[serde(default)]
    #[schema(nullable = true, example = "1.1.1.1")]
    pub resolver: Option<String>,
    /// Optional substring that must appear in at least one answer value.
    /// Empty answers, NXDOMAIN, or a missing substring all fail the check.
    #[serde(default)]
    #[schema(nullable = true, example = "192.0.2.1")]
    pub expected_contains: Option<String>,
    /// Query timeout in milliseconds.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, minimum = 100, maximum = 60000, example = 3000)]
    pub timeout: Duration,
}

/// A browser-driven login/transaction flow: a step sequence replayed against a
/// real page through a headless engine. It carries cookies across steps and runs
/// page JavaScript, so it verifies a login *session* (form → submit →
/// authenticated page), not just that a login endpoint responds.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FlowCheck {
    #[schema(value_type = String, format = "uri", example = "https://app.example.com/login")]
    pub start_url: Url,
    pub steps: Vec<FlowStep>,
    /// Whole-flow budget.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, minimum = 1000, maximum = 120000, example = 30000)]
    pub timeout: Duration,
    /// Per-step wait for a selector to appear.
    #[serde(with = "duration_ms")]
    #[schema(value_type = u64, minimum = 100, maximum = 60000, example = 5000)]
    pub step_timeout: Duration,
    pub verify_tls: bool,
}

impl FlowCheck {
    /// Ceiling the engine is sized for, above any plan. A plan may allow fewer
    /// steps but never more: the whole-run budget is what a longer journey
    /// actually runs out of, and no plan column can buy more of it.
    pub const MAX_STEPS: usize = 30;

    /// Steps this plan allows, never past the engine ceiling.
    pub fn allowed_steps(plan_max: i32) -> usize {
        (plan_max.max(1) as usize).min(Self::MAX_STEPS)
    }
}

/// One action in a [`FlowCheck`]. `Fill.value` may carry a `{{secret}}` token
/// resolved at probe time; every other field is literal.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FlowStep {
    Goto {
        #[schema(value_type = String, format = "uri")]
        url: Url,
    },
    Click {
        selector: String,
    },
    Fill {
        selector: String,
        value: String,
    },
    WaitFor {
        selector: String,
    },
    /// Assert that a substring is present, optionally scoped to a selector's
    /// text rather than the whole page.
    AssertText {
        #[schema(nullable = true)]
        selector: Option<String>,
        contains: String,
    },
    AssertUrl {
        contains: String,
    },
}

// Null or absent port becomes 0 so port validation flags it, not serde.
fn deserialize_port<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u16, D::Error> {
    Ok(Option::<u16>::deserialize(d)?.unwrap_or(0))
}

mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}

mod duration_ms_opt {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_u64(d.as_millis() as u64),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_millis))
    }
}

/// Strip the surrounding `[ ]` of a bracketed IPv6 literal host. The SSRF IP
/// check and the abuse domain check MUST normalise a host identically — a
/// single shared definition keeps them from drifting apart.
pub fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_suggests_a_cadence_it_would_accept() {
        for kind in CheckSpec::ALL_KINDS {
            let hints = interval_hints_for_kind(kind);
            let floor = min_interval_secs_for_kind(kind);
            assert!(hints.min >= floor, "{kind}: picker offers below the floor");
            assert!(
                hints.default >= hints.min,
                "{kind}: default is not offered by the picker"
            );
        }
    }

    // Adding a CheckSpec variant breaks this exhaustive match, forcing ALL_KINDS
    // (and the count asserted below) to be updated in the same change.
    #[allow(dead_code)]
    fn variant_guard(spec: &CheckSpec) {
        match spec {
            CheckSpec::Http(_)
            | CheckSpec::Tcp(_)
            | CheckSpec::Ping(_)
            | CheckSpec::Heartbeat(_)
            | CheckSpec::Dns(_)
            | CheckSpec::TlsCert(_)
            | CheckSpec::DomainExpiry(_)
            | CheckSpec::Flow(_) => {}
        }
    }

    #[test]
    fn all_kinds_unique_and_complete() {
        let mut seen = std::collections::HashSet::new();
        for k in CheckSpec::ALL_KINDS {
            assert!(seen.insert(k), "duplicate kind in ALL_KINDS: {k}");
        }
        assert_eq!(CheckSpec::ALL_KINDS.len(), 8);
    }

    fn http(url: &str) -> CheckSpec {
        CheckSpec::Http(HttpCheck {
            url: url.parse().unwrap(),
            method: HttpMethod::Get,
            timeout: Duration::from_secs(5),
            follow_redirects: false,
            max_redirects: 0,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: HashMap::new(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        })
    }

    #[test]
    fn primary_host_reads_every_outbound_kind() {
        assert_eq!(
            http("https://app.example.com/healthz").primary_host(),
            Some("app.example.com")
        );
        let tcp: CheckSpec = serde_json::from_str(
            r#"{"type":"tcp","host":"db.example.com","port":5432,"timeout":3000}"#,
        )
        .unwrap();
        assert_eq!(tcp.primary_host(), Some("db.example.com"));
        let dns: CheckSpec = serde_json::from_str(
            r#"{"type":"dns","domain":"api.example.com","record_type":"A","timeout":3000}"#,
        )
        .unwrap();
        assert_eq!(dns.primary_host(), Some("api.example.com"));
        let domain: CheckSpec = serde_json::from_str(
            r#"{"type":"domain_expiry","domain":"example.com","warn_days":30,"critical_days":7,"timeout":5000}"#,
        )
        .unwrap();
        assert_eq!(domain.primary_host(), Some("example.com"));
        let tls: CheckSpec = serde_json::from_str(
            r#"{"type":"tls_cert","host":"example.com","port":443,"warn_days":30,"critical_days":7,"timeout":5000}"#,
        )
        .unwrap();
        assert_eq!(tls.primary_host(), Some("example.com"));
    }

    #[test]
    fn primary_host_strips_ipv6_brackets() {
        let tcp: CheckSpec =
            serde_json::from_str(r#"{"type":"tcp","host":"[::1]","port":5432,"timeout":3000}"#)
                .unwrap();
        assert_eq!(tcp.primary_host(), Some("::1"));
        assert_eq!(
            http("https://[2606:4700::1111]/").primary_host(),
            Some("2606:4700::1111")
        );
    }

    #[test]
    fn primary_host_none_for_heartbeat() {
        let hb: CheckSpec =
            serde_json::from_str(r#"{"type":"heartbeat","period":300000,"grace":60000}"#).unwrap();
        assert_eq!(hb.primary_host(), None);
    }

    #[test]
    fn registered_domain_reduces_subdomains() {
        assert_eq!(registered_domain("app.example.dev"), "example.dev");
        assert_eq!(registered_domain("example.dev"), "example.dev");
        assert_eq!(registered_domain("fra.my-app.com"), "my-app.com");
    }

    #[test]
    fn registered_domain_keeps_multi_level_suffixes_intact() {
        assert_eq!(registered_domain("shop.com.ua"), "shop.com.ua");
        assert_eq!(registered_domain("www.shop.com.ua"), "shop.com.ua");
        assert_eq!(registered_domain("sub.example.co.uk"), "example.co.uk");
    }

    #[test]
    fn registered_domain_normalises_case_and_trailing_dot() {
        assert_eq!(registered_domain("APP.Uptimepage.DEV."), "uptimepage.dev");
    }

    #[test]
    fn reduced_domain_hint_none_for_apex_or_normalisation_only() {
        assert_eq!(reduced_domain_hint("example.com"), None);
        assert_eq!(reduced_domain_hint("Example.com."), None);
        assert_eq!(reduced_domain_hint("  example.co.uk  "), None);
    }

    #[test]
    fn reduced_domain_hint_some_for_real_subdomain() {
        assert_eq!(
            reduced_domain_hint("app.example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            reduced_domain_hint("www.shop.com.ua").as_deref(),
            Some("shop.com.ua")
        );
    }

    #[test]
    fn tcp_port_null_or_missing_deserialises_to_zero() {
        let null_port: CheckSpec = serde_json::from_str(
            r#"{"type":"tcp","host":"db.example.com","port":null,"timeout":3000}"#,
        )
        .unwrap();
        let missing_port: CheckSpec =
            serde_json::from_str(r#"{"type":"tcp","host":"db.example.com","timeout":3000}"#)
                .unwrap();
        let real_port: CheckSpec = serde_json::from_str(
            r#"{"type":"tcp","host":"db.example.com","port":5432,"timeout":3000}"#,
        )
        .unwrap();
        assert!(matches!(
            null_port,
            CheckSpec::Tcp(TcpCheck { port: 0, .. })
        ));
        assert!(matches!(
            missing_port,
            CheckSpec::Tcp(TcpCheck { port: 0, .. })
        ));
        assert!(matches!(
            real_port,
            CheckSpec::Tcp(TcpCheck { port: 5432, .. })
        ));
    }
}
