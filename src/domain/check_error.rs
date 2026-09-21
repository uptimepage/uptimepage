//! Single source for turning a probe's terse error code into display text,
//! shared by the web views, the JSON API, and MCP, plus the bounded
//! classification of that same code for operator metrics.

use crate::domain::strip_served_stale;

pub fn humanize_check_error(raw: &str) -> String {
    let raw = match strip_served_stale(raw) {
        Some(r) => r,
        None => return "using last known result".into(),
    };
    if raw.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw)
            && let Some(days) = v.get("days_remaining").and_then(|d| d.as_i64())
        {
            if let Some(cn) = v.get("subject_common_name").and_then(|s| s.as_str()) {
                return fmt_days_label("cert", days, cn);
            }
            if let Some(domain) = v.get("domain").and_then(|s| s.as_str()) {
                return fmt_days_label("domain", days, domain);
            }
        }
        // Never hand a raw JSON blob to a reader.
        return "check failed".into();
    }
    match raw {
        "timeout" => "timed out".into(),
        "connect timeout" => "couldn't connect (timed out)".into(),
        "no response" => "connected, but no response (timed out)".into(),
        "body timeout" => "response body timed out".into(),
        "tls" => "TLS handshake failed".into(),
        "tls handshake rejected" => "server rejected the TLS handshake".into(),
        "tls version or cipher mismatch" => "no TLS version or cipher in common".into(),
        "malformed tls response" => "malformed TLS response".into(),
        "tls handshake reset" => "TLS handshake reset".into(),
        "tls handshake closed early" => "server closed the TLS handshake early".into(),
        "certificate chain incomplete" => {
            "incomplete certificate chain (server sent only its own certificate)".into()
        }
        "certificate self-signed" => "self-signed certificate".into(),
        "connect" => "connection failed".into(),
        "transport" => "transport error".into(),
        "reset before response" => "server reset the connection before responding".into(),
        "closed before response" => "server closed the connection before responding".into(),
        "invalid response" => "server sent an invalid HTTP response".into(),
        "response headers too large" => "response headers exceed the probe's limit".into(),
        "h2 protocol error" => "HTTP/2 protocol error".into(),
        "h2 internal error" => "server reported an HTTP/2 internal error".into(),
        "h2 stream refused" => "server refused the HTTP/2 stream".into(),
        "h2 stream cancelled" => "server cancelled the HTTP/2 stream".into(),
        "h2 enhance your calm" => {
            "server throttled the connection (HTTP/2 ENHANCE_YOUR_CALM)".into()
        }
        "dns: domain not found" => "domain not found (DNS)".into(),
        "dns: no address records" => "no DNS address records".into(),
        "dns: lookup timed out" => "DNS lookup timed out".into(),
        "dns: lookup failed" => "DNS lookup failed".into(),
        other => humanize_prefixed(other),
    }
}

/// The request-phase fallbacks carry the peer's own text after a stable
/// prefix; only the prefix is reworded.
fn humanize_prefixed(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("transport: ") {
        return format!("transport error: {rest}");
    }
    if let Some(rest) = raw.strip_prefix("h2 rejected locally: ") {
        return format!(
            "HTTP/2 exchange rejected by the probe ({rest}): malformed frames, or over its limits"
        );
    }
    if let Some(rest) = raw.strip_prefix("h2 ") {
        return format!("HTTP/2 error: {rest}");
    }
    raw.into()
}

/// Who failed. Only `Internal` is alertable — the other three persist
/// legitimately for hours whenever customer sites are down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorFamily {
    /// This node could not run the check at all. Nothing here depends on what
    /// the target did, so none of it has an honest steady state.
    Internal,
    Transport,
    Verdict,
    /// Unclassified free text. Its size measures what the taxonomy still misses.
    Other,
}

impl ErrorFamily {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Transport => "transport",
            Self::Verdict => "verdict",
            Self::Other => "other",
        }
    }
}

