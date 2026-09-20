//! SSL certificate checker: reads a public host's leaf certificate and reports
//! what it says.
//!
//! The only marketing surface that opens an outbound socket. Every other tool
//! either computes in the browser or renders a static page, so the guards that
//! make this safe live here rather than in a shared layer: a hostname parser
//! that refuses anything but a public DNS name, an SSRF filter over the
//! resolved addresses, a port allowlist, a per-IP rate limit and a hard
//! deadline. The certificate read itself is `security::cert_probe`, shared with
//! the `tls_cert` monitor so the two can never disagree about a chain.

use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use askama::Template;
use askama_web::WebTemplate;
use axum::Extension;
use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::marketing::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_faqpage, json_ld_web_application,
    json_ld_webpage,
};
use crate::security::cert_probe::{self, CertProbeError};
use crate::templates::filters;

use super::super::config::{BRAND, MarketingCfg};
use super::super::pages::{CachedRender, cached_render, serve_cached};
use super::probe::{self, Answer, ProbeError, ProbeLimiter};
use super::{TOOL_CACHE_CONTROL, TOOLS, ToolMeta};

pub const SSL_CHECKER_PATH: &str = "/tools/ssl-certificate-checker";
pub const SSL_PROBE_PATH: &str = "/tools/ssl-certificate-checker/probe";
const SSL_CHECKER_CREATED: &str = "2026-09-04";
pub const SSL_CHECKER_LASTMOD: &str = "2026-09-18";
pub const SSL_CHECKER_TITLE: &str = "SSL Certificate Checker: Expiry and Chain";
pub const SSL_CHECKER_LABEL: &str = "SSL certificate checker";
pub const SSL_CHECKER_DESCRIPTION: &str = "Read any public host's TLS certificate: days until expiry, who issued it, which names it covers and whether the chain is complete. Free, no sign-up.";

/// Ports that speak TLS immediately on connect. Without this list the endpoint
/// is a port scanner that reports which ports accept a connection.
const ALLOWED_PORTS: &[u16] = &[443, 465, 563, 636, 853, 989, 990, 993, 995, 8443, 9443];

/// Per-IP budget. Caddy already caps public pages per IP at the edge; this is
/// the app-side floor for deployments that do not front the site with it.
const PROBES_PER_MIN: u32 = 10;
const PROBE_BURST: u32 = 5;

/// A handshake that has not finished by now is telling the visitor what they
/// need to know: the host is not answering.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_DEADLINE: Duration = Duration::from_secs(9);

static LIMITER: LazyLock<ProbeLimiter> =
    LazyLock::new(|| probe::limiter(PROBES_PER_MIN, PROBE_BURST));

const SSL_CHECKER_FAQS: &[(&str, &str)] = &[
    (
        "How many days before expiry should I renew?",
        "Thirty days is the usual alarm and fourteen the usual panic. Let's Encrypt certificates last ninety days and renew at sixty, so a certificate sitting below thirty means the renewal has already failed at least once and nobody read the mail about it.",
    ),
    (
        "The certificate is valid but my browser still complains. Why?",
        "Almost always an incomplete chain. The server is sending its own certificate without the intermediate that links it to a trusted root. Desktop browsers often paper over it from cache, while a fresh client, a mobile app or a server-to-server call fails outright. The chain length reported here is what the server actually sent.",
    ),
    (
        "What does the hostname match mean?",
        "A certificate is issued for a set of names. If the name you asked for is not among them, every client rejects it no matter how valid the dates are. This is the usual failure after a domain is added to a load balancer but not to the certificate.",
    ),
    (
        "Do you check the whole chain against a trust store?",
        "No, and deliberately. Validation is skipped so an expired or self-signed certificate can still be read and reported instead of the handshake refusing it and losing the dates. That is the case you most need to see.",
    ),
    (
        "Can I be told before it expires instead?",
        "Yes. A TLS certificate check reads the same certificate on a schedule and alerts at the day count you choose, which is the point: an expiry is the one outage you can know about weeks ahead. The button beside the result opens a monitor prefilled with the host you just checked.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_ssl_checker.html")]
struct SslCheckerPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    probe_path: &'static str,
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

static SSL_CHECKER_CACHED: OnceLock<CachedRender> = OnceLock::new();

