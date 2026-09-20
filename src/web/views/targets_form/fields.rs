//! Per-check-kind field groups: what the form renders for an http, tcp, ping,
//! heartbeat, dns, tls_cert, domain_expiry or flow monitor.

use crate::templates::format::exact_duration;

/// Starting budget for the kinds that open a connection and wait on a third
/// party's response: http, tls_cert, domain_expiry. Matches what the MCP
/// surface already defaults to, so a monitor means the same thing whichever
/// surface created it. The protocol-level kinds stay tighter.
const ROUND_TRIP_TIMEOUT_MS: u64 = 10_000;

/// One HTTP header in the form's key/value row repeater.
pub struct HeaderPair {
    pub name: String,
    pub value: String,
}

pub struct HttpFields {
    pub url: String,
    pub method: &'static str,
    pub timeout_ms: u64,
    pub follow_redirects: bool,
    pub max_redirects: u8,
    /// Single-input replacement for the old radio + 3 fields. Accepts
    /// "200", "200-299", "200, 201, 204". JS parses on submit.
    pub expected_status_input: String,
    pub expected_body_contains: String,
    pub headers: Vec<HeaderPair>,
    pub body: String,
    pub verify_tls: bool,
}

impl Default for HttpFields {
    fn default() -> Self {
        Self {
            url: String::new(),
            method: "GET",
            // A probe crossing continents to a proxied origin has a fat tail:
            // legitimate responses land past 5s often enough that a tighter
            // budget reports the probe's impatience as the target being down.
            timeout_ms: ROUND_TRIP_TIMEOUT_MS,
            // Follow by default: most real targets (apex domains, http→https)
            // 301 to a canonical host, and a fresh monitor pointed at them
            // should report Up, not Down on the redirect.
            follow_redirects: true,
            max_redirects: 5,
            expected_status_input: "200-299".into(),
            expected_body_contains: String::new(),
            headers: Vec::new(),
            body: String::new(),
            verify_tls: true,
        }
    }
}

pub struct TcpFields {
    pub host: String,
    pub port: u16,
    pub timeout_ms: u64,
}

impl Default for TcpFields {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 0,
            timeout_ms: 3_000,
        }
    }
}

pub struct PingFields {
    pub host: String,
    pub timeout_ms: u64,
}

impl Default for PingFields {
    fn default() -> Self {
        Self {
            host: String::new(),
            timeout_ms: 3_000,
        }
    }
}

pub struct HeartbeatFields {
    pub period_s: u64,
    pub grace_s: u64,
    /// Cap on a `/start`ed run's length; 0 renders the field empty, meaning off.
    pub max_runtime_s: u64,
    pub cadence: Option<CadenceHint>,
}

pub struct CadenceHint {
    pub observed_s: u64,
    pub suggested_s: u64,
    pub too_tight: bool,
}

impl Default for HeartbeatFields {
    fn default() -> Self {
        Self {
            period_s: 86_400,
            grace_s: 3_600,
            max_runtime_s: 0,
            cadence: None,
        }
    }
}

impl HeartbeatFields {
    pub fn period_presets(&self) -> Vec<DurationChoice> {
        duration_presets(&[300, 900, 3_600, 21_600, 86_400], self.period_s)
    }

    pub fn grace_presets(&self) -> Vec<DurationChoice> {
        duration_presets(&[0, 60, 300, 900, 3_600], self.grace_s)
    }

    pub fn max_runtime_presets(&self) -> Vec<DurationChoice> {
        duration_presets(&[0, 300, 900, 3_600, 21_600], self.max_runtime_s)
    }

    pub fn period_value(&self) -> String {
        exact_duration(self.period_s)
    }

    pub fn grace_value(&self) -> String {
        exact_duration(self.grace_s)
    }

    /// Empty is the off state, which the `off` preset also selects.
    pub fn max_runtime_value(&self) -> String {
        if self.max_runtime_s == 0 {
            String::new()
        } else {
            exact_duration(self.max_runtime_s)
        }
    }
}

pub struct DurationChoice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

