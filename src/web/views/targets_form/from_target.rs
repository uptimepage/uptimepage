//! A stored monitor read back into form fields, for the edit and copy paths.

use crate::domain::{CheckSpec, ExpectedStatus, HttpMethod, Target};
use crate::error::AppError;

use super::fields::{
    DnsFields, DomainExpiryFields, FlowFields, HeaderPair, HeartbeatFields, HttpFields, PingFields,
    TcpFields, TlsCertFields, flow_fields_from,
};
use super::model::{EscalationOffer, FormModel};

/// Whether `form_from_target` produces an edit form (PATCH the same monitor)
/// or a copy (POST a new monitor seeded from an existing one).
pub(super) enum FormKind {
    Edit,
    Copy,
}

pub(super) fn empty_create_form() -> FormModel {
    FormModel {
        mode: "create",
        id: String::new(),
        action: "/api/v1/targets".into(),
        submit_method: "POST",
        name: String::new(),
        interval_s: 60,
        min_interval_s: 60,
        carried_over: false,
        enabled: true,
        tags: Vec::new(),
        group_name: String::new(),
        group_options: Vec::new(),
        tag_options: Vec::new(),
        owner_user_id: String::new(),
        owner_options: Vec::new(),
        check_type: "http",
        http: HttpFields::default(),
        tcp: TcpFields::default(),
        ping: PingFields::default(),
        heartbeat: HeartbeatFields::default(),
        dns: DnsFields::default(),
        tls_cert: TlsCertFields::default(),
        domain_expiry: DomainExpiryFields::default(),
        flow: FlowFields::default(),
        channels: Vec::new(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        recovery_period_secs: crate::domain::default_recovery_period_secs_for_kind("http"),
        escalation_choices: Vec::new(),
        escalation_hint: String::new(),
        escalation: EscalationOffer::Open,
        region_groups: Vec::new(),
        region_threshold_options: Vec::new(),
        show_regions: false,
        flow_available: false,
    }
}
pub(super) fn form_from_target(t: Target, kind: FormKind) -> Result<FormModel, AppError> {
    let tags = t.tags;
    let group_name = t.group_name.unwrap_or_default();
    let owner_user_id = t.owner_user_id.map(|id| id.to_string()).unwrap_or_default();

    let mut http = HttpFields::default();
    let mut tcp = TcpFields::default();
    let mut ping = PingFields::default();
    let mut heartbeat = HeartbeatFields::default();
    let mut dns = DnsFields::default();
    let mut tls_cert = TlsCertFields::default();
    let mut domain_expiry = DomainExpiryFields::default();
    let mut flow = FlowFields::default();
    let check_type: &'static str = match t.check {
        CheckSpec::Http(h) => {
            http = http_fields_from(h);
            "http"
        }
        CheckSpec::Tcp(c) => {
            tcp = TcpFields {
                host: c.host,
                port: c.port,
                timeout_ms: c.timeout.as_millis() as u64,
            };
            "tcp"
        }
        CheckSpec::Ping(c) => {
            ping = PingFields {
                host: c.host,
                timeout_ms: c.timeout.as_millis() as u64,
            };
            "ping"
        }
        CheckSpec::Heartbeat(c) => {
            heartbeat = HeartbeatFields {
                period_s: c.period.as_secs(),
                grace_s: c.grace.as_secs(),
                max_runtime_s: c.max_runtime.map_or(0, |d| d.as_secs()),
                ..Default::default()
            };
            "heartbeat"
        }
        CheckSpec::Manual(_) => "manual",
        CheckSpec::Dns(d) => {
            dns = DnsFields {
                domain: d.domain,
                record_type: d.record_type.as_str(),
                resolver: d.resolver.unwrap_or_default(),
                expected_contains: d.expected_contains.unwrap_or_default(),
                timeout_ms: d.timeout.as_millis() as u64,
            };
            "dns"
        }
        CheckSpec::TlsCert(c) => {
            tls_cert = TlsCertFields {
                host: c.host,
                port: c.port,
                server_name: c.server_name.unwrap_or_default(),
                warn_days: c.warn_days,
                critical_days: c.critical_days,
                timeout_ms: c.timeout.as_millis() as u64,
            };
            "tls_cert"
        }
        CheckSpec::DomainExpiry(d) => {
            domain_expiry = DomainExpiryFields {
                domain: d.domain,
                warn_days: d.warn_days,
                critical_days: d.critical_days,
                timeout_ms: d.timeout.as_millis() as u64,
            };
            "domain_expiry"
        }
        CheckSpec::Flow(f) => {
            flow = flow_fields_from(f);
            "flow"
        }
    };

    let (mode, id, action, submit_method, name) = match kind {
        FormKind::Edit => (
            "edit",
            t.id.to_string(),
            format!("/api/v1/targets/{}", t.id),
            "PATCH",
            t.name,
        ),
        FormKind::Copy => (
            "create",
            String::new(),
            "/api/v1/targets".into(),
            "POST",
            format!("{} (copy)", t.name),
        ),
    };

    let alert_confirmations = t.alert_confirmations;
    let notify_recovery = t.notify_recovery;
    let renotify_interval_secs = t.renotify_interval_secs;
    let recovery_period_secs = t.recovery_period_secs;

    Ok(FormModel {
        mode,
        id,
        action,
        submit_method,
        name,
        interval_s: t.interval.as_secs(),
        // Overwritten by the handler with the org plan's real floor.
        min_interval_s: 60,
        carried_over: true,
        enabled: t.enabled,
        tags,
        group_name,
        // Populated by the handler from `distinct_groups` / `list_tags`.
        group_options: Vec::new(),
        tag_options: Vec::new(),
        owner_user_id,
        // Populated by the handler from `orgs::list_members`.
        owner_options: Vec::new(),
        check_type,
        http,
        tcp,
        ping,
        heartbeat,
        dns,
        tls_cert,
        domain_expiry,
        flow,
        channels: Vec::new(),
        alert_confirmations,
        notify_recovery,
        renotify_interval_secs,
        recovery_period_secs,
        escalation_choices: Vec::new(),
        escalation_hint: String::new(),
        escalation: EscalationOffer::Open,
        region_groups: Vec::new(),
        region_threshold_options: Vec::new(),
        show_regions: false,
        flow_available: false,
    })
}

pub(super) fn http_fields_from(h: crate::domain::HttpCheck) -> HttpFields {
    let expected_status_input = match h.expected_status {
        ExpectedStatus::Exact(c) => c.to_string(),
        ExpectedStatus::Range { min, max } => format!("{min}-{max}"),
        ExpectedStatus::OneOf(v) => v.iter().map(u16::to_string).collect::<Vec<_>>().join(", "),
    };
    let mut headers: Vec<HeaderPair> = h
        .headers
        .into_iter()
        .map(|(name, value)| HeaderPair { name, value })
        .collect();
    headers.sort_by(|a, b| a.name.cmp(&b.name));
    HttpFields {
        url: h.url.to_string(),
        method: http_method_str(h.method),
        timeout_ms: h.timeout.as_millis() as u64,
        follow_redirects: h.follow_redirects,
        max_redirects: h.max_redirects,
        expected_status_input,
        expected_body_contains: h.expected_body_contains.unwrap_or_default(),
        headers,
        body: h.body.unwrap_or_default(),
        verify_tls: h.verify_tls,
    }
}

pub(super) fn http_method_str(m: HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Head => "HEAD",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Options => "OPTIONS",
    }
}