/// A probe error reduced to a fixed set of names. Raw errors interpolate
/// hostnames, IPs and vendor text, so they are unbounded and carry customer
/// data; they can never label a metric, and a class can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    HeartbeatStateUnavailable,
    FlowNotConfigured,
    HeartbeatNotProbed,
    Body,
    Decode,
    CircuitOpen,
    Timeout,
    ConnectTimeout,
    Connect,
    ConnectionRefused,
    ConnectionReset,
    HostUnreachable,
    NetworkUnreachable,
    NoResponse,
    Tls,
    Transport,
    BodyTimeout,
    DnsFailed,
    PingFailed,
    WhoisLookup,
    RdapLookup,
    UnexpectedStatus,
    RateLimited,
    BodyMatchFailed,
    BodyOverCap,
    DnsNoRecord,
    AddressNotAllowed,
    CertChain,
    CertExpired,
    CertNotYetValid,
    CertRevoked,
    CertNotTrusted,
    CertHostnameMismatch,
    CertInvalid,
    CertExpiry,
    DomainExpiry,
    FlowStep,
    HeartbeatJob,
    HeartbeatMissed,
    Other,
}

impl ErrorClass {
    /// Lets a sweep zero the classes it did not see; an unwritten gauge freezes
    /// at its last value and the alert on it never clears.
    pub const ALL: &'static [ErrorClass] = &[
        Self::HeartbeatStateUnavailable,
        Self::FlowNotConfigured,
        Self::HeartbeatNotProbed,
        Self::Body,
        Self::Decode,
        Self::CircuitOpen,
        Self::Timeout,
        Self::ConnectTimeout,
        Self::Connect,
        Self::ConnectionRefused,
        Self::ConnectionReset,
        Self::HostUnreachable,
        Self::NetworkUnreachable,
        Self::NoResponse,
        Self::Tls,
        Self::Transport,
        Self::BodyTimeout,
        Self::DnsFailed,
        Self::PingFailed,
        Self::WhoisLookup,
        Self::RdapLookup,
        Self::UnexpectedStatus,
        Self::RateLimited,
        Self::BodyMatchFailed,
        Self::BodyOverCap,
        Self::DnsNoRecord,
        Self::AddressNotAllowed,
        Self::CertChain,
        Self::CertExpired,
        Self::CertNotYetValid,
        Self::CertRevoked,
        Self::CertNotTrusted,
        Self::CertHostnameMismatch,
        Self::CertInvalid,
        Self::CertExpiry,
        Self::DomainExpiry,
        Self::FlowStep,
        Self::HeartbeatJob,
        Self::HeartbeatMissed,
        Self::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeartbeatStateUnavailable => "heartbeat_state_unavailable",
            Self::FlowNotConfigured => "flow_not_configured",
            Self::HeartbeatNotProbed => "heartbeat_not_probed",
            Self::Body => "body",
            Self::Decode => "decode",
            Self::CircuitOpen => "circuit_open",
            Self::Timeout => "timeout",
            Self::ConnectTimeout => "connect_timeout",
            Self::Connect => "connect",
            Self::ConnectionRefused => "connection_refused",
            Self::ConnectionReset => "connection_reset",
            Self::HostUnreachable => "host_unreachable",
            Self::NetworkUnreachable => "network_unreachable",
            Self::NoResponse => "no_response",
            Self::Tls => "tls",
            Self::Transport => "transport",
            Self::BodyTimeout => "body_timeout",
            Self::DnsFailed => "dns_failed",
            Self::PingFailed => "ping_failed",
            Self::WhoisLookup => "whois_lookup",
            Self::RdapLookup => "rdap_lookup",
            Self::UnexpectedStatus => "unexpected_status",
            Self::RateLimited => "rate_limited",
            Self::BodyMatchFailed => "body_match_failed",
            Self::BodyOverCap => "body_over_cap",
            Self::DnsNoRecord => "dns_no_record",
            Self::AddressNotAllowed => "address_not_allowed",
            Self::CertChain => "cert_chain",
            Self::CertExpired => "cert_expired",
            Self::CertNotYetValid => "cert_not_yet_valid",
            Self::CertRevoked => "cert_revoked",
            Self::CertNotTrusted => "cert_not_trusted",
            Self::CertHostnameMismatch => "cert_hostname_mismatch",
            Self::CertInvalid => "cert_invalid",
            Self::CertExpiry => "cert_expiry",
            Self::DomainExpiry => "domain_expiry",
            Self::FlowStep => "flow_step",
            Self::HeartbeatJob => "heartbeat_job",
            Self::HeartbeatMissed => "heartbeat_missed",
            Self::Other => "other",
        }
    }

    pub const fn family(self) -> ErrorFamily {
        match self {
            Self::HeartbeatStateUnavailable
            | Self::FlowNotConfigured
            | Self::HeartbeatNotProbed => ErrorFamily::Internal,
            Self::Timeout
            | Self::ConnectTimeout
            | Self::Connect
            | Self::ConnectionRefused
            | Self::ConnectionReset
            | Self::HostUnreachable
            | Self::NetworkUnreachable
            | Self::NoResponse
            | Self::Tls
            | Self::Transport
            // A slow origin starves the body read like any other phase.
            | Self::BodyTimeout
            // A mid-stream read failure is the connection dying, not us, and a
            // body that will not decode is bytes the origin mangled. Refusing an
            // oversized one is our decision, so that is `BodyOverCap` below.
            | Self::Body
            | Self::Decode
            // An open breaker on a target that keeps failing is honest.
            | Self::CircuitOpen
            | Self::DnsFailed
            | Self::PingFailed
            | Self::WhoisLookup
            | Self::RdapLookup => ErrorFamily::Transport,
            Self::UnexpectedStatus
            | Self::RateLimited
            | Self::BodyMatchFailed
            // Either cap with a body assertion on it is a settled fact about
            // the page, like a failed match.
            | Self::BodyOverCap
            | Self::DnsNoRecord
            // The target resolved to an address policy forbids probing.
            | Self::AddressNotAllowed
            | Self::CertChain
            // A certificate the target presented and we refused. Persists until
            // they reissue, so none of these can ever be `Internal`.
            | Self::CertExpired
            | Self::CertNotYetValid
            | Self::CertRevoked
            | Self::CertNotTrusted
            | Self::CertHostnameMismatch
            | Self::CertInvalid
            | Self::CertExpiry
            | Self::DomainExpiry
            | Self::FlowStep
            | Self::HeartbeatJob
            | Self::HeartbeatMissed => ErrorFamily::Verdict,
            Self::Other => ErrorFamily::Other,
        }
    }
}

