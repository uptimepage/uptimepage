//! Checks worth adding beside a monitor, derived from its host. Pure: the
//! monitor's own check plus the org's existing coverage, nothing else.

use std::net::IpAddr;

use crate::domain::{CheckSpec, registered_domain};

/// Also the filter for the org-coverage lookup, so the query never drags back
/// monitors that could not match.
pub const COVERAGE_KINDS: [&str; 3] = ["tls_cert", "domain_expiry", "dns"];

pub struct CoverageSuggestion {
    pub kind: &'static str,
    /// Card name from the check-type chooser, so the suggestion and the card
    /// it lands on read alike.
    pub label: &'static str,
    /// Spoken form: the card names alone are not a sentence.
    pub what: &'static str,
    pub why: &'static str,
    pub host: String,
    pub href: String,
}

pub type CoveredHosts = [(String, String)];

pub struct CoveragePanel {
    /// Doubles as the dismissal key: saying no once covers every monitor
    /// pointed at the same name.
    pub host: String,
    pub items: Vec<CoverageSuggestion>,
}

pub fn panel(check: &CheckSpec, covered: &CoveredHosts) -> Option<CoveragePanel> {
    let host = check
        .primary_host()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    // A literal address has no certificate, no registration, and nothing to resolve.
    if host.is_empty() || host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let items = suggestions(check, &host, covered);
    (!items.is_empty()).then_some(CoveragePanel { host, items })
}

fn suggestions(check: &CheckSpec, host: &str, covered: &CoveredHosts) -> Vec<CoverageSuggestion> {
    let host = host.to_owned();
    let already = |kind: &str, h: &str| {
        covered
            .iter()
            .any(|(k, c)| k == kind && c.trim_end_matches('.').eq_ignore_ascii_case(h))
    };

    let mut out = Vec::new();
    let mut offer = |kind: &'static str,
                     label: &'static str,
                     what: &'static str,
                     why: &'static str,
                     h: String| {
        if check.kind() != kind && !already(kind, &h) {
            let encoded: String = url::form_urlencoded::byte_serialize(h.as_bytes()).collect();
            out.push(CoverageSuggestion {
                kind,
                label,
                what,
                why,
                href: format!("/targets/new?kind={kind}&host={encoded}"),
                host: h,
            });
        }
    };

    if serves_tls(check) {
        offer(
            "tls_cert",
            "tls cert",
            "a TLS certificate check",
            "HTTPS keeps answering right up to the day the certificate expires.",
            host.clone(),
        );
    }
    let apex = registered_domain(&host);
    // A registry publishing no expiry would only fail at probe time.
    if apex
        .rsplit('.')
        .next()
        .is_some_and(crate::domain::is_monitorable)
    {
        offer(
            "domain_expiry",
            "domain",
            "a domain expiry check",
            "One missed renewal takes down every host under the name at once.",
            apex,
        );
    }
    offer(
        "dns",
        "dns",
        "a DNS record check",
        "Resolution can break while the service itself is answering fine.",
        host,
    );
    out
}

fn serves_tls(check: &CheckSpec) -> bool {
    match check {
        CheckSpec::Http(h) => h.url.scheme() == "https",
        CheckSpec::Flow(f) => f.start_url.scheme() == "https",
        CheckSpec::TlsCert(_) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use crate::domain::{ExpectedStatus, HttpCheck, HttpMethod};

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

    fn kinds(check: &CheckSpec, covered: &CoveredHosts) -> Vec<&'static str> {
        panel(check, covered)
            .map(|p| p.items.iter().map(|c| c.kind).collect())
            .unwrap_or_default()
    }

    #[test]
    fn https_monitor_offers_cert_registration_and_resolution() {
        let p = panel(&http("https://app.acme.com/healthz"), &[]).unwrap();
        assert_eq!(
            p.items.iter().map(|c| c.kind).collect::<Vec<_>>(),
            ["tls_cert", "domain_expiry", "dns"]
        );
        // Registration is owned by the registrable domain, not the subdomain.
        assert_eq!(p.items[0].host, "app.acme.com");
        assert_eq!(p.items[1].host, "acme.com");
        assert_eq!(p.items[2].host, "app.acme.com");
        // The dismissal key is the monitor's host, not any one suggestion's.
        assert_eq!(p.host, "app.acme.com");
    }

    #[test]
    fn plain_http_has_no_certificate_to_watch() {
        assert_eq!(
            kinds(&http("http://acme.com/"), &[]),
            ["domain_expiry", "dns"]
        );
    }

    #[test]
    fn existing_coverage_drops_out() {
        let covered = [
            ("tls_cert".to_string(), "app.acme.com".to_string()),
            ("domain_expiry".to_string(), "ACME.com.".to_string()),
        ];
        assert_eq!(
            kinds(&http("https://app.acme.com/"), &covered),
            ["dns"],
            "case and trailing dot must not hide existing coverage"
        );
    }

    #[test]
    fn a_fully_covered_host_shows_no_panel() {
        let covered = [
            ("tls_cert".to_string(), "acme.com".to_string()),
            ("domain_expiry".to_string(), "acme.com".to_string()),
            ("dns".to_string(), "acme.com".to_string()),
        ];
        assert!(panel(&http("https://acme.com/"), &covered).is_none());
    }

    #[test]
    fn a_monitor_never_suggests_its_own_kind() {
        let dns: CheckSpec = serde_json::from_str(
            r#"{"type":"dns","domain":"acme.com","record_type":"A","timeout":3000}"#,
        )
        .unwrap();
        assert_eq!(kinds(&dns, &[]), ["domain_expiry"]);
    }

    #[test]
    fn bare_addresses_and_heartbeats_yield_nothing() {
        assert!(panel(&http("https://192.0.2.10/"), &[]).is_none());
        assert!(panel(&http("https://[2606:4700::1111]/"), &[]).is_none());
        let hb: CheckSpec =
            serde_json::from_str(r#"{"type":"heartbeat","period":300000,"grace":60000}"#).unwrap();
        assert!(panel(&hb, &[]).is_none());
    }

    #[test]
    fn expiryless_registries_are_not_offered() {
        // DENIC publishes no expiry, so the check would only fail at probe time.
        assert_eq!(
            kinds(&http("https://example.de/"), &[]),
            ["tls_cert", "dns"]
        );
        // A registry that does publish one still gets offered.
        assert!(kinds(&http("https://example.com/"), &[]).contains(&"domain_expiry"));
    }

    #[test]
    fn href_carries_the_prefill_and_escapes_it() {
        let p = panel(&http("https://acme.com/"), &[]).unwrap();
        let dns = p.items.iter().find(|c| c.kind == "dns").unwrap();
        assert_eq!(dns.href, "/targets/new?kind=dns&host=acme.com");
    }
}
