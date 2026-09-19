use super::text::{present_error, sanitize_data};

use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use crate::api::redaction::{REDACTED, strip_url_credentials};
use crate::domain::IncidentVisibility;
use crate::domain::TargetAlerts;
use crate::domain::incident::Incident;
use crate::domain::metrics::DashboardMetrics;
use crate::domain::notification_channel::NotificationChannel;
use crate::domain::result::{CheckResult, CheckStatus};
use crate::domain::target::RegionIncidentPolicy;
use crate::domain::{CheckSpec, ExpectedStatus, FlowStep};
use crate::storage::incidents::IncidentBrief;

use crate::mcp::schema::{
    CheckConfig, CheckDiagnosticView, CheckTiming, DnsCheckConfig, DomainExpiryCheckConfig,
    FlowCheckConfig, FlowRunEvidence, FlowRunItem, FlowStepConfig, FlowStepRun, FlowStepTrendItem,
    HeartbeatCheckConfig, HttpCheckConfig, IncidentDetail, IncidentSummary, IncidentUpdateItem,
    IncidentVisibilityResult, PingCheckConfig, ProbeOutcome, RegionHealth, RegionItem,
    RegionPolicyArg, RegionPolicyMode, TcpCheckConfig, TlsCertCheckConfig,
};

pub(super) fn check_diagnostic(result: &CheckResult) -> Option<CheckDiagnosticView> {
    result
        .diagnostic
        .as_ref()
        .map(|diagnostic| CheckDiagnosticView {
            kind: diagnostic.kind.as_str().to_string(),
            confidence: diagnostic.confidence.as_str().to_string(),
            provider: diagnostic
                .provider
                .map(|provider| provider.as_str().to_string()),
            evidence: diagnostic
                .evidence
                .iter()
                .map(|item| item.as_str().to_string())
                .collect(),
            remediations: diagnostic
                .remediations
                .iter()
                .map(|item| item.as_str().to_string())
                .collect(),
            summary: diagnostic.summary(),
            guidance: diagnostic.guidance().to_string(),
        })
}

/// Who a page would reach, as the names a human approves: the bound channels,
/// then any whose tag rule covers `tags`, flagging the ones that deliver
/// nothing. A binding whose channel is gone is named as deleted rather than
/// dropped from the line, which would read as one fewer channel losing its
/// alerts. `None` means nothing pages.
pub(super) fn channel_names(
    alerts: &TargetAlerts,
    tags: &[String],
    channels: &[NotificationChannel],
    failure_limit: u32,
) -> Option<String> {
    let named = |c: &NotificationChannel| match undeliverable_reason(c, failure_limit) {
        Some(why) => format!("{} ({why})", sanitize_data(&c.name)),
        None => sanitize_data(&c.name),
    };
    let mut out: Vec<String> = alerts
        .iter()
        .map(|b| match channels.iter().find(|c| c.id == b.channel_id) {
            Some(c) => named(c),
            None => format!("a deleted channel ({})", b.channel_id),
        })
        .collect();
    for c in channels
        .iter()
        .filter(|c| c.auto_binds(tags) && !alerts.iter().any(|b| b.channel_id == c.id))
    {
        // One parenthetical: stacking the health note behind the routing note
        // leaves the reader to work out which is which.
        out.push(match undeliverable_reason(c, failure_limit) {
            Some(why) => format!("{} (by tag, {why})", sanitize_data(&c.name)),
            None => format!("{} (by tag)", sanitize_data(&c.name)),
        });
    }
    (!out.is_empty()).then(|| out.join(", "))
}

/// Why a channel would deliver nothing if a monitor were bound to it. One
/// definition, so a new undeliverable state cannot be added to the listing and
/// forgotten in the confirmation.
pub(super) fn undeliverable_reason(
    channel: &NotificationChannel,
    failure_limit: u32,
) -> Option<&'static str> {
    if !channel.enabled {
        return Some("disabled, delivers nothing");
    }
    if channel.awaiting_verification() {
        return Some("address never verified, delivers nothing");
    }
    if channel.is_failing(failure_limit) {
        return Some("recent alerts did not arrive");
    }
    None
}

/// Origin and path only: userinfo, query and fragment can each carry a token.
pub(super) fn safe_url(url: &url::Url) -> url::Url {
    let mut url = url.clone();
    strip_url_credentials(&mut url);
    url
}

pub(super) fn sorted(tags: &[String]) -> Vec<&str> {
    let mut v: Vec<&str> = tags.iter().map(String::as_str).collect();
    v.sort_unstable();
    v
}

