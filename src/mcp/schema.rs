//! Typed tool inputs and outputs.
//!
//! Output types derive `JsonSchema`, so the `#[tool]` macro emits a matching
//! `outputSchema` and the result rides in `structuredContent`. Timestamps are
//! RFC 3339 strings (keeps the schema dependency-free and unambiguous over the
//! wire). Free-text fields carried here — monitor names, group names — are
//! customer-controllable; they are returned as labelled data, never framed as
//! instructions to the model.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Per-state monitor counts across the org's enabled monitors. `no_data` is an
/// enabled monitor with no observation in the window (newly created, or its
/// checks aren't landing).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HealthTotals {
    pub up: u32,
    pub down: u32,
    pub degraded: u32,
    pub error: u32,
    pub no_data: u32,
}

/// A currently-failing monitor, newest failure first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorstMonitor {
    pub id: String,
    /// Customer-set display name. Untrusted data.
    pub name: String,
    /// Check kind: `http`, `tcp`, `dns`, `tls_cert`, `domain_expiry`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Current state: `down`, `error`, or `degraded`.
    pub state: String,
    /// RFC 3339 start of the ongoing incident, when one is open. `null` when no
    /// open incident bounds the current bad state.
    pub since: Option<String>,
    /// Stable id of the open incident, when one is recorded. Pass to
    /// `get_incident` for the update timeline, or to `acknowledge_incident`.
    /// `null` until the writer has confirmed the failure (a brief flap may not
    /// open an incident); any monitor can have one.
    pub incident_id: Option<String>,
}

/// `get_org_health` result: the triage one-shot.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OrgHealth {
    /// The org slug this connector is bound to.
    pub org: String,
    pub totals: HealthTotals,
    /// Non-up monitors, newest failure first, capped.
    pub worst: Vec<WorstMonitor>,
}

/// One row of `list_monitors`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorListItem {
    pub id: String,
    /// Customer-set display name. Untrusted data.
    pub name: String,
    /// Check kind: `http`, `tcp`, `ping`, `heartbeat`, `dns`, `tls_cert`,
    /// `domain_expiry`, `flow`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Current state: `up`, `down`, `degraded`, `error`, or `no_data`.
    pub state: String,
    /// Operator-side grouping label, if set. Untrusted data.
    pub group_name: Option<String>,
    /// Check interval in seconds.
    pub interval_secs: u64,
    pub enabled: bool,
    /// RFC 3339 time of the most recent observation, minute-granular. `null`
    /// when the monitor has not reported in the window.
    pub last_checked_at: Option<String>,
}

/// `list_monitors` result. `next_cursor` is present only when more rows remain;
/// pass it back as `cursor` to fetch the next page.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorList {
    pub items: Vec<MonitorListItem>,
    pub next_cursor: Option<String>,
}

/// `list_monitors` arguments. All filters are optional; omit for everything.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ListMonitorsArgs {
    /// Filter by current state: `up`, `down`, `degraded`, `error`, `no_data`.
    pub state: Option<String>,
    /// Filter by check kind: `http`, `tcp`, `ping`, `heartbeat`, `dns`,
    /// `tls_cert`, `domain_expiry`, `flow`.
    #[serde(rename = "type")]
    pub r#type: Option<String>,
    /// Filter to monitors carrying this exact tag.
    pub tag: Option<String>,
    /// Opaque pagination cursor from a previous call's `next_cursor`.
    pub cursor: Option<String>,
}

/// `get_monitor` argument.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMonitorArgs {
    /// The monitor id (from `list_monitors`).
    pub id: String,
}