pub(super) fn duration_presets(presets: &[u64], current: u64) -> Vec<DurationChoice> {
    presets
        .iter()
        .map(|&secs| DurationChoice {
            value: if secs == 0 {
                String::new()
            } else {
                exact_duration(secs)
            },
            label: if secs == 0 {
                "off".to_string()
            } else {
                exact_duration(secs)
            },
            selected: secs == current,
        })
        .collect()
}

pub struct DnsFields {
    pub domain: String,
    pub record_type: &'static str,
    pub resolver: String,
    pub expected_contains: String,
    pub timeout_ms: u64,
}

impl Default for DnsFields {
    fn default() -> Self {
        Self {
            domain: String::new(),
            record_type: "A",
            resolver: String::new(),
            expected_contains: String::new(),
            timeout_ms: 3_000,
        }
    }
}

pub struct TlsCertFields {
    pub host: String,
    pub port: u16,
    pub server_name: String,
    pub warn_days: u32,
    pub critical_days: u32,
    pub timeout_ms: u64,
}

impl Default for TlsCertFields {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 443,
            server_name: String::new(),
            warn_days: 30,
            critical_days: 7,
            timeout_ms: ROUND_TRIP_TIMEOUT_MS,
        }
    }
}

pub struct DomainExpiryFields {
    pub domain: String,
    pub warn_days: u32,
    pub critical_days: u32,
    pub timeout_ms: u64,
}

impl Default for DomainExpiryFields {
    fn default() -> Self {
        Self {
            domain: String::new(),
            warn_days: 30,
            critical_days: 7,
            timeout_ms: ROUND_TRIP_TIMEOUT_MS,
        }
    }
}

impl DomainExpiryFields {
    pub fn registered_domain_hint(&self) -> Option<String> {
        crate::domain::reduced_domain_hint(&self.domain)
    }
}

/// One step row in the flow builder. Only the fields the step's `op` uses are
/// populated; the rest stay empty and the template hides them.
pub struct FlowStepFields {
    pub op: &'static str,
    pub url: String,
    pub selector: String,
    pub value: String,
    pub contains: String,
}

pub struct FlowFields {
    pub start_url: String,
    pub steps: Vec<FlowStepFields>,
    pub timeout_s: u64,
    pub step_timeout_s: u64,
    pub verify_tls: bool,
}

impl Default for FlowFields {
    fn default() -> Self {
        Self {
            start_url: String::new(),
            steps: Vec::new(),
            timeout_s: 30,
            step_timeout_s: 10,
            verify_tls: true,
        }
    }
}

/// Build the flow form fields from a stored monitor. The owner's edit form shows
/// the real fill values so they can see and adjust their test credentials without
/// re-typing; this is an authenticated, edit-scoped owner surface. Every non-edit
/// surface masks them: the detail config panel and API via `redact_check`, the
/// public share view via `redact_check_for_public`.
pub(super) fn flow_fields_from(f: crate::domain::FlowCheck) -> FlowFields {
    use crate::domain::FlowStep;
    let steps = f
        .steps
        .into_iter()
        .map(|s| {
            let mut r = FlowStepFields {
                op: "",
                url: String::new(),
                selector: String::new(),
                value: String::new(),
                contains: String::new(),
            };
            match s {
                FlowStep::Goto { url } => {
                    r.op = "goto";
                    r.url = url.to_string();
                }
                FlowStep::Click { selector } => {
                    r.op = "click";
                    r.selector = selector;
                }
                FlowStep::Fill { selector, value } => {
                    r.op = "fill";
                    r.selector = selector;
                    r.value = value;
                }
                FlowStep::WaitFor { selector } => {
                    r.op = "wait_for";
                    r.selector = selector;
                }
                FlowStep::AssertText { selector, contains } => {
                    r.op = "assert_text";
                    r.selector = selector.unwrap_or_default();
                    r.contains = contains;
                }
                FlowStep::AssertUrl { contains } => {
                    r.op = "assert_url";
                    r.contains = contains;
                }
            }
            r
        })
        .collect();
    FlowFields {
        start_url: f.start_url.to_string(),
        steps,
        timeout_s: f.timeout.as_secs(),
        step_timeout_s: f.step_timeout.as_secs(),
        verify_tls: f.verify_tls,
    }
}