pub(super) fn tag_list(tags: &[String]) -> String {
    if tags.is_empty() {
        "none".to_string()
    } else {
        tags.iter()
            .map(|t| sanitize_data(t))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(super) fn region_policy_view(policy: RegionIncidentPolicy) -> RegionPolicyArg {
    let (mode, count) = match policy {
        RegionIncidentPolicy::Any => (RegionPolicyMode::Any, None),
        RegionIncidentPolicy::Majority => (RegionPolicyMode::Majority, None),
        RegionIncidentPolicy::All => (RegionPolicyMode::All, None),
        RegionIncidentPolicy::Count(n) => (RegionPolicyMode::Count, Some(n)),
    };
    RegionPolicyArg { mode, count }
}

pub(super) fn region_policy_str(policy: RegionIncidentPolicy) -> String {
    match policy {
        RegionIncidentPolicy::Any => "any region down".to_string(),
        RegionIncidentPolicy::Majority => "a majority of regions down".to_string(),
        RegionIncidentPolicy::All => "every region down".to_string(),
        RegionIncidentPolicy::Count(n) => format!("{n} regions down"),
    }
}

/// One line a human can judge the trial run by.
pub(super) fn probe_line(p: &ProbeOutcome) -> String {
    let head = match (p.state.as_str(), p.http_status) {
        ("up", Some(code)) => format!("passed, HTTP {code}"),
        ("up", None) => "passed".to_string(),
        (state, Some(code)) => format!("{state}, HTTP {code}"),
        (state, None) => state.to_string(),
    };
    let result = match &p.error {
        Some(err) => format!("{head} in {}ms — {err}", p.duration_ms),
        None => format!("{head} in {}ms", p.duration_ms),
    };
    match &p.diagnostic {
        Some(diagnostic) => format!("{result}; {}", diagnostic.summary),
        None => result,
    }
}

/// Flags against the resolved set, not the raw column: a plan cap trims it.
pub(super) fn region_items(
    catalog: Vec<crate::storage::RegionOption>,
    applied_default: &[String],
) -> Vec<RegionItem> {
    catalog
        .into_iter()
        .map(|r| RegionItem {
            default_selected: applied_default.contains(&r.id),
            id: r.id,
            name: sanitize_data(&r.name),
            city: sanitize_data(&r.city),
            country_code: r.country_code,
            continent: r.continent,
        })
        .collect()
}

/// Reported only where it bites: a ceiling equal to the catalog invites naming
/// every row.
pub(super) fn region_cap(max_regions: i32, catalog_len: usize) -> Option<u32> {
    let cap = max_regions.max(0) as usize;
    (cap < catalog_len).then_some(cap as u32)
}

pub(super) fn region_health(r: crate::domain::metrics::RegionRollup) -> RegionHealth {
    RegionHealth {
        region: r.region,
        samples: r.samples,
        up: r.up,
        uptime_pct: (r.samples > 0).then(|| r.up as f64 / r.samples as f64 * 100.0),
        p50_ms: r.p50_ms,
        p95_ms: r.p95_ms,
        p99_ms: r.p99_ms,
        last_status: status_str(&r.last_status).to_string(),
    }
}

/// Structured view of what a check asserts. Built field by field rather than
/// serialising [`CheckSpec`], so a credential slot cannot reach the model by
/// being added upstream: HTTP credentials collapse to a boolean, header values
/// and the request body are masked, and a flow's fill values are dropped.
///
/// Header values and the body are masked rather than name-matched against a
/// denylist, on the reasoning [`redact_check_for_public`] already records: they
/// are where `Authorization` / `X-Api-Key` / `Cookie` live, and a value that
/// reaches a chat transcript is a value that has left the building. What the
/// model actually needs is which headers are sent, and it still gets that.
///
/// [`redact_check_for_public`]: crate::api::redaction
pub(super) fn check_config(check: &CheckSpec) -> CheckConfig {
    let ms = |d: &std::time::Duration| d.as_millis() as u64;
    match check {
        CheckSpec::Http(h) => CheckConfig::Http(HttpCheckConfig {
            url: sanitize_data(h.url.as_str()),
            method: format!("{:?}", h.method).to_uppercase(),
            timeout_ms: ms(&h.timeout),
            follow_redirects: h.follow_redirects,
            max_redirects: h.max_redirects,
            expected_status: expected_status_str(&h.expected_status),
            expected_body_contains: h.expected_body_contains.as_deref().map(sanitize_data),
            headers: h
                .headers
                .keys()
                .map(|k| (sanitize_data(k), REDACTED.to_string()))
                .collect(),
            body: h.body.as_ref().map(|_| REDACTED.to_string()),
            verify_tls: h.verify_tls,
            has_basic_auth: h.basic_auth.is_some(),
            has_bearer_token: h.bearer_token.is_some(),
        }),
        CheckSpec::Tcp(t) => CheckConfig::Tcp(TcpCheckConfig {
            host: sanitize_data(&t.host),
            port: t.port,
            timeout_ms: ms(&t.timeout),
        }),
        CheckSpec::Ping(p) => CheckConfig::Ping(PingCheckConfig {
            host: sanitize_data(&p.host),
            timeout_ms: ms(&p.timeout),
        }),
        CheckSpec::Heartbeat(h) => CheckConfig::Heartbeat(HeartbeatCheckConfig {
            period_secs: h.period.as_secs(),
            grace_secs: h.grace.as_secs(),
            max_runtime_secs: h.max_runtime.map(|d| d.as_secs()),
        }),
        CheckSpec::Dns(d) => CheckConfig::Dns(DnsCheckConfig {
            domain: sanitize_data(&d.domain),
            record_type: d.record_type.as_str().to_string(),
            resolver: d.resolver.as_deref().map(sanitize_data),
            expected_contains: d.expected_contains.as_deref().map(sanitize_data),
            timeout_ms: ms(&d.timeout),
        }),
        CheckSpec::TlsCert(c) => CheckConfig::TlsCert(TlsCertCheckConfig {
            host: sanitize_data(&c.host),
            port: c.port,
            server_name: c.server_name.as_deref().map(sanitize_data),
            warn_days: c.warn_days,
            critical_days: c.critical_days,
            timeout_ms: ms(&c.timeout),
        }),
        CheckSpec::DomainExpiry(d) => CheckConfig::DomainExpiry(DomainExpiryCheckConfig {
            domain: sanitize_data(&d.domain),
            warn_days: d.warn_days,
            critical_days: d.critical_days,
            timeout_ms: ms(&d.timeout),
        }),
        CheckSpec::Flow(f) => CheckConfig::Flow(FlowCheckConfig {
            start_url: sanitize_data(f.start_url.as_str()),
            steps: f
                .steps
                .iter()
                .enumerate()
                .map(|(i, s)| flow_step_config(u32::try_from(i + 1).unwrap_or(u32::MAX), s))
                .collect(),
            timeout_ms: ms(&f.timeout),
            step_timeout_ms: ms(&f.step_timeout),
            verify_tls: f.verify_tls,
        }),
    }
}

pub(super) fn flow_step_config(step: u32, s: &FlowStep) -> FlowStepConfig {
    let base = |op: &str| FlowStepConfig {
        step,
        op: op.to_string(),
        selector: None,
        url: None,
        contains: None,
        value_withheld: false,
    };
    match s {
        // A mid-flow nav URL can carry a one-time token, and only `start_url` is
        // already surfaced through `address`.
        FlowStep::Goto { url } => FlowStepConfig {
            url: Some(sanitize_data(safe_url(url).as_str())),
            ..base("goto")
        },
        FlowStep::Click { selector } => FlowStepConfig {
            selector: Some(sanitize_data(selector)),
            ..base("click")
        },
        FlowStep::Fill { selector, .. } => FlowStepConfig {
            selector: Some(sanitize_data(selector)),
            value_withheld: true,
            ..base("fill")
        },
        FlowStep::WaitFor { selector } => FlowStepConfig {
            selector: Some(sanitize_data(selector)),
            ..base("wait_for")
        },
        FlowStep::AssertText { selector, contains } => FlowStepConfig {
            selector: selector.as_deref().map(sanitize_data),
            contains: Some(sanitize_data(contains)),
            ..base("assert_text")
        },
        FlowStep::AssertUrl { contains } => FlowStepConfig {
            contains: Some(sanitize_data(contains)),
            ..base("assert_url")
        },
    }
}

/// The passing status codes as one phrase, so "why was this a failure?" is
/// answerable without the model reconstructing a shape.
pub(super) fn expected_status_str(e: &ExpectedStatus) -> String {
    match e {
        ExpectedStatus::Exact(c) => c.to_string(),
        ExpectedStatus::Range { min, max } => format!("{min}-{max}"),
        ExpectedStatus::OneOf(codes) => codes
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Per-phase timing from a check result.
pub(super) fn check_timing(r: &CheckResult) -> CheckTiming {
    CheckTiming {
        dns_ms: r.dns_ms,
        connect_ms: r.connect_ms,
        tls_ms: r.tls_ms,
        ttfb_ms: r.ttfb_ms,
    }
}

pub(super) fn incident_summary(i: &IncidentBrief) -> IncidentSummary {
    IncidentSummary {
        id: i.id.to_string(),
        monitor_id: i.target_id.to_string(),
        monitor_name: sanitize_data(&i.target_name),
        severity: i.severity.as_db_str().to_string(),
        opened_at: i.started_at.to_rfc3339(),
        resolved_at: i.ended_at.map(|t| t.to_rfc3339()),
        latest_phase: i
            .latest_update
            .as_ref()
            .map(|u| u.phase.as_db_str().to_string()),
        latest_update_at: i.latest_update.as_ref().map(|u| u.posted_at.to_rfc3339()),
    }
}

/// Callers pass the raw incident; error text is humanized and scrubbed here.
pub(super) fn incident_detail(i: &Incident, monitor_name: Option<String>) -> IncidentDetail {
    IncidentDetail {
        id: i.id.to_string(),
        monitor_id: i.target_id.map(|t| t.to_string()).unwrap_or_default(),
        monitor_name: monitor_name.map(|n| sanitize_data(&n)),
        state: i.status.as_str().to_string(),
        severity: i.severity.as_db_str().to_string(),
        opened_at: i.started_at.to_rfc3339(),
        resolved_at: i.ended_at.map(|e| e.to_rfc3339()),
        error_sample: i.error_sample.as_deref().map(present_error),
        regions_down: i.regions_down.iter().map(|r| sanitize_data(r)).collect(),
        regions_up: i.regions_up.iter().map(|r| sanitize_data(r)).collect(),
        updates: i
            .updates
            .iter()
            .map(|u| IncidentUpdateItem {
                posted_at: u.posted_at.to_rfc3339(),
                phase: u.phase.as_db_str().to_string(),
                message: sanitize_data(&u.message),
            })
            .collect(),
    }
}

/// Prefers the folded status: raw `last_status` is whichever region reported
/// most recently, so alone it calls a monitor down over one failing region.
pub(super) fn current_state(
    metrics: Option<&DashboardMetrics>,
    folded: Option<CheckStatus>,
) -> &'static str {
    match metrics {
        Some(m) if m.samples > 0 => match folded {
            Some(f) => f.as_str(),
            None => status_str(&m.last_status),
        },
        _ => "no_data",
    }
}

/// A stored status string as one of the states the tools document. An
/// unexpected value degrades to `no_data` rather than leaking it.
pub(super) fn status_str(stored: &str) -> &'static str {
    match stored {
        "up" => "up",
        "down" => "down",
        "degraded" => "degraded",
        "error" => "error",
        _ => "no_data",
    }
}

pub(super) fn ts_to_rfc3339(secs: Option<i64>) -> Option<String> {
    secs.and_then(|s| Utc.timestamp_opt(s, 0).single())
        .map(|dt: DateTime<Utc>| dt.to_rfc3339())
}

pub(super) fn ms_to_rfc3339(ms: i64) -> Option<String> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt: DateTime<Utc>| dt.to_rfc3339())
}