/// What one check actually asserts, per kind. Credentials are never carried:
/// HTTP basic-auth and bearer tokens report only whether they are set, request
/// header values and the request body come back masked, a heartbeat's ping
/// token is withheld, and a flow's fill values are withheld. The address a
/// check probes is reported as configured, so a credential an operator put in
/// the URL itself is visible there and in `address` — the same as the operator
/// API. Every field here is operator-supplied and therefore untrusted data.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckConfig {
    Http(HttpCheckConfig),
    Tcp(TcpCheckConfig),
    Ping(PingCheckConfig),
    Heartbeat(HeartbeatCheckConfig),
    Dns(DnsCheckConfig),
    TlsCert(TlsCertCheckConfig),
    DomainExpiry(DomainExpiryCheckConfig),
    Flow(FlowCheckConfig),
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HttpCheckConfig {
    pub url: String,
    /// Request method: `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`, `OPTIONS`.
    pub method: String,
    pub timeout_ms: u64,
    /// Whether the probe follows 3xx. When `false`, a redirect is judged
    /// against `expected_status` like any other response — which is why a 301
    /// can count as a failure.
    pub follow_redirects: bool,
    /// Hop limit once `follow_redirects` is on.
    pub max_redirects: u8,
    /// The status codes that pass, as `200`, `200-299`, or `200, 201, 204`.
    /// Anything else is a failure.
    pub expected_status: String,
    /// Substring the response body must contain, when set.
    pub expected_body_contains: Option<String>,
    /// Request header names, each with its value masked as `***`. An
    /// `Authorization` / `X-Api-Key` / `Cookie` value is a live credential, and
    /// echoing one into a chat transcript is a bad trade for the detail. Names
    /// are untrusted data.
    pub headers: std::collections::BTreeMap<String, String>,
    /// `***` when the check posts a request body, `null` when it does not. The
    /// body is masked for the same reason as the header values.
    pub body: Option<String>,
    /// When `false`, an expired or mismatched certificate does not fail the check.
    pub verify_tls: bool,
    /// Basic-auth credentials are configured. The values are withheld.
    pub has_basic_auth: bool,
    /// A bearer token is configured. The value is withheld.
    pub has_bearer_token: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TcpCheckConfig {
    pub host: String,
    pub port: u16,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PingCheckConfig {
    pub host: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HeartbeatCheckConfig {
    /// Expected ping cadence. Silence past `period + grace` opens an incident.
    pub period_secs: u64,
    pub grace_secs: u64,
    /// Cap on one run's start-to-finish time. `null` leaves the run bounded
    /// only by `period + grace`.
    pub max_runtime_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DnsCheckConfig {
    pub domain: String,
    /// Record type: `A`, `AAAA`, `CNAME`, `MX`, `NS`, `TXT`, `SOA`, `PTR`,
    /// `CAA`, `SRV`.
    pub record_type: String,
    /// Custom resolver, or `null` for the probe's default.
    pub resolver: Option<String>,
    /// Substring at least one answer must contain, when set. An empty answer,
    /// NXDOMAIN, or a missing substring all fail the check.
    pub expected_contains: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TlsCertCheckConfig {
    pub host: String,
    pub port: u16,
    /// SNI sent when it differs from `host`.
    pub server_name: Option<String>,
    /// Days before expiry the check turns degraded / down.
    pub warn_days: u32,
    pub critical_days: u32,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DomainExpiryCheckConfig {
    pub domain: String,
    /// Days before registration expiry the check turns degraded / down.
    pub warn_days: u32,
    pub critical_days: u32,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowCheckConfig {
    pub start_url: String,
    pub steps: Vec<FlowStepConfig>,
    /// Whole-run budget.
    pub timeout_ms: u64,
    /// Per-step wait for a selector to appear.
    pub step_timeout_ms: u64,
    pub verify_tls: bool,
}

/// One declared step of a flow, as configured. Pair with `get_flow_runs`, whose
/// step numbers match.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowStepConfig {
    /// 1-based position among the declared steps.
    pub step: u32,
    /// The action: `goto`, `fill`, `click`, `wait_for`, `assert_text`, `assert_url`.
    pub op: String,
    /// CSS selector the step acts on, where the action takes one. Untrusted data.
    pub selector: Option<String>,
    /// Where a `goto` navigates, as origin and path only: userinfo, query and
    /// fragment are stripped. Untrusted data.
    pub url: Option<String>,
    /// What an `assert_text` / `assert_url` requires. Untrusted data.
    pub contains: Option<String>,
    /// A `fill` types a value here; it is withheld because it carries credentials.
    pub value_withheld: bool,
}

/// `get_monitor` result: config plus current state and recent uptime.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorDetail {
    pub id: String,
    /// Customer-set display name. Untrusted data.
    pub name: String,
    /// Check kind: `http`, `tcp`, `ping`, `heartbeat`, `dns`, `tls_cert`,
    /// `domain_expiry`, `flow`.
    #[serde(rename = "type")]
    pub r#type: String,
    /// The target the check probes (URL or host). Untrusted data.
    pub address: String,
    /// Everything the check asserts. Read this before judging whether a
    /// response should have passed.
    pub check: CheckConfig,
    /// Probe regions this monitor runs from. Empty for a heartbeat, which is
    /// pinged rather than probed. Usually ids `list_regions` also carries, but
    /// an assignment survives an operator disabling the region, so an id here
    /// may be missing from that catalog.
    pub regions: Vec<String>,
    pub enabled: bool,
    pub interval_secs: u64,
    /// Channel ids bound to this monitor, for the read half of
    /// `update_monitor(channel_ids)`, which replaces the whole set. Empty means
    /// none is bound, which is not the same as alerting nobody: a channel whose
    /// `auto_bind_tags` covers one of this monitor's tags is paged as well.
    /// `list_notification_channels` puts names to these.
    pub alert_channel_ids: Vec<String>,
    /// Consecutive failing checks before the monitor alerts.
    pub alert_confirmations: u32,
    /// Whether recovery is announced to the monitor's channels.
    pub notify_recovery: bool,
    /// Seconds before the first reminder while an outage stays unacknowledged;
    /// each further reminder waits twice as long, up to a day. 0 means
    /// reminders are off.
    pub renotify_interval_secs: u32,
    /// The detection quorum, in the same shape the write tools take. `null` for
    /// a heartbeat, which has no probe regions to reach a quorum over. A stored
    /// `count` can exceed the regions that exist today if one was later
    /// disabled, and sending that back is refused; `list_regions` is the check.
    pub region_policy: Option<RegionPolicyArg>,
    /// Terraform declares this monitor, so `update_monitor`, `pause_monitor`
    /// and `resume_monitor` all refuse it. Change it in the `.tf` instead.
    pub managed_externally: bool,
    pub group_name: Option<String>,
    /// Operator tags. Untrusted data.
    pub tags: Vec<String>,
    /// Current state: `up`, `down`, `degraded`, `error`, or `no_data`.
    pub state: String,
    /// RFC 3339 time of the most recent observation in the last 24 hours.
    /// `null` when nothing landed in that window, which is not the same as
    /// never checked: a monitor paused yesterday, or a heartbeat on a longer
    /// period, reads `null` here and `no_data` in `state`.
    pub last_checked_at: Option<String>,
    /// Most recent error text, when the last check failed. Untrusted data.
    pub last_error: Option<String>,
    /// Structured edge-access diagnosis for the last failed HTTP check.
    pub last_diagnostic: Option<CheckDiagnosticView>,
    /// HTTP status code of the last check, for `http` monitors. `null` for
    /// non-HTTP checks or when the last probe never got a response.
    pub last_http_status: Option<u16>,
    /// Per-phase timing of the last check — pinpoints where latency is (DNS vs
    /// connect vs TLS vs server). Fields `null` when not applicable.
    pub last_timing: CheckTiming,
    /// Response body size of the last check in bytes, when measured.
    pub last_response_size: Option<u32>,
    /// Uptime percentage over the trailing 24 hours / 30 days. `null` when the
    /// window holds no checks — that is unknown, not zero.
    pub uptime_24h: Option<f64>,
    pub uptime_30d: Option<f64>,
}

/// `get_monitor_history` arguments.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetMonitorHistoryArgs {
    /// The monitor id (from `list_monitors`).
    pub id: String,
    /// Time window: `1h`, `24h`, `7d`, or `30d`.
    pub window: String,
    /// Narrow uptime, the latency series, and the region breakdown to one probe
    /// region (an id the monitor is assigned to, from `get_monitor.regions`).
    /// Omit for every region together.
    pub region: Option<String>,
}

/// Per-phase timing of a single check, in milliseconds. Fields are `null` for
/// phases that don't apply (e.g. `tls_ms` on plain HTTP, all phases on a DNS or
/// TCP check) or when the probe failed before reaching them.
#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct CheckTiming {
    /// DNS resolution time.
    pub dns_ms: Option<u16>,
    /// TCP connect time.
    pub connect_ms: Option<u16>,
    /// TLS handshake time.
    pub tls_ms: Option<u16>,
    /// Time to first byte.
    pub ttfb_ms: Option<u16>,
}

/// Bounded, machine-readable explanation for a failed HTTP observation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckDiagnosticView {
    pub kind: String,
    pub confidence: String,
    pub provider: Option<String>,
    pub evidence: Vec<String>,
    pub remediations: Vec<String>,
    pub summary: String,
    pub guidance: String,
}

/// One latency sample bucket.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LatencyPoint {
    /// RFC 3339 bucket start.
    pub at: String,
    /// Median / 95th / 99th-percentile duration over the bucket, in ms.
    pub p50_ms: u32,
    pub p95_ms: u32,
    pub p99_ms: u32,
}

/// One failing observation window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Failure {
    /// RFC 3339 start of the failure.
    pub at: String,
    /// `down` or `error`.
    pub state: String,
    /// Sampled error text. Untrusted data.
    pub error: Option<String>,
}