pub fn classify_check_error(raw: &str) -> ErrorClass {
    // A stale-served error always carries the payload it stood in for, so the
    // payload's own class wins. A bare annotation would be unclassified, which
    // `Other` reports honestly rather than inventing a class for it.
    let Some(raw) = strip_served_stale(raw) else {
        return ErrorClass::Other;
    };
    if raw.starts_with('{') {
        return classify_json(raw);
    }
    match raw {
        "body" => return ErrorClass::Body,
        "decode" => return ErrorClass::Decode,
        "heartbeat state unavailable on this node" => {
            return ErrorClass::HeartbeatStateUnavailable;
        }
        "flow engine not configured on this node" => return ErrorClass::FlowNotConfigured,
        "heartbeat monitors are evaluated on the control plane, not probed" => {
            return ErrorClass::HeartbeatNotProbed;
        }
        "circuit_open" => return ErrorClass::CircuitOpen,
        "timeout" => return ErrorClass::Timeout,
        "connect timeout" => return ErrorClass::ConnectTimeout,
        "connect" => return ErrorClass::Connect,
        // Mirrors `tcp_reason` in http_client::connector — keep the two in step.
        "connection refused" => return ErrorClass::ConnectionRefused,
        "connection reset" => return ErrorClass::ConnectionReset,
        "host unreachable" => return ErrorClass::HostUnreachable,
        "network unreachable" => return ErrorClass::NetworkUnreachable,
        "address not allowed" => return ErrorClass::AddressNotAllowed,
        // Mirrors `tls_reason` in the same module.
        "certificate expired" => return ErrorClass::CertExpired,
        "certificate not yet valid" => return ErrorClass::CertNotYetValid,
        "certificate revoked" => return ErrorClass::CertRevoked,
        "certificate not trusted" | "certificate self-signed" => {
            return ErrorClass::CertNotTrusted;
        }
        "certificate hostname mismatch" => return ErrorClass::CertHostnameMismatch,
        "certificate invalid" => return ErrorClass::CertInvalid,
        "rdap timeout" | "rdap throttled" => return ErrorClass::RdapLookup,
        "no response" => return ErrorClass::NoResponse,
        "tls" => return ErrorClass::Tls,
        // The TLS-phase reasons share `Tls`: the class is what dashboards
        // aggregate on and the phase has not changed, only how precisely the
        // customer is told about it.
        "tls handshake rejected"
        | "tls version or cipher mismatch"
        | "malformed tls response"
        | "tls handshake reset"
        | "tls handshake closed early" => return ErrorClass::Tls,
        // Mirrors `classify_hyper_error` in worker::http_check: the request
        // phase, after connect and TLS both succeeded.
        "transport"
        | "reset before response"
        | "closed before response"
        | "invalid response"
        | "response headers too large"
        | "h2 protocol error"
        | "h2 internal error"
        | "h2 stream refused"
        | "h2 stream cancelled"
        | "h2 enhance your calm" => return ErrorClass::Transport,
        "body timeout" => return ErrorClass::BodyTimeout,
        "body match failed" => return ErrorClass::BodyMatchFailed,
        "dns: domain not found" | "dns: no address records" => {
            return ErrorClass::DnsNoRecord;
        }
        "server returned no certificate chain"
        | "empty certificate chain"
        | "certificate chain incomplete" => {
            return ErrorClass::CertChain;
        }
        _ => {}
    }
    // Matched by stem: the worker interpolates the cap it compiled with, and
    // there are two of them, raw and decoded.
    if raw.starts_with("body over the ")
        && (raw.ends_with(" read cap") || raw.ends_with(" decoded cap"))
    {
        return ErrorClass::BodyOverCap;
    }
    if raw.starts_with("unexpected status ") {
        return ErrorClass::UnexpectedStatus;
    }
    if raw.starts_with("rate-limited ") {
        return ErrorClass::RateLimited;
    }
    if raw.starts_with("dns: ") {
        return ErrorClass::DnsFailed;
    }
    // `h2 <REASON>` for the rarer GOAWAY/RST_STREAM codes and `transport: <root
    // cause>` for anything hyper raised without a mapped shape.
    if raw.starts_with("h2 ") || raw.starts_with("transport: ") {
        return ErrorClass::Transport;
    }
    if raw.starts_with("parsing leaf certificate: ") {
        return ErrorClass::CertChain;
    }
    // The non-HTTP kinds interpolate the host into the SSRF rejection, so this
    // is the same condition HTTP reports as the exact `address not allowed`.
    if raw.starts_with("no allowed addresses for ") {
        return ErrorClass::AddressNotAllowed;
    }
    // `echo to <ip> failed: …` and `no echo reply from <ip> within <n>ms` both
    // interpolate the address, so neither can be matched whole.
    if raw.starts_with("echo to ") || raw.starts_with("no echo reply from ") {
        return ErrorClass::PingFailed;
    }
    if raw.starts_with("connecting to WHOIS server ")
        || raw.starts_with("sending WHOIS query to ")
        || raw.starts_with("reading WHOIS response from ")
    {
        return ErrorClass::WhoisLookup;
    }
    if raw.starts_with("invalid RDAP base url ") || raw.starts_with("building RDAP request for ") {
        return ErrorClass::RdapLookup;
    }
    if raw.starts_with("job reported failure") || raw.starts_with("job started ") {
        return ErrorClass::HeartbeatJob;
    }
    if raw.starts_with("no ping for ") {
        return ErrorClass::HeartbeatMissed;
    }
    // `step N/M op: reason`
    if raw.starts_with("step ") && raw.contains(": ") {
        return ErrorClass::FlowStep;
    }
    ErrorClass::Other
}