pub(super) fn flow_run_item(v: crate::storage::traits::FlowRunView) -> FlowRunItem {
    let stopped = v.stopped_step;
    FlowRunItem {
        at: v.timestamp.to_rfc3339(),
        region: sanitize_data(&v.region),
        state: v.status.as_str().to_string(),
        duration_ms: v.duration_ms,
        // Stored as an index; the error text counts from one.
        failed_step: stopped.and_then(|i| u32::try_from(i + 1).ok()),
        error: v.error.as_deref().map(present_error),
        steps: v
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| FlowStepRun {
                step: u32::try_from(i + 1).unwrap_or(u32::MAX),
                op: s.op.clone(),
                outcome: s.outcome.as_str().to_string(),
                duration_ms: s.duration_ms,
            })
            .collect(),
        // Console lines are left out: long, and the URL and text name the fault.
        evidence: v.evidence.map(|e| FlowRunEvidence {
            final_url: e.final_url.as_deref().map(sanitize_data),
            title: e.title.as_deref().map(sanitize_data),
            text_snippet: e.text_snippet.as_deref().map(sanitize_data),
        }),
        evidence_expired: v.evidence_expired,
    }
}

pub(super) fn step_trend_item(t: crate::domain::metrics::FlowStepTrend) -> FlowStepTrendItem {
    // A bucket carries no mean when nothing passed it, so the ends are the
    // outermost slices that timed anything.
    let first = t.buckets.iter().find_map(|b| b.avg);
    let last = t.buckets.iter().rev().find_map(|b| b.avg);
    FlowStepTrendItem {
        step: u32::from(t.step) + 1,
        op: t.op,
        first_ms: first,
        last_ms: last,
        change_ratio: first
            .zip(last)
            .filter(|(f, _)| *f > 0)
            .map(|(f, l)| (f64::from(l) / f64::from(f) * 100.0).round() / 100.0),
        samples: t.buckets.iter().map(|b| b.samples).sum(),
        failed: t.buckets.iter().map(|b| b.failed).sum(),
    }
}

pub(super) fn visibility_result(
    id: Uuid,
    visibility: IncidentVisibility,
) -> IncidentVisibilityResult {
    IncidentVisibilityResult {
        incident_id: id.to_string(),
        visibility: visibility.as_db_str().to_string(),
    }
}