/// One incident window (a contiguous failing run).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentWindow {
    /// RFC 3339 incident start.
    pub opened_at: String,
    /// RFC 3339 incident end, or `null` while ongoing.
    pub resolved_at: Option<String>,
    /// `false` when this window is listed but explains none of the `uptime` gap.
    pub counts_as_downtime: bool,
}

/// One region's share of a monitor's window, straight from its own checks.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RegionHealth {
    /// Region id (see `list_regions`).
    pub region: String,
    /// Checks this region ran in the window.
    pub samples: u64,
    /// How many of them passed.
    pub up: u64,
    /// `up` over `samples`. Raw per-check rate, so it can read lower than the
    /// monitor's headline `uptime`, which counts only confirmed incidents.
    pub uptime_pct: Option<f64>,
    /// Median / 95th / 99th-percentile duration over the window, in ms.
    pub p50_ms: u32,
    pub p95_ms: u32,
    pub p99_ms: u32,
    /// This region's last observation: `up`, `down`, `degraded`, `error`, or
    /// `no_data`.
    pub last_status: String,
}

/// `get_monitor_history` result, bounded to the requested window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorHistory {
    /// Uptime percentage over the window. `null` when the window holds no
    /// checks — that is unknown, not zero. Unfiltered it counts confirmed
    /// incidents; under a `region` filter it is that region's raw check rate,
    /// so the two are not comparable.
    pub uptime: Option<f64>,
    /// The region this answer was narrowed to, or `null` for all of them.
    pub region: Option<String>,
    pub latency_series: Vec<LatencyPoint>,
    /// Per-region split of the same window, so a partial outage is visible.
    /// Always every region the monitor runs in, including under a `region`
    /// filter, and empty when it runs in only one. Regions that ran no checks
    /// in the window are omitted; this reads per-minute data, which is kept for
    /// 30 days, so at the far edge of a `30d` window a region can be short of
    /// samples or absent while the headline numbers still cover it.
    pub regions: Vec<RegionHealth>,
    /// Confirmed failures on the monitor as a whole. A `region` filter does not
    /// narrow these: an incident is raised for the monitor, not per region.
    pub failures: Vec<Failure>,
    /// Incident windows on the monitor as a whole, unnarrowed by `region` for
    /// the same reason as `failures`.
    pub incidents: Vec<IncidentWindow>,
}

/// One probe region in the fleet catalog.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RegionItem {
    /// Stable id — what `get_monitor.regions` lists and `get_monitor_history`
    /// takes as `region`.
    pub id: String,
    /// Display name.
    pub name: String,
    pub city: String,
    /// ISO 3166-1 alpha-2 country code, when set.
    pub country_code: Option<String>,
    /// Continent slug, when set.
    pub continent: Option<String>,
    /// Whether a new monitor probes from here unless told otherwise. Omitting
    /// `create_monitor.regions` takes exactly the regions flagged here.
    pub default_selected: bool,
}

/// `list_regions` result: every enabled probe region.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RegionList {
    pub items: Vec<RegionItem>,
    /// How many of these regions one monitor may probe from, when the plan
    /// allows fewer than the catalog holds. A `create_monitor` naming more is
    /// refused outright, not trimmed to fit. Null when the plan reaches every
    /// region listed, which is not licence to name them all: what a monitor
    /// takes by default is `default_selected`, not this ceiling.
    pub max_regions: Option<u32>,
}

/// One tag and how many monitors carry it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagItem {
    /// Operator-set tag. Untrusted data.
    pub name: String,
    pub count: u64,
}

