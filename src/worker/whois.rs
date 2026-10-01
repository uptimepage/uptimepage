//! WHOIS (port 43) registration lookup, for TLDs the RDAP bootstrap misses.
//!
//! Server names come from a static table, never from the queried name. The
//! resolved addresses still go through the shared SSRF guard: the table fixes
//! which host we look up, not which IP the resolver hands back.

use std::time::Duration;

use anyhow::{Context, anyhow};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::domain::publishes_no_expiry;
use crate::error::{AppError, Result};
use crate::http_client::HttpClients;
use crate::worker::connect_via_guard;
use crate::worker::registration::{RegistrationAnswer, RegistrationError};

const WHOIS_PORT: u16 = 43;
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Caps a hostile or broken server that would otherwise stream until timeout.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Add a TLD only after confirming its live WHOIS carries a parseable expiry,
/// and that `parse_timestamp` handles its date format.
const WHOIS_SERVERS: &[(&str, &str)] = &[
    ("co", "whois.registry.co"),
    ("dk", "whois.punktum.dk"),
    ("ee", "whois.tld.ee"),
    ("hk", "whois.hkirc.hk"),
    ("hr", "whois.dns.hr"),
    ("ie", "whois.weare.ie"),
    ("is", "whois.isnic.is"),
    ("it", "whois.nic.it"),
    ("la", "whois.nic.la"),
    ("lt", "whois.domreg.lt"),
    ("me", "whois.nic.me"),
    ("mx", "whois.mx"),
    ("nu", "whois.iis.nu"),
    ("pt", "whois.dns.pt"),
    ("se", "whois.iis.se"),
    ("sh", "whois.nic.sh"),
    ("si", "whois.register.si"),
    ("sk", "whois.sk-nic.sk"),
    ("so", "whois.nic.so"),
    ("st", "whois.nic.st"),
    ("us", "whois.nic.us"),
];

/// Ordered by preference: the registry's value beats the registrar's copy.
const EXPIRY_LABELS: &[&str] = &[
    "registry expiry date",
    "expiry date",
    "expiration date",
    "expire date",
    "registrar registration expiration date",
    "expires",
    "expire",
    "valid until",
];

pub fn whois_server(tld: &str) -> Option<&'static str> {
    WHOIS_SERVERS
        .iter()
        .find(|(t, _)| *t == tld)
        .map(|(_, server)| *server)
}

#[derive(Default)]
pub struct WhoisClient {
    /// Test-only: forces every lookup at a fixed address instead of the static
    /// table's server on port 43, so a mock server can stand in.
    addr_override: Option<(String, u16)>,
}

impl WhoisClient {
    pub fn new() -> Self {
        Self::default()
    }

    #[doc(hidden)]
    pub fn with_addr_override(host: impl Into<String>, port: u16) -> Self {
        Self {
            addr_override: Some((host.into(), port)),
        }
    }

    pub async fn lookup_expiration(
        &self,
        registrable: &str,
        tld: &str,
        clients: &HttpClients,
    ) -> Result<RegistrationAnswer> {
        if publishes_no_expiry(tld) {
            return Err(RegistrationError::NoPublicExpiry {
                tld: tld.to_owned(),
            }
            .into());
        }
        let (host, port) = match &self.addr_override {
            Some((h, p)) => (h.as_str(), *p),
            None => (
                whois_server(tld).ok_or_else(|| {
                    AppError::from(RegistrationError::TldUnsupported {
                        tld: tld.to_owned(),
                    })
                })?,
                WHOIS_PORT,
            ),
        };

        let body = query(host, port, registrable, clients).await?;
        parse_answer(&body)
    }
}