fn classify_json(raw: &str) -> ErrorClass {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return ErrorClass::Other;
    };
    if v.get("days_remaining").is_none() {
        return ErrorClass::Other;
    }
    if v.get("subject_common_name").is_some() {
        return ErrorClass::CertExpiry;
    }
    if v.get("domain").is_some() {
        return ErrorClass::DomainExpiry;
    }
    ErrorClass::Other
}

fn fmt_days_label(kind: &str, days: i64, name: &str) -> String {
    if days < 0 {
        format!("{kind} expired {} days ago · {name}", -days)
    } else if days == 0 {
        format!("{kind} expires today · {name}")
    } else {
        format!("{kind} expires in {days} days · {name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SERVED_STALE_PREFIX;

    #[test]
    fn expands_terse_transport_codes() {
        assert_eq!(
            humanize_check_error("no response"),
            "connected, but no response (timed out)"
        );
        assert_eq!(humanize_check_error("tls"), "TLS handshake failed");
        assert_eq!(
            humanize_check_error("connect timeout"),
            "couldn't connect (timed out)"
        );
    }

    #[test]
    fn expands_request_phase_codes() {
        assert_eq!(
            humanize_check_error("reset before response"),
            "server reset the connection before responding"
        );
        assert_eq!(
            humanize_check_error("closed before response"),
            "server closed the connection before responding"
        );
        assert_eq!(
            humanize_check_error("h2 stream refused"),
            "server refused the HTTP/2 stream"
        );
        assert_eq!(
            humanize_check_error("h2 HTTP_1_1_REQUIRED"),
            "HTTP/2 error: HTTP_1_1_REQUIRED"
        );
        assert_eq!(
            humanize_check_error("h2 rejected locally: PROTOCOL_ERROR"),
            "HTTP/2 exchange rejected by the probe (PROTOCOL_ERROR): malformed frames, or over its limits"
        );
        assert_eq!(
            humanize_check_error("response headers too large"),
            "response headers exceed the probe's limit"
        );
        assert_eq!(
            humanize_check_error("transport: operation was canceled"),
            "transport error: operation was canceled"
        );
    }

    #[test]
    fn de_machines_dns_reasons() {
        assert_eq!(
            humanize_check_error("dns: domain not found"),
            "domain not found (DNS)"
        );
        assert_eq!(
            humanize_check_error("dns: lookup timed out"),
            "DNS lookup timed out"
        );
    }

    #[test]
    fn formats_tls_cert_json() {
        let raw = r#"{"days_remaining":12,"subject_common_name":"example.com"}"#;
        assert_eq!(
            humanize_check_error(raw),
            "cert expires in 12 days · example.com"
        );
    }

    #[test]
    fn formats_domain_expiry_json() {
        let raw = r#"{"days_remaining":-3,"domain":"example.com"}"#;
        assert_eq!(
            humanize_check_error(raw),
            "domain expired 3 days ago · example.com"
        );
    }

    #[test]
    fn never_leaks_raw_json() {
        assert_eq!(humanize_check_error(r#"{"weird":"shape"}"#), "check failed");
    }

    #[test]
    fn passes_through_clean_reasons_and_unknown() {
        assert_eq!(
            humanize_check_error("connection refused"),
            "connection refused"
        );
        assert_eq!(
            humanize_check_error("certificate expired"),
            "certificate expired"
        );
    }

    #[test]
    fn served_stale_with_payload_decodes_and_hides_annotation() {
        let raw = r#"served_stale: last_verified_age_secs=3600; refresh_failed=whois_timeout; {"domain":"example.com","days_remaining":5}"#;
        let out = humanize_check_error(raw);
        assert_eq!(out, "domain expires in 5 days · example.com");
        assert!(!out.contains("served_stale"));
        assert!(!out.contains("refresh_failed"));
    }

    /// One sample per error shape the executors emit, kept by hand. It pins the
    /// shapes listed here against a reordered or broken match. Two things it
    /// cannot do: notice a new executor error, which lands in `Other` until
    /// someone adds it; or notice that a sample here is not a string any
    /// executor produces, which passes while the real path falls through.
    /// Copy samples from a real emitter, never from the match arm.
    const EMITTED: &[(&str, ErrorClass)] = &[
        ("body", ErrorClass::Body),
        ("decode", ErrorClass::Decode),
        (
            "heartbeat state unavailable on this node",
            ErrorClass::HeartbeatStateUnavailable,
        ),
        (
            "flow engine not configured on this node",
            ErrorClass::FlowNotConfigured,
        ),
        (
            "heartbeat monitors are evaluated on the control plane, not probed",
            ErrorClass::HeartbeatNotProbed,
        ),
        ("circuit_open", ErrorClass::CircuitOpen),
        ("timeout", ErrorClass::Timeout),
        ("connect timeout", ErrorClass::ConnectTimeout),
        ("connect", ErrorClass::Connect),
        ("connection refused", ErrorClass::ConnectionRefused),
        ("connection reset", ErrorClass::ConnectionReset),
        ("host unreachable", ErrorClass::HostUnreachable),
        ("network unreachable", ErrorClass::NetworkUnreachable),
        ("address not allowed", ErrorClass::AddressNotAllowed),
        ("certificate expired", ErrorClass::CertExpired),
        ("certificate not yet valid", ErrorClass::CertNotYetValid),
        ("certificate revoked", ErrorClass::CertRevoked),
        ("certificate not trusted", ErrorClass::CertNotTrusted),
        ("certificate self-signed", ErrorClass::CertNotTrusted),
        ("certificate chain incomplete", ErrorClass::CertChain),
        (
            "certificate hostname mismatch",
            ErrorClass::CertHostnameMismatch,
        ),
        ("certificate invalid", ErrorClass::CertInvalid),
        ("rdap timeout", ErrorClass::RdapLookup),
        ("rdap throttled", ErrorClass::RdapLookup),
        (
            "no echo reply from 2a01:116f:4013:3c00::1 within 3000ms",
            ErrorClass::PingFailed,
        ),
        ("no response", ErrorClass::NoResponse),
        ("tls", ErrorClass::Tls),
        ("tls handshake rejected", ErrorClass::Tls),
        ("tls version or cipher mismatch", ErrorClass::Tls),
        ("malformed tls response", ErrorClass::Tls),
        ("tls handshake reset", ErrorClass::Tls),
        ("tls handshake closed early", ErrorClass::Tls),
        ("transport", ErrorClass::Transport),
        ("reset before response", ErrorClass::Transport),
        ("closed before response", ErrorClass::Transport),
        ("invalid response", ErrorClass::Transport),
        ("h2 protocol error", ErrorClass::Transport),
        ("h2 internal error", ErrorClass::Transport),
        ("h2 stream refused", ErrorClass::Transport),
        ("h2 stream cancelled", ErrorClass::Transport),
        ("h2 enhance your calm", ErrorClass::Transport),
        ("h2 HTTP_1_1_REQUIRED", ErrorClass::Transport),
        ("h2 rejected locally: PROTOCOL_ERROR", ErrorClass::Transport),
        ("response headers too large", ErrorClass::Transport),
        ("transport: operation was canceled", ErrorClass::Transport),
        ("body timeout", ErrorClass::BodyTimeout),
        ("body match failed", ErrorClass::BodyMatchFailed),
        ("body over the 1 MiB read cap", ErrorClass::BodyOverCap),
        ("body over the 8 MiB decoded cap", ErrorClass::BodyOverCap),
        ("unexpected status 403", ErrorClass::UnexpectedStatus),
        (
            "rate-limited 429 (Retry-After: 30)",
            ErrorClass::RateLimited,
        ),
        ("rate-limited 503", ErrorClass::RateLimited),
        ("dns: domain not found", ErrorClass::DnsNoRecord),
        ("dns: no address records", ErrorClass::DnsNoRecord),
        // Interpolated by `worker::mod`, and by the connector's Display with a
        // literal `host`. Both must classify, so the sample carries a real host.
        (
            "no allowed addresses for 127.0.0.1",
            ErrorClass::AddressNotAllowed,
        ),
        (
            "no allowed addresses for host",
            ErrorClass::AddressNotAllowed,
        ),
        ("dns: lookup timed out", ErrorClass::DnsFailed),
        ("dns: lookup failed", ErrorClass::DnsFailed),
        (
            "server returned no certificate chain",
            ErrorClass::CertChain,
        ),
        ("empty certificate chain", ErrorClass::CertChain),
        (
            "parsing leaf certificate: unexpected tag",
            ErrorClass::CertChain,
        ),
        (
            "echo to 192.0.2.1 failed: permission denied",
            ErrorClass::PingFailed,
        ),
        (
            "connecting to WHOIS server whois.nic.example",
            ErrorClass::WhoisLookup,
        ),
        (
            "sending WHOIS query to whois.nic.example",
            ErrorClass::WhoisLookup,
        ),
        (
            "reading WHOIS response from whois.nic.example",
            ErrorClass::WhoisLookup,
        ),
        ("invalid RDAP base url 'not a url'", ErrorClass::RdapLookup),
        (
            "building RDAP request for 'example.com'",
            ErrorClass::RdapLookup,
        ),
        ("job reported failure", ErrorClass::HeartbeatJob),
        ("job reported failure (exit 137)", ErrorClass::HeartbeatJob),
        (
            "job started 900s ago and has not finished, past the 600s max runtime",
            ErrorClass::HeartbeatJob,
        ),
        (
            "no ping for 700s, expected every 600s (+60s grace)",
            ErrorClass::HeartbeatMissed,
        ),
        (
            "step 2/5 http_get: connection refused",
            ErrorClass::FlowStep,
        ),
        (
            r#"{"days_remaining":12,"subject_common_name":"example.com"}"#,
            ErrorClass::CertExpiry,
        ),
        (
            r#"{"days_remaining":-3,"domain":"example.com"}"#,
            ErrorClass::DomainExpiry,
        ),
    ];

    #[test]
    fn every_emitted_error_has_a_class() {
        for (raw, want) in EMITTED {
            assert_eq!(
                classify_check_error(raw),
                *want,
                "unclassified or misclassified: {raw}"
            );
        }
    }

    #[test]
    fn only_failures_to_run_the_check_at_all_are_internal() {
        let internal: Vec<&str> = EMITTED
            .iter()
            .filter(|(_, c)| c.family() == ErrorFamily::Internal)
            .map(|(raw, _)| *raw)
            .collect();
        assert_eq!(
            internal,
            vec![
                "heartbeat state unavailable on this node",
                "flow engine not configured on this node",
                "heartbeat monitors are evaluated on the control plane, not probed",
            ],
            "the alertable family must stay narrow; a class that persists \
             legitimately buries the alert in customer downtime"
        );
    }

    /// `body` is a connection dying mid-stream and `decode` is a page that
    /// inflates past the decoded cap on every check. Both are properties of
    /// the target, so both sit at full share on one monitor indefinitely.
    #[test]
    fn target_shaped_failures_never_reach_the_alertable_family() {
        for raw in [
            "body",
            "decode",
            "circuit_open",
            "body over the 1 MiB read cap",
        ] {
            assert_ne!(
                classify_check_error(raw).family(),
                ErrorFamily::Internal,
                "{raw} can persist honestly for hours"
            );
        }
    }

    #[test]
    fn free_text_is_other_not_a_guess() {
        assert_eq!(classify_check_error("something new"), ErrorClass::Other);
        assert_eq!(
            classify_check_error(r#"{"weird":"shape"}"#),
            ErrorClass::Other
        );
        assert_eq!(ErrorClass::Other.family(), ErrorFamily::Other);
    }

    #[test]
    fn served_stale_classifies_the_payload_it_wraps() {
        let raw = r#"served_stale: last_verified_age_secs=3600; {"domain":"example.com","days_remaining":5}"#;
        assert_eq!(classify_check_error(raw), ErrorClass::DomainExpiry);
    }

    #[test]
    fn class_names_are_unique() {
        let mut names: Vec<&str> = ErrorClass::ALL.iter().map(|c| c.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(before, names.len(), "two classes share a name");
    }

    #[test]
    fn all_lists_every_class_the_classifier_returns() {
        for (raw, _) in EMITTED {
            let class = classify_check_error(raw);
            assert!(
                ErrorClass::ALL.contains(&class),
                "{} is missing from ALL, so its gauge would never be zeroed",
                class.as_str()
            );
        }
        assert!(ErrorClass::ALL.contains(&ErrorClass::Other));
    }

    #[test]
    fn served_stale_without_renderable_is_generic() {
        assert_eq!(
            humanize_check_error(SERVED_STALE_PREFIX),
            "using last known result"
        );
        assert_eq!(
            humanize_check_error("served_stale: last_verified_age_secs=10"),
            "using last known result"
        );
    }
}