/// `list_tags` result: the org's tag inventory, most-used first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TagList {
    pub items: Vec<TagItem>,
    /// The org has more tags than the cap returned here, so a tag missing from
    /// `items` is not proof it does not exist.
    pub truncated: bool,
}

/// What a new monitor should check. Narrower than what the product supports on
/// purpose: no request headers, body or credentials, since those are literal
/// secrets an operator types once and this tool would carry through a chat log;
/// and no browser flow, which needs the fill values withheld everywhere else.
/// Add those in the Uptimepage app after the monitor exists.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NewCheck {
    /// An HTTP(S) endpoint.
    Http {
        url: String,
        /// `get` (default), `head`, `post`, `put`, `patch`, `delete`, `options`.
        method: Option<String>,
        /// A single code like `200`, a range like `200-299`, or a list like
        /// `200,201,204`. Defaults to `200-299`.
        expected_status: Option<String>,
        /// Text the response body must contain for the check to pass.
        expected_body_contains: Option<String>,
        /// Defaults to 10000. A redirect counts as a pass only with
        /// `follow_redirects`.
        timeout_ms: Option<u64>,
        /// Defaults to true, following at most 5 hops.
        follow_redirects: Option<bool>,
        /// Defaults to true. False accepts an invalid certificate, which a
        /// `tls_cert` monitor is the better way to watch.
        verify_tls: Option<bool>,
        /// Request headers. A header that carries a credential
        /// (`authorization`, `x-api-key`, `api-key`, `cookie`,
        /// `proxy-authorization`) must reference an org variable rather than
        /// spell the secret out: `Bearer {{ my_key }}`. Use `list_variables`
        /// to find the key, and never paste the credential itself — this tool
        /// refuses it, and a pasted one would live on in the chat log.
        headers: Option<std::collections::HashMap<String, String>>,
        /// Request body, sent as given. `{{ key }}` references are resolved at
        /// probe time, so a body may carry a secret by reference.
        body: Option<String>,
    },
    /// A TCP port accepting connections.
    Tcp {
        host: String,
        port: u16,
        timeout_ms: Option<u64>,
    },
    /// ICMP reachability.
    Ping {
        host: String,
        timeout_ms: Option<u64>,
    },
    /// A DNS record resolving, optionally to an expected value.
    Dns {
        domain: String,
        /// `a` (default), `aaaa`, `cname`, `mx`, `ns`, `txt`, `soa`, `ptr`,
        /// `caa`, `srv`.
        record_type: Option<String>,
        /// Resolver to ask. Defaults to the agent's own.
        resolver: Option<String>,
        /// Text the answer must contain.
        expected_contains: Option<String>,
        timeout_ms: Option<u64>,
    },
    /// A TLS certificate's remaining validity.
    TlsCert {
        host: String,
        /// Defaults to 443.
        port: Option<u16>,
        /// Days of remaining validity that degrade the check. Default 30.
        warn_days: Option<u32>,
        /// Days of remaining validity that fail it. Default 7.
        critical_days: Option<u32>,
        timeout_ms: Option<u64>,
    },
    /// A domain registration's remaining validity.
    DomainExpiry {
        domain: String,
        /// Default 30.
        warn_days: Option<u32>,
        /// Default 7.
        critical_days: Option<u32>,
        timeout_ms: Option<u64>,
    },
    /// An inbound ping from a job that is expected to report in. Nothing is
    /// probed; the monitor fails when the ping does not arrive. The ping URL
    /// and its token are shown in the app, never here.
    Heartbeat {
        /// How often the job is expected to report.
        period_secs: u64,
        /// Lateness tolerated before the monitor fails.
        grace_secs: u64,
        /// Fail a run that starts and does not finish within this.
        max_runtime_secs: Option<u64>,
    },
}

/// `create_monitor` arguments.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateMonitorArgs {
    /// What the operator will see in lists and alerts.
    pub name: String,
    pub check: NewCheck,
    /// Seconds between checks, held to the plan's floor and the check kind's
    /// own floor. Omit it to get the cadence the app's own picker opens this
    /// kind at, which is well above the hard minimum for the slow-moving kinds:
    /// a certificate is checked twice a day, a domain registration daily.
    pub interval_secs: Option<u64>,
    /// At most 50, each at most 50 characters.
    pub tags: Option<Vec<String>>,
    /// Operator-side grouping label.
    pub group_name: Option<String>,
    /// Consecutive failing checks before the monitor alerts. Minimum 1,
    /// defaults to 2.
    pub alert_confirmations: Option<u32>,
    /// Whether recovery is announced. Defaults to true.
    pub notify_recovery: Option<bool>,
    /// Seconds before the first reminder while an outage stays unacknowledged;
    /// each further reminder waits twice as long, up to a day. 0 turns
    /// reminders off; otherwise at least 60. Defaults to 3600.
    pub renotify_interval_secs: Option<u32>,
    /// Detection quorum across probe regions.
    pub region_policy: Option<RegionPolicyArg>,
    /// Probe regions to run the check from, as ids from `list_regions`. Omit
    /// unless the user named the places they want covered: omitting takes the
    /// regions `list_regions` flags `default_selected`, which is the coverage
    /// the operator chose, capped at the plan's region cap and falling back to
    /// the control plane's own region when nothing is flagged. A vantage point
    /// can be offered without being on by default, so the full catalog is not
    /// the thorough answer. Rejected for a heartbeat, which is pinged rather
    /// than probed, and a set larger than a `max_regions` `list_regions`
    /// reports is refused outright, not trimmed to fit.
    pub regions: Option<Vec<String>>,
    /// Channel ids from `list_notification_channels` to alert. Omitting them
    /// creates a monitor that pages nobody, which is worth saying out loud
    /// rather than leaving for an outage to reveal. The channels themselves are
    /// set up in the app, since they hold the tokens and addresses.
    pub channel_ids: Option<Vec<String>>,
}