pub(super) fn render(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, SSL_CHECKER_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{SSL_CHECKER_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = SSL_CHECKER_DESCRIPTION.to_string();
    let page = SslCheckerPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            SSL_CHECKER_TITLE,
            SSL_CHECKER_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            SSL_CHECKER_TITLE,
            SSL_CHECKER_PATH,
            SSL_CHECKER_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            SSL_CHECKER_PATH,
            SSL_CHECKER_TITLE,
            SSL_CHECKER_CREATED,
            SSL_CHECKER_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(SSL_CHECKER_FAQS),
        faqs: SSL_CHECKER_FAQS,
        probe_path: SSL_PROBE_PATH,
        canonical_url,
        og,
        tools: TOOLS,
        self_path: SSL_CHECKER_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- ssl-checker render failed: {e} -->"));
    cached_render(body)
}

pub(super) fn warm(cfg: &MarketingCfg) {
    SSL_CHECKER_CACHED.get_or_init(|| render(cfg));
}

pub(super) async fn page(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = SSL_CHECKER_CACHED.get_or_init(|| render(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

#[derive(Debug, Deserialize)]
pub(super) struct ProbeQuery {
    host: Option<String>,
    port: Option<u16>,
}

#[derive(Debug, Serialize)]
struct ProbeReport {
    host: String,
    port: u16,
    resolved_ip: String,
    subject_common_name: Option<String>,
    issuer_common_name: Option<String>,
    issuer_organization: Option<String>,
    not_before: String,
    not_after: String,
    days_remaining: i64,
    /// Sent alongside the day count because that count truncates toward zero:
    /// a certificate six hours dead reports as 0 days, which reads as "expires
    /// today" for something already breaking every client.
    expired: bool,
    san_dns_names: Vec<String>,
    serial: String,
    name_matches: bool,
    self_signed: bool,
    chain_len: usize,
    handshake_ms: u32,
}

pub(super) async fn probe(
    State(cfg): State<Arc<MarketingCfg>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    q: Result<Query<ProbeQuery>, QueryRejection>,
) -> Response {
    // The page's own script reads every answer as JSON, so a malformed query
    // string must not fall through to axum's plain-text rejection: the parse
    // would throw and the visitor would be told the server was unreachable.
    let Ok(Query(q)) = q else {
        return probe::fail(
            StatusCode::BAD_REQUEST,
            "That request could not be read. Check the host and port.",
        );
    };
    let Some(host) = q.host.as_deref().and_then(probe::clean_host) else {
        return probe::fail(
            StatusCode::BAD_REQUEST,
            "That does not look like a domain name. Enter a hostname such as example.com.",
        );
    };
    let port = q.port.unwrap_or(443);
    if !ALLOWED_PORTS.contains(&port) {
        return probe::fail(
            StatusCode::BAD_REQUEST,
            "That port is not one this checker will open. TLS-on-connect ports only.",
        );
    }

    // Metered after parsing, because the budget exists to bound outbound
    // sockets: a typo costs nobody a connection and should not spend a turn.
    if !probe::spend_budget(&cfg, &headers, peer, &LIMITER, 1) {
        return probe::fail(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many checks from this address. Wait a minute and try again.",
        );
    }

    match tokio::time::timeout(PROBE_DEADLINE, run(&host, port)).await {
        Ok(Ok(report)) => (
            probe::probe_headers(),
            Json(Answer {
                ok: true,
                body: report,
            }),
        )
            .into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(_) => ProbeError::Host("The host did not finish a handshake in time.").into_response(),
    }
}

async fn run(host: &str, port: u16) -> Result<ProbeReport, ProbeError> {
    let addrs = probe::resolve(host).await?;
    let facts = cert_probe::connect_and_read(&addrs, port, host, CONNECT_TIMEOUT)
        .await
        .map_err(describe)?;

    Ok(ProbeReport {
        host: host.to_owned(),
        port,
        resolved_ip: facts.peer.to_string(),
        subject_common_name: facts.subject_common_name,
        issuer_common_name: facts.issuer_common_name,
        issuer_organization: facts.issuer_organization,
        not_before: facts.not_before.to_rfc3339(),
        not_after: facts.not_after.to_rfc3339(),
        days_remaining: facts.days_remaining,
        expired: facts.not_after <= chrono::Utc::now(),
        san_dns_names: facts.san_dns_names,
        serial: facts.serial,
        name_matches: facts.name_matches,
        self_signed: facts.self_signed,
        chain_len: facts.chain_len,
        handshake_ms: facts.handshake_ms,
    })
}

/// Probe errors carry a platform errno and a rustls internal, neither of which
/// is meaningful to a visitor. Each becomes a sentence about the host.
fn describe(err: CertProbeError) -> ProbeError {
    let text = match err {
        CertProbeError::Connect(ref e) if probe::egress_is_broken(e) => {
            return ProbeError::Server(
                StatusCode::SERVICE_UNAVAILABLE,
                "The checker could not open an outbound connection.",
            );
        }
        CertProbeError::Connect(_) => "Nothing accepted a connection on that port.",
        CertProbeError::Handshake(_) => {
            "The host accepted the connection but did not complete a TLS handshake. It may not speak TLS on that port."
        }
        CertProbeError::NoChain => "The host completed a handshake without sending a certificate.",
        CertProbeError::Parse(_) => "The host sent a certificate that could not be parsed.",
        CertProbeError::Validity => "The certificate carries dates outside any usable range.",
        CertProbeError::ServerName(_) => "That name cannot be used in a TLS handshake.",
        CertProbeError::Timeout => "The host did not finish a handshake in time.",
    };
    ProbeError::Host(text)
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    /// Guards the error budget: none of these may drift back into the 5xx class.
    #[tokio::test]
    async fn a_host_verdict_answers_200_with_ok_false() {
        for err in [
            describe(CertProbeError::NoChain),
            describe(CertProbeError::Validity),
            describe(CertProbeError::Timeout),
            describe(CertProbeError::Parse(String::new())),
            describe(CertProbeError::ServerName(String::new())),
            describe(CertProbeError::Connect(io::Error::from(
                io::ErrorKind::ConnectionRefused,
            ))),
            describe(CertProbeError::Handshake(io::Error::from(
                io::ErrorKind::UnexpectedEof,
            ))),
            ProbeError::Host("That name does not resolve."),
        ] {
            let res = err.into_response();
            assert_eq!(res.status(), StatusCode::OK);
            let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["ok"], false);
            assert!(json["error"].is_string(), "the page renders this sentence");
        }
    }

    #[test]
    fn port_allowlist_excludes_plaintext_services() {
        for port in [22, 80, 3306, 5432, 6379, 11211] {
            assert!(
                !ALLOWED_PORTS.contains(&port),
                "port {port} must not be probeable"
            );
        }
        assert!(ALLOWED_PORTS.contains(&443));
    }
}