async fn query(
    server: &str,
    port: u16,
    registrable: &str,
    clients: &HttpClients,
) -> Result<String> {
    let mut stream: TcpStream = connect_via_guard(server, port, clients)
        .await
        .with_context(|| format!("connecting to WHOIS server {server}"))?;

    let request = format!("{registrable}\r\n");
    timeout(READ_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| AppError::Other(anyhow!("WHOIS write to {server} timed out")))?
        .with_context(|| format!("sending WHOIS query to {server}"))?;

    let mut buf = Vec::with_capacity(8 * 1024);
    let read = timeout(
        READ_TIMEOUT,
        stream.take(MAX_RESPONSE_BYTES as u64).read_to_end(&mut buf),
    )
    .await
    .map_err(|_| AppError::Other(anyhow!("WHOIS read from {server} timed out")))?;
    read.with_context(|| format!("reading WHOIS response from {server}"))?;

    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Takes the highest-priority label that is PRESENT, then parses it. Falling
/// through on a parse failure would silently answer with the registrar's copy
/// while the registry's own value sat there unread.
fn parse_answer(body: &str) -> Result<RegistrationAnswer> {
    let (label, raw) = EXPIRY_LABELS
        .iter()
        .find_map(|label| field(body, label).map(|v| (*label, v)))
        .ok_or_else(|| AppError::Other(anyhow!("no expiration date in WHOIS response")))?;

    let expiration = parse_timestamp(&raw)
        .ok_or_else(|| AppError::Other(anyhow!("unparseable WHOIS '{label}' value: {raw}")))?;

    Ok(RegistrationAnswer {
        expiration,
        registrar: field(body, "registrar"),
    })
}

/// Matches the label alone, so registry-specific casing and padding do not
/// matter.
fn field(body: &str, label: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case(label) {
            return None;
        }
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim().trim_end_matches('.');
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    // Collapse runs of whitespace so month-name forms like "September  5 2027"
    // match a single-space format.
    let squeezed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");

    // Registries that omit the zone designator publish UTC. Year-first formats
    // come first: they are unambiguous, so a day-first pattern never reaches an
    // ISO value. The day-first `-`/`/` forms assume day-before-month, which
    // holds for the European ccTLDs that use them; a value whose leading field
    // exceeds 12 fails loudly rather than parsing as the wrong month.
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%d/%m/%Y %H:%M:%S",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&squeezed, format) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    for format in ["%Y-%m-%d", "%d-%m-%Y", "%d/%m/%Y", "%B %d %Y"] {
        if let Ok(date) = NaiveDate::parse_from_str(&squeezed, format) {
            return date.and_hms_opt(0, 0, 0).map(|n| Utc.from_utc_datetime(&n));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_expiry_date_and_registrar() {
        let body = "Domain Name: POSTYOURSTARTUP.CO\n\
                    Registry Expiry Date: 2027-07-22T23:59:59.0Z\n\
                    Registrar: NameCheap, Inc.\n";
        let answer = parse_answer(body).expect("parses");
        assert_eq!(answer.expiration.to_rfc3339(), "2027-07-22T23:59:59+00:00");
        assert_eq!(answer.registrar.as_deref(), Some("NameCheap, Inc."));
    }

    #[test]
    fn registry_value_wins_over_registrar_copy() {
        let body = "Registrar Registration Expiration Date: 2026-07-22T19:30:52.57Z\n\
                    Registry Expiry Date: 2027-07-22T23:59:59.0Z\n";
        let answer = parse_answer(body).expect("parses");
        assert_eq!(answer.expiration.to_rfc3339(), "2027-07-22T23:59:59+00:00");
    }

    #[test]
    fn parses_zoneless_and_date_only_forms() {
        assert!(parse_answer("expires: 2027-01-02 03:04:05\n").is_ok());
        assert!(parse_answer("expire: 2027-01-02\n").is_ok());
    }

    #[test]
    fn day_first_value_reads_day_before_month() {
        // 16-11-2035 is 16 Nov, not an invalid 16th month.
        let ts = parse_timestamp("16-11-2035").expect("day-first parses");
        assert_eq!(ts.to_rfc3339(), "2035-11-16T00:00:00+00:00");
        let ts = parse_timestamp("31/12/2026 23:59:00").expect("day-first datetime parses");
        assert_eq!(ts.to_rfc3339(), "2026-12-31T23:59:00+00:00");
    }

    #[test]
    fn month_name_value_with_padding_parses() {
        let ts = parse_timestamp("September  5 2027").expect("month name parses");
        assert_eq!(ts.to_rfc3339(), "2027-09-05T00:00:00+00:00");
    }

    #[test]
    fn month_first_value_with_day_over_twelve_fails_loudly() {
        // A US-style 08-22-2026 must not be misread as day-first: month 22 is
        // invalid, so it returns None rather than a wrong date.
        assert!(parse_timestamp("08-22-2026").is_none());
    }

    /// An unparseable authoritative value must fail loudly. Falling through to
    /// the registrar's copy would report a different date as if it were the
    /// registry's.
    #[test]
    fn unparseable_top_priority_value_does_not_fall_through() {
        let body = "Registry Expiry Date: 22-Jul-2027\n\
                    Registrar Registration Expiration Date: 2026-07-22T19:30:52.57Z\n";
        let err = parse_answer(body).expect_err("must not silently use the registrar copy");
        assert!(err.to_string().contains("22-Jul-2027"), "got: {err}");
    }

    /// Verbatim lines from each live registry: upstream label changes fail
    /// here rather than in production.
    #[test]
    fn parses_every_supported_registry_format() {
        let samples = [
            ("co", "Registry Expiry Date: 2027-07-22T23:59:59.0Z"),
            ("sh", "Registry Expiry Date: 2027-05-01T00:00:02Z"),
            ("so", "Registry Expiry Date: 2031-10-31T00:00:00Z"),
            ("la", "Registry Expiry Date: 2026-11-20T23:59:59.0Z"),
            ("it", "Expire Date:        2026-12-31"),
            ("st", "Expiration Date: 2035-06-19"),
            ("nu", "expires:          2032-01-17"),
            ("me", "Registry Expiry Date: 2034-04-29T17:53:02Z"),
            ("us", "Registry Expiry Date: 2027-04-17T23:59:59Z"),
            ("se", "expires:                       2031-03-09"),
            ("dk", "Expires: 2027-01-31"),
            ("ie", "Registry Expiry Date: 2027-01-01T14:45:44Z"),
            (
                "hr",
                "Registrar Registration Expiration Date: 2050-11-15T23:00:00Z",
            ),
            ("si", "expire:  2026-10-28"),
            ("lt", "Expires:    2026-10-17"),
            ("ee", "expire:      2030-02-05"),
            ("mx", "Expiration Date: 2027-01-14"),
            ("pt", "Expiration Date: 31/12/2026 23:59:00"),
            ("hk", "Expiry Date: 16-11-2035"),
            ("is", "expires:      September  5 2027"),
            ("sk", "Valid Until: 2027-06-10"),
        ];
        for (tld, line) in samples {
            assert!(
                parse_answer(&format!("{line}\n")).is_ok(),
                ".{tld} format no longer parses: {line}"
            );
        }
    }

    #[test]
    fn missing_expiry_is_an_error() {
        assert!(parse_answer("Domain Name: DENIC.DE\nStatus: connect\n").is_err());
    }

    #[test]
    fn label_match_is_exact_not_substring() {
        let body = "Registrar URL: https://example.test\nRegistrar: Real Registrar\n";
        assert_eq!(field(body, "registrar").as_deref(), Some("Real Registrar"));
    }

    /// Hits live third-party registries; run when changing the table or parser
    /// with `WHOIS_LIVE_TEST=1`. CI runs ignored tests, so it no-ops without the
    /// env var rather than depending on 21 registries answering from the runner.
    #[tokio::test]
    #[ignore = "set WHOIS_LIVE_TEST=1: reaches live WHOIS registries"]
    async fn live_lookup_returns_expiry_for_every_supported_tld() {
        if std::env::var_os("WHOIS_LIVE_TEST").is_none() {
            return;
        }
        let clients = crate::http_client::build_clients(
            &crate::config::HttpClientConfig {
                tcp_keepalive_secs: 30,
                user_agent: "Uptimepage/test".into(),
            },
            &crate::config::CheckerConfig {
                max_concurrent_checks: 100,
                default_timeout_ms: 5_000,
                connect_timeout_ms: 2_000,
                default_check_interval_secs: 60,
                per_host_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
                rdap_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
            },
            &crate::config::DnsConfig {
                cache_size: 64,
                positive_ttl_secs: 60,
                negative_ttl_secs: 10,
                servers: vec!["1.1.1.1:53".into()],
            },
            // Strict guard on purpose: the live registries must clear the same
            // filter production applies.
            &crate::config::SecurityConfig {
                allow_private_targets: false,
                credentials_kek_base64: secrecy::SecretString::from(String::new()),
                trusted_proxies: vec![],
            },
        )
        .expect("clients build");
        let client = WhoisClient::new();
        let floor = Utc::now() - chrono::Duration::days(365 * 5);
        for (domain, tld) in [
            ("postyourstartup.co", "co"),
            ("nic.sh", "sh"),
            ("nic.so", "so"),
            ("nic.la", "la"),
            ("nic.it", "it"),
            ("nic.st", "st"),
            ("iis.nu", "nu"),
            ("about.me", "me"),
            ("about.us", "us"),
            ("iis.se", "se"),
            ("punktum.dk", "dk"),
            ("weare.ie", "ie"),
            ("dns.hr", "hr"),
            ("register.si", "si"),
            ("domreg.lt", "lt"),
            ("internet.ee", "ee"),
            ("nic.mx", "mx"),
            ("dns.pt", "pt"),
            ("hkirc.hk", "hk"),
            ("isnic.is", "is"),
            ("sk-nic.sk", "sk"),
        ] {
            let answer = client
                .lookup_expiration(domain, tld, &clients)
                .await
                .unwrap_or_else(|e| panic!("live .{tld} lookup failed: {e}"));
            assert!(answer.expiration > floor, ".{tld} expiry is implausible");
        }
    }

    #[test]
    fn server_table_covers_known_gaps_and_excludes_expiryless_registries() {
        assert_eq!(whois_server("co"), Some("whois.registry.co"));
        assert_eq!(whois_server("nu"), Some("whois.iis.nu"));
        for tld in ["de", "eu", "gg", "at", "ch", "jp"] {
            assert!(whois_server(tld).is_none(), ".{tld} must have no server");
            assert!(publishes_no_expiry(tld), ".{tld} publishes no expiry");
        }
        assert!(!publishes_no_expiry("co"));
        assert!(!publishes_no_expiry("se"));
    }
}