/// The trial run a monitor was created from. Carries no id: it happened before
/// the monitor existed and is not stored as one of its results.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ProbeOutcome {
    /// Observed state: `up`, `down`, `degraded`, `error`.
    pub state: String,
    pub duration_ms: u32,
    /// HTTP status code, for an `http` check.
    pub http_status: Option<u16>,
    /// Error text when the probe failed. Untrusted data.
    pub error: Option<String>,
    /// Structured edge-access diagnosis, when a supported signature matched.
    pub diagnostic: Option<CheckDiagnosticView>,
}

/// `create_monitor` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorCreated {
    pub id: String,
    pub name: String,
    /// What the check watches, as stored.
    pub address: String,
    pub interval_secs: u64,
    /// Probe regions the monitor was assigned, which is the operator's default
    /// set when `regions` was omitted. Empty for a heartbeat.
    pub regions: Vec<String>,
    /// The trial run's outcome, which the operator saw before approving. Absent
    /// for a heartbeat, which has nothing to probe.
    pub probe: Option<ProbeOutcome>,
    /// The channels this monitor will alert, by name, or `nobody` when nothing
    /// reaches it. One covered by a channel's tag rule rather than a binding is
    /// marked `by tag`. A channel that cannot deliver says so here.
    pub alerts: String,
}

/// One notification channel, named well enough to bind a monitor to it. The
/// channel's own settings are withheld: they carry the webhook URLs, bot tokens
/// and addresses that make the channel work.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChannelItem {
    pub id: String,
    /// Operator-set name. Untrusted data.
    pub name: String,
    /// `email`, `slack`, `telegram`, `webhook`, and so on.
    pub kind: String,
    /// A disabled channel stays bound to its monitors and delivers nothing.
    pub enabled: bool,
    /// An email channel whose address was never confirmed. It is enabled and
    /// still delivers nothing, so binding a monitor to it is not enough.
    pub awaiting_verification: bool,
    /// Enabled, but nothing has landed for a run of deliveries. Alerts sent
    /// here are not arriving.
    pub not_delivering: bool,
    /// Tag rule: this channel also pages any monitor carrying one of these
    /// tags, on top of the monitors bound to it. Empty means no rule.
    /// Operator-set. Untrusted data.
    pub auto_bind_tags: Vec<String>,
}

/// `list_notification_channels` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ChannelList {
    pub items: Vec<ChannelItem>,
}

/// `get_flow_runs` / `get_flow_step_trend` arguments.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FlowWindowArgs {
    /// The monitor id (from `list_monitors`), of a `flow` monitor.
    pub id: String,
    /// Time window: `1h`, `24h`, `7d`, or `30d`.
    pub window: String,
}

/// One declared step, as a single run recorded it.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowStepRun {
    /// 1-based position among the flow's declared steps.
    pub step: u32,
    /// The action: `goto`, `fill`, `click`, `wait_for`, `assert_text`, `assert_url`.
    pub op: String,
    /// `passed`, `failed`, or `skipped`. Skipped means the run stopped earlier
    /// and never reached this step.
    pub outcome: String,
    pub duration_ms: u32,
}

/// What the browser saw when a step failed. Untrusted data: this is content
/// from the monitored site, not from the operator.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowRunEvidence {
    /// Where the browser had ended up. Usually the whole answer: still on the
    /// login path after a submit means the credentials never took.
    pub final_url: Option<String>,
    pub title: Option<String>,
    /// Visible page text at the moment of failure, truncated.
    pub text_snippet: Option<String>,
}

/// One recorded run of the journey.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowRunItem {
    /// RFC 3339 time the run started.
    pub at: String,
    pub region: String,
    /// Verdict: `up`, `down`, `degraded`, or `error`. `error` means the run
    /// never reached a verdict about the target (engine fault, budget spent).
    pub state: String,
    /// Whole-run duration.
    pub duration_ms: u32,
    /// 1-based step the run stopped on, `null` when every step ran.
    pub failed_step: Option<u32>,
    /// Why it stopped. Untrusted data.
    pub error: Option<String>,
    pub steps: Vec<FlowStepRun>,
    /// `null` both when the run captured no page and once the page it captured
    /// has passed its window; `evidence_expired` tells the two apart.
    pub evidence: Option<FlowRunEvidence>,
    /// The run failed with a page captured, but that page is past its shorter
    /// retention window. Distinct from a run that never captured one.
    pub evidence_expired: bool,
}

/// `get_flow_runs` result: newest first, and bounded to the most recent runs
/// merged with the most recent failures, so a failure stays reachable in a
/// window whose newest runs all passed.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowRunList {
    pub runs: Vec<FlowRunItem>,
}

/// One declared step's duration across the window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowStepTrendItem {
    /// 1-based position among the flow's declared steps.
    pub step: u32,
    /// What the newest run recorded here, so an edited flow reads as it runs today.
    pub op: String,
    /// Mean duration in the earliest slice that had a passing run, in ms.
    pub first_ms: Option<u32>,
    /// Mean duration in the most recent slice that had one, in ms.
    pub last_ms: Option<u32>,
    /// `last_ms` over `first_ms`. 1.0 is flat, 4.0 has quadrupled. `null` when
    /// either end is missing or the first is zero.
    pub change_ratio: Option<f64>,
    /// Runs that passed this step across the window — what the means average.
    pub samples: u64,
    /// Runs that reached this step and failed it. Kept out of the means: a
    /// failed step waited out its whole timeout and says nothing about how long
    /// the step takes when it works.
    pub failed: u64,
}

/// `get_flow_step_trend` result, one entry per declared step in order.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FlowStepTrendSummary {
    pub steps: Vec<FlowStepTrendItem>,
}

/// `list_incidents` arguments. All filters are optional.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ListIncidentsArgs {
    /// Which incidents to return: `open` (default) for the ones still running,
    /// or `all` to include resolved ones inside the window.
    pub state: Option<String>,
    /// RFC 3339 start of the window. Defaults to 30 days ago. An incident that
    /// is still running is listed however long ago it opened.
    pub from: Option<String>,
    /// RFC 3339 end of the window. Defaults to now. Incidents that opened after
    /// it are excluded, running or not.
    pub to: Option<String>,
    /// Restrict to one monitor (id from `list_monitors`).
    pub monitor_id: Option<String>,
    /// Opaque pagination cursor from a previous call's `next_cursor`. It
    /// carries the whole query, so send it on its own: any other filter passed
    /// alongside it is ignored rather than silently changing the page.
    pub cursor: Option<String>,
}

/// One incident in `list_incidents`. Incident-centric; for a monitor's full
/// state use `get_monitor`/`get_org_health`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentSummary {
    /// Stable incident id. Pass to `get_incident` or `acknowledge_incident`.
    pub id: String,
    /// The affected monitor's id.
    pub monitor_id: String,
    /// The affected monitor's display name. Untrusted data.
    pub monitor_name: String,
    /// Severity: `minor`, `major`, or `critical`.
    pub severity: String,
    /// RFC 3339 incident start.
    pub opened_at: String,
    /// RFC 3339 incident end, or `null` while ongoing.
    pub resolved_at: Option<String>,
    /// Phase of the latest operator update: `investigating`, `identified`,
    /// `monitoring`, `resolved`, `postmortem`. `null` if no update was posted.
    pub latest_phase: Option<String>,
    /// RFC 3339 time of the latest operator update, when one exists.
    pub latest_update_at: Option<String>,
}

/// `list_incidents` result. `next_cursor` is present only when more rows remain.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentList {
    pub items: Vec<IncidentSummary>,
    /// RFC 3339 window actually read, after the defaults and the one-year cap.
    /// It bounds the *resolved* incidents only: one that is still running is
    /// listed however long ago it opened, so it can be older than `from`.
    /// Describe spans from these, never from what was asked for.
    pub from: String,
    pub to: String,
    pub next_cursor: Option<String>,
}

/// Argument of the incident tools that take only an id.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IncidentIdArg {
    /// The incident id (from `list_incidents` or `get_org_health`).
    pub id: String,
}

/// `get_incident_metrics` argument.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct GetIncidentMetricsArgs {
    /// Trailing window in days (1..=365). Defaults to 30 when omitted.
    pub window_days: Option<u32>,
}

/// One `{key, count}` bucket in the metrics rollup.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MetricCount {
    pub key: String,
    pub count: u64,
}

/// A monitor and how many incidents it raised in the window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NoisyMonitor {
    pub monitor_id: String,
    pub count: u64,
}

/// `get_incident_metrics` result: MTTA/MTTR and incident counts over a window.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentMetricsResult {
    pub window_days: u32,
    /// Incidents opened in the window.
    pub total: u64,
    /// Mean time to acknowledge, seconds. `null` if none were acknowledged.
    pub mtta_secs: Option<f64>,
    /// Mean time to resolve, seconds. `null` if none were resolved.
    pub mttr_secs: Option<f64>,
    pub by_severity: Vec<MetricCount>,
    pub by_state: Vec<MetricCount>,
    /// Resolved automatically on recovery, with no human resolver.
    pub auto_resolved: u64,
    /// Resolved by a person.
    pub human_resolved: u64,
    /// Noisiest monitors, most incidents first.
    pub top_monitors: Vec<NoisyMonitor>,
}

/// One operator update on an incident, oldest first.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentUpdateItem {
    /// RFC 3339 time the update was posted.
    pub posted_at: String,
    /// `investigating`, `identified`, `monitoring`, `resolved`, `postmortem`.
    pub phase: String,
    /// Update text, shown on the public status page. Untrusted data.
    pub message: String,
}

/// `get_incident` result: one incident with its full update timeline.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentDetail {
    pub id: String,
    /// The affected monitor's id.
    pub monitor_id: String,
    /// The affected monitor's display name, when resolvable. Untrusted data.
    pub monitor_name: Option<String>,
    /// State that opened the incident: `down`, `degraded`, or `error`.
    pub state: String,
    /// Severity: `minor`, `major`, or `critical`.
    pub severity: String,
    /// RFC 3339 incident start.
    pub opened_at: String,
    /// RFC 3339 incident end, or `null` while ongoing.
    pub resolved_at: Option<String>,
    /// Sampled error text. Untrusted data.
    pub error_sample: Option<String>,
    /// Regions that have confirmed the failure at any point in this incident;
    /// a region that recovers before the close stays listed. Empty for a
    /// single-region monitor. Untrusted data.
    pub regions_down: Vec<String>,
    /// Regions that have not confirmed the failure. Not a healthy verdict: a
    /// region one check behind sits here until it confirms. Untrusted data.
    pub regions_up: Vec<String>,
    /// Operator updates, oldest first.
    pub updates: Vec<IncidentUpdateItem>,
}

/// `list_status_pages` arguments.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ListStatusPagesArgs {
    /// Opaque pagination cursor from a previous call's `next_cursor`.
    pub cursor: Option<String>,
}

/// One status page in `list_status_pages`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusPageSummary {
    pub slug: String,
    /// Page display name. Untrusted data.
    pub name: String,
    /// Public URL the page is served at (absolute in subdomain mode).
    pub public_url: String,
    pub enabled: bool,
}

/// `list_status_pages` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusPageList {
    pub items: Vec<StatusPageSummary>,
    pub next_cursor: Option<String>,
}

/// `get_status_page` argument.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetStatusPageArgs {
    /// The page slug (from `list_status_pages`).
    pub slug: String,
}

/// One component (a curated monitor) on a status page.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusPageComponent {
    /// Public-facing component name. Untrusted data.
    pub public_name: String,
    /// Public grouping label, if any. Untrusted data.
    pub group: Option<String>,
    /// The linked monitor's id.
    pub linked_monitor: String,
    /// Current state of the linked monitor: `up`, `down`, `degraded`, `error`,
    /// or `no_data`.
    pub state: String,
}

/// `get_status_page` result: "what customers see".
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusPageDetail {
    pub slug: String,
    /// Page display name. Untrusted data.
    pub name: String,
    pub public_url: String,
    pub enabled: bool,
    pub components: Vec<StatusPageComponent>,
}

/// A single quota's usage against its cap.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Quota {
    pub used: i64,
    pub cap: i64,
}

// ── Write tools (Phase 4) ───────────────────────────────────────────────────

/// `run_check_now` / `pause_monitor` / `resume_monitor` argument.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MonitorIdArg {
    /// The monitor id (from `list_monitors`).
    pub id: String,
}

/// `run_check_now` result: the fresh probe's observation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CheckRunResult {
    pub id: String,
    /// Observed state: `up`, `down`, `degraded`, `error`.
    pub state: String,
    /// RFC 3339 time of the probe.
    pub checked_at: String,
    pub duration_ms: u32,
    /// HTTP status code, for `http` monitors. `null` for non-HTTP checks or
    /// when no response was received.
    pub http_status: Option<u16>,
    /// Per-phase timing of the probe (DNS / connect / TLS / first byte).
    pub timing: CheckTiming,
    /// Response body size in bytes, when measured.
    pub response_size: Option<u32>,
    /// Error text when the probe failed. Untrusted data.
    pub error: Option<String>,
    /// Structured edge-access diagnosis, when a supported signature matched.
    pub diagnostic: Option<CheckDiagnosticView>,
}

/// `pause_monitor` / `resume_monitor` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorStateResult {
    pub id: String,
    /// The monitor's enabled state after the change.
    pub enabled: bool,
}

/// How the quorum is expressed. `count` carries its number in `count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegionPolicyMode {
    /// One region down is enough.
    Any,
    /// More than half of them.
    Majority,
    /// Every region.
    All,
    /// A fixed number.
    Count,
}

/// How many regions must agree a monitor is down before an incident opens.
/// `get_monitor` reads this back in the same shape, so the enum of legal modes
/// is one definition rather than a written-out list that can drift.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionPolicyArg {
    pub mode: RegionPolicyMode,
    /// Required with `mode: "count"`, ignored otherwise. Must be between 1 and
    /// the number of regions the fleet has; `list_regions` reports them.
    pub count: Option<u32>,
}

/// `update_monitor` arguments. Every field except `id` is optional; omit what
/// should stay as it is. Name, address or URL, assertions, expected status,
/// headers, body, probe regions and owner are not editable here, and passing
/// one is an error rather than a silent no-op.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateMonitorArgs {
    /// The monitor id (from `list_monitors`).
    pub id: String,
    /// Seconds between checks. Held to the plan's floor and the check kind's
    /// own floor, whichever is higher.
    pub interval_secs: Option<u64>,
    /// Consecutive failing checks before the monitor alerts. Minimum 1. Raising
    /// it quietens a flapping monitor at the cost of alerting later.
    pub alert_confirmations: Option<u32>,
    /// Whether recovery is announced to the monitor's channels.
    pub notify_recovery: Option<bool>,
    /// Seconds before the first reminder while an outage stays unacknowledged;
    /// each further reminder waits twice as long, up to a day. 0 turns
    /// reminders off; otherwise at least 60.
    pub renotify_interval_secs: Option<u32>,
    /// Replaces the whole tag list. Read the monitor first: a tag left out of
    /// this list is removed. At most 50 tags, each at most 50 characters.
    pub tags: Option<Vec<String>>,
    /// Operator-side grouping label. Send `null` to clear it; omit to keep it.
    #[serde(default, deserialize_with = "crate::domain::target::double_option")]
    pub group_name: Option<Option<String>>,
    /// Detection quorum across probe regions.
    pub region_policy: Option<RegionPolicyArg>,
    /// Replaces the whole set of alerted channels, by id from
    /// `list_notification_channels`. Read the monitor first: a channel left out
    /// of this list stops being alerted. An empty list silences the monitor.
    pub channel_ids: Option<Vec<String>>,
}

/// One field an update actually moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct FieldChange {
    pub field: String,
    /// The value before and after, rendered for reporting back to a human.
    pub from: String,
    pub to: String,
}

/// `update_monitor` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorUpdateResult {
    pub id: String,
    /// What moved. Empty when every value sent already matched the stored one,
    /// in which case nothing was written and no confirmation was asked for.
    pub changes: Vec<FieldChange>,
}

/// `acknowledge_incident` / `resolve_incident` argument.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IncidentActionArgs {
    /// The incident id.
    pub id: String,
    /// Optional internal note recorded on the incident's activity timeline.
    /// This is operator-facing, not published to the public status page.
    pub note: Option<String>,
}

/// Result of an incident lifecycle action.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentActionResult {
    pub incident_id: String,
    /// Operational state after the action: `triggered`, `acknowledged`, `resolved`.
    pub state: String,
    /// RFC 3339 acknowledged time, when set.
    pub acknowledged_at: Option<String>,
    /// RFC 3339 resolved (ended) time, when set.
    pub resolved_at: Option<String>,
}

/// `publish_incident` argument. The optional narration seeds what customers
/// read on the status page; omitted fields keep whatever is stored.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PublishIncidentArgs {
    /// The incident id.
    pub id: String,
    /// Public headline shown on the status page.
    pub public_title: Option<String>,
    /// Public summary shown under the headline.
    pub public_description: Option<String>,
}

/// `publish_incident` / `unpublish_incident` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentVisibilityResult {
    pub incident_id: String,
    /// Visibility after the change: `public` or `internal`.
    pub visibility: String,
}

/// `post_incident_update` argument: appends a public, customer-facing entry to
/// the incident's status-page timeline.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PostIncidentUpdateArgs {
    /// The incident id.
    pub id: String,
    /// Public message shown on the status page.
    pub message: String,
    /// Optional phase: `investigating`, `identified`, `monitoring`,
    /// `resolved`, `postmortem`. Defaults to `investigating`.
    pub phase: Option<String>,
}

/// `post_incident_update` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IncidentUpdatePosted {
    pub incident_id: String,
    /// RFC 3339 time the update was posted.
    pub posted_at: String,
}

/// One org variable, as `list_variables` reports it. A value is never carried:
/// a plain variable's is withheld for brevity and a secret's is never read.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VariableSummary {
    /// The key to write as `{{ key }}` in a header or body.
    pub key: String,
    pub is_secret: bool,
}

/// `list_variables` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VariableList {
    pub items: Vec<VariableSummary>,
}

/// `create_monitors` argument.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateMonitorsArgs {
    /// The monitors to create. Each is validated and probed before the single
    /// confirmation covering the batch.
    pub monitors: Vec<CreateMonitorArgs>,
}

/// What became of one requested monitor.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorCreateOutcome {
    /// The name as requested. Untrusted data.
    pub name: String,
    /// The new monitor's id, absent when it was not created.
    pub id: Option<String>,
    pub address: Option<String>,
    pub probe: Option<ProbeOutcome>,
    /// Why it was not created, when it was not.
    pub error: Option<String>,
}

/// `create_monitors` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MonitorsCreated {
    pub created: usize,
    pub results: Vec<MonitorCreateOutcome>,
}

/// `create_status_page` argument.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateStatusPageArgs {
    /// URL slug, lowercase. This is the page's public address and cannot be
    /// guessed back later, so pick it deliberately.
    pub slug: String,
    /// Page display name, at most 80 characters.
    pub name: String,
    /// Defaults to false: a page is created dark so components can be curated
    /// before anyone can read it.
    pub enabled: Option<bool>,
}

/// `create_status_page` / `update_status_page` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusPageWritten {
    pub slug: String,
    pub name: String,
    pub public_url: String,
    pub enabled: bool,
}

/// `update_status_page` argument. Every field but `slug` is optional; an
/// omitted field is left as it is.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateStatusPageArgs {
    /// The page to change, by its current slug.
    pub slug: String,
    /// New display name.
    pub name: Option<String>,
    /// New slug. Changing it moves the public URL and breaks existing links.
    pub new_slug: Option<String>,
    /// Publish or unpublish the page.
    pub enabled: Option<bool>,
}

/// One monitor to curate onto a page.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct NewComponentArg {
    /// The monitor id (from `list_monitors`).
    pub monitor_id: String,
    /// Public-facing name, at most 80 characters. Defaults to the monitor's
    /// own name, which is operator-facing and may not read well in public.
    pub public_name: Option<String>,
    /// Public description, at most 200 characters.
    pub public_description: Option<String>,
    /// Grouping label, at most 50 characters. Components sharing a label are
    /// rendered together.
    pub public_group: Option<String>,
    /// Position on the page, ascending. Defaults to the order given.
    pub sort_order: Option<i32>,
    /// Publish a per-monitor detail view. That view renders the monitor's
    /// operator-side name and address, not `public_name`.
    pub detail_link_enabled: Option<bool>,
}

/// `add_status_page_components` argument.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AddComponentsArgs {
    /// The page slug (from `list_status_pages`).
    pub slug: String,
    /// The monitors to add, in the order they should appear.
    pub components: Vec<NewComponentArg>,
}

/// What became of one requested component.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ComponentOutcome {
    pub monitor_id: String,
    /// `added`, `already_on_page`, or `failed`.
    pub outcome: String,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

/// `add_status_page_components` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ComponentsAdded {
    pub slug: String,
    pub added: usize,
    pub results: Vec<ComponentOutcome>,
}

/// `update_status_page_component` argument. An omitted field is left as it is.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateComponentArgs {
    /// The page slug (from `list_status_pages`).
    pub slug: String,
    /// The monitor id whose curation is being changed.
    pub monitor_id: String,
    pub public_name: Option<String>,
    pub public_description: Option<String>,
    pub public_group: Option<String>,
    pub sort_order: Option<i32>,
}

/// `update_status_page_component` result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ComponentUpdated {
    pub slug: String,
    pub monitor_id: String,
}

/// `get_org_usage` result: the account's usage against its plan limits, pooled
/// across every org it owns.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OrgUsage {
    /// The org slug this connector is bound to.
    pub org: String,
    /// The org's display name.
    pub org_name: String,
    /// Plan id (e.g. `free`, `pro`).
    pub plan: String,
    pub targets: Quota,
    pub status_pages: Quota,
    pub members: Quota,
    pub public_components: Quota,
    pub maintenance_windows: Quota,
    pub notification_channels: Quota,
    /// Minimum allowed check interval, seconds.
    pub min_check_interval_secs: i64,
    /// History retention, days.
    pub retention_days: i64,
}
