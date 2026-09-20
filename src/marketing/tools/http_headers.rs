//! HTTP header and redirect chain checker: walks a URL the way a monitor does
//! and reports every hop it takes to get an answer.
//!
//! Wider than the certificate checker in one way that matters: a single request
//! can touch several hosts, because following the chain is the point. Every hop
//! is re-parsed, re-filtered through the SSRF guard at connect time and charged
//! to the visitor's budget, the chain is capped and cycle-checked, and the
//! response body is never read — only the head. Reading it would make this an
//! open proxy for whatever a stranger points it at.

use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

use askama::Template;
use askama_web::WebTemplate;
use axum::Extension;
use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::Full;
use hyper::Request;
use hyper::body::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use url::Url;

use crate::http_outbound::{OutboundHttpClient, build_outbound_client};
use crate::marketing::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_faqpage, json_ld_web_application,
    json_ld_webpage,
};
use crate::security::SsrfGuard;
use crate::templates::filters;

use super::super::config::{BRAND, MarketingCfg};
use super::super::pages::{CachedRender, cached_render, serve_cached};
use super::probe::{self, Answer, ProbeError, ProbeLimiter};
use super::{TOOL_CACHE_CONTROL, TOOLS, ToolMeta};

pub const HEADER_CHECKER_PATH: &str = "/tools/http-header-checker";
pub const HEADER_PROBE_PATH: &str = "/tools/http-header-checker/probe";
const HEADER_CHECKER_CREATED: &str = "2026-09-05";
pub const HEADER_CHECKER_LASTMOD: &str = "2026-09-18";
pub const HEADER_CHECKER_TITLE: &str = "HTTP Header & Redirect Chain Checker";
pub const HEADER_CHECKER_LABEL: &str = "HTTP header checker";
pub const HEADER_CHECKER_DESCRIPTION: &str = "See every redirect hop, the final status code and the response headers a URL returns, the way an uptime monitor sees them. Free, no sign-up.";

/// Web ports only. Without this the endpoint reports which ports on a public
/// host accept a connection, which is a scanner with a nicer front end.
const ALLOWED_PORTS: &[u16] = &[80, 443, 8080, 8443];

/// Hop ceiling. Matches the monitor's own cap so the chain shown here is the
/// chain a check would walk; a site that needs more is broken either way.
const MAX_HOPS: usize = 10;

/// The per-minute rate is tighter than the certificate checker's because one
/// probe here can cost up to [`MAX_HOPS`] outbound requests. Hops past the
/// first are charged back after the walk, so a long chain spends what it
/// actually used. A security-checker run spends one cell per probe plus one
/// per redirect hop: three for a site with no redirects, five with an
/// apex-to-www hop. The burst covers two plain runs and stays below
/// [`MAX_CONCURRENT_PROBES`], so one address can never hold every permit.
const PROBES_PER_MIN: u32 = 6;
const PROBE_BURST: u32 = 7;

/// Ceiling on outbound requests in flight across every visitor at once.
/// Per-IP budgets bound one stranger; this bounds all of them together.
const MAX_CONCURRENT_PROBES: usize = 8;

const HOP_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_DEADLINE: Duration = Duration::from_secs(12);

/// A hostile server can answer with as many headers as it likes. The page
/// shows a response, not a payload, so both the count and each value are cut.
/// The value cap leaves room for a real Content-Security-Policy, which runs
/// to a few kilobytes on the sites that bother with one.
const MAX_HEADERS_REPORTED: usize = 60;
const MAX_HEADER_VALUE_CHARS: usize = 4096;

static LIMITER: LazyLock<ProbeLimiter> =
    LazyLock::new(|| probe::limiter(PROBES_PER_MIN, PROBE_BURST));

static IN_FLIGHT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_PROBES));

/// Validates certificates, unlike the certificate checker next door: a monitor
/// that cannot complete a handshake records a failure, and this page exists to
/// show what the monitor sees. The FAQ sends the expired-certificate case to
/// the tool built to read one anyway.
static CLIENT: LazyLock<OutboundHttpClient> =
    LazyLock::new(|| build_outbound_client(SsrfGuard::strict()));

const HEADER_CHECKER_FAQS: &[(&str, &str)] = &[
    (
        "What causes ERR_TOO_MANY_REDIRECTS?",
        "Two rules that each think the other should have finished the job. The classic pair is a CDN forcing HTTPS while the origin redirects HTTPS back to HTTP, so the chain bounces between the two forever. The hop list above shows the cycle: look for the same URL appearing twice.",
    ),
    (
        "Which redirect codes keep the request method?",
        "307 and 308 preserve it, so a POST stays a POST. 301 and 302 do not guarantee that, and in practice almost every client turns them into a GET. 303 always becomes a GET. If a form submission breaks only after a redirect was added, this is usually why.",
    ),
    (
        "Why does my browser show something different?",
        "A browser answers from its own cache, sends your cookies, and may have been handed a different edge server than we were. The first request of a session is the honest one, and that is what this runs: no cookies, no cache, a fresh connection each time.",
    ),
    (
        "The check fails here but the site loads for me. Why?",
        "Usually the certificate. This checker validates the chain the way a monitor does, so an expired certificate or a missing intermediate fails outright, while your browser may paper over it from cache. The SSL certificate checker reads the certificate without validating it, which is the tool for that case.",
    ),
    (
        "Should an uptime monitor follow redirects?",
        "Follow them when you are watching whether people can reach the site, because that is the journey they take. Turn following off when you are watching a specific endpoint and a redirect would mean something has changed: an API route that starts answering 302 is an incident, not a success.",
    ),
    (
        "Which response headers matter for uptime?",
        "The status code first, then anything that explains a bad one: Retry-After on a 429 or 503, cache and edge headers that say whether a CDN answered instead of your origin, and Location on a redirect. Security headers such as HSTS do not affect uptime, but the chain that sets them often does.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_header_checker.html")]
struct HeaderCheckerPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    probe_path: &'static str,
    max_hops: usize,
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

static HEADER_CHECKER_CACHED: OnceLock<CachedRender> = OnceLock::new();

pub(super) fn render(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, HEADER_CHECKER_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{HEADER_CHECKER_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = HEADER_CHECKER_DESCRIPTION.to_string();
    let page = HeaderCheckerPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            HEADER_CHECKER_TITLE,
            HEADER_CHECKER_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            HEADER_CHECKER_TITLE,
            HEADER_CHECKER_PATH,
            HEADER_CHECKER_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            HEADER_CHECKER_PATH,
            HEADER_CHECKER_TITLE,
            HEADER_CHECKER_CREATED,
            HEADER_CHECKER_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(HEADER_CHECKER_FAQS),
        faqs: HEADER_CHECKER_FAQS,
        probe_path: HEADER_PROBE_PATH,
        max_hops: MAX_HOPS,
        tools: TOOLS,
        self_path: HEADER_CHECKER_PATH,
        canonical_url,
        og,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- header-checker render failed: {e} -->"));
    cached_render(body)
}

pub(super) fn warm(cfg: &MarketingCfg) {
    HEADER_CHECKER_CACHED.get_or_init(|| render(cfg));
}

pub(super) async fn page(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = HEADER_CHECKER_CACHED.get_or_init(|| render(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

#[derive(Debug, Deserialize)]
pub(super) struct ProbeQuery {
    url: Option<String>,
}

#[derive(Debug, Serialize)]
struct Hop {
    url: String,
    status: u16,
    /// The raw `Location` the server sent, before it is resolved against the
    /// current URL. A relative one is the usual cause of a chain that works in
    /// a browser and not in a client that joins it differently.
    location: Option<String>,
    ms: u32,
}

#[derive(Debug, Serialize)]
struct ProbeReport {
    url: String,
    final_url: String,
    final_status: u16,
    hops: Vec<Hop>,
    /// The chain came back to a URL it had already asked for. Reported
    /// separately because the hop list alone reads as "it just kept going".
    redirect_loop: bool,
    /// Still redirecting when the cap ran out.
    hop_limit_hit: bool,
    headers: Vec<(String, String)>,
    headers_truncated: bool,
    total_ms: u32,
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
            "That request could not be read. Check the URL.",
        );
    };
    let start = match q
        .url
        .as_deref()
        .map(|raw| parse_target(raw, &cfg.canonical_origin))
    {
        Some(Ok(url)) => url,
        Some(Err(e)) => return probe::fail(StatusCode::BAD_REQUEST, e),
        None => {
            return probe::fail(
                StatusCode::BAD_REQUEST,
                "Enter a URL, such as https://example.com.",
            );
        }
    };

    // The permit comes first so a visitor turned away by the concurrency cap
    // has not paid for a socket that was never opened.
    let Ok(_permit) = IN_FLIGHT.try_acquire() else {
        return probe::fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "The checker is busy right now. Try again in a moment.",
        );
    };

    // Metered after parsing, because the budget exists to bound outbound
    // sockets: a typo costs nobody a connection and should not spend a turn.
    if !probe::spend_budget(&cfg, &headers, peer, &LIMITER, 1) {
        return probe::fail(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many checks from this address. Wait a minute and try again.",
        );
    }

    let outcome = tokio::time::timeout(PROBE_DEADLINE, walk(start, &cfg.canonical_origin)).await;

    // Hops past the first are charged after the walk: the cost is not known
    // until the chain has been followed, and a ten-hop chain should not be
    // priced the same as a direct answer. A walk that ran out the deadline is
    // charged in full, because a chain designed to stall is the expensive one.
    let extra = match outcome {
        Ok(Ok(ref report)) => report.hops.len().saturating_sub(1),
        Ok(Err(_)) => 0,
        Err(_) => MAX_HOPS - 1,
    };
    charge_hops(&cfg, &headers, peer, extra);

    match outcome {
        Ok(Ok(report)) => (
            probe::probe_headers(),
            Json(Answer {
                ok: true,
                body: report,
            }),
        )
            .into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(_) => {
            ProbeError::Host("The chain did not finish in time. The host is answering too slowly.")
                .into_response()
        }
    }
}

/// Charges the extra hops one cell at a time. `check_key_n` refuses any weight
/// above the configured burst outright — it answers `InsufficientCapacity`
/// rather than draining what it can — so a single multi-cell call would make
/// exactly the long chains this is meant to price cost nothing. Each cell is
/// best-effort: the requests have already been made, and the point is to leave
/// the bucket empty for the next one.
fn charge_hops(
    cfg: &MarketingCfg,
    headers: &HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    extra: usize,
) {
    for _ in 0..extra {
        probe::spend_budget(cfg, headers, peer, &LIMITER, 1);
    }
}

/// Accepts what a person pastes and yields a URL this checker will open, or the
/// sentence explaining why it will not. A bare name becomes HTTPS, which is
/// what someone typing `example.com` means.
fn parse_target(raw: &str, origin: &str) -> Result<Url, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Enter a URL, such as https://example.com.");
    }
    let candidate = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let url = Url::parse(&candidate).map_err(|_| "That is not a URL this checker can read.")?;
    validate(url, origin)
}

/// Every hop goes through this, not just the one the visitor typed: a redirect
/// is an instruction from a stranger's server about what to open next.
fn validate(url: Url, origin: &str) -> Result<Url, &'static str> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only http:// and https:// URLs can be checked.");
    }
    let Some(host) = url.host_str() else {
        return Err("That URL has no host.");
    };
    // Refuses address literals, so the resolver guard is never the only thing
    // standing between this endpoint and the internal network.
    if probe::clean_host(host).is_none() {
        return Err("That does not look like a public domain name.");
    }
    let Some(port) = url.port_or_known_default() else {
        return Err("That URL has no port this checker recognises.");
    };
    if !ALLOWED_PORTS.contains(&port) {
        return Err("That port is not one this checker will open. Web ports only.");
    }
    // Pointing the checker at its own probe would make one visitor request
    // start a nested walk, and every hop of that walk another. The chain
    // amplifies until it drains the global permits and answers real visitors
    // with the busy page.
    if is_own_probe(&url, origin) {
        return Err("This checker will not check itself.");
    }
    Ok(url)
}

/// True for our own probe endpoint on our own origin. Compared on host, because
/// a redirect naming us is the case this exists for, not a same-path URL that
/// happens to live somewhere else.
fn is_own_probe(url: &Url, origin: &str) -> bool {
    let ours = Url::parse(origin).ok();
    let ours = ours.as_ref().and_then(Url::host_str);
    ours.is_some_and(|h| url.host_str() == Some(h)) && url.path().starts_with(HEADER_PROBE_PATH)
}

/// Follows the chain one hop at a time. Nothing here reads a response body:
/// the head carries every fact the page shows, and draining the rest would
/// turn this into a fetching service for whatever a stranger points it at.
async fn walk(start: Url, origin: &str) -> Result<ProbeReport, ProbeError> {
    let began = Instant::now();
    let requested = start.to_string();
    let mut current = start;
    let mut seen: Vec<String> = Vec::new();
    let mut hops: Vec<Hop> = Vec::new();

    loop {
        let url_text = current.to_string();
        if seen.contains(&url_text) {
            return Ok(finish(
                requested,
                hops,
                Vec::new(),
                false,
                began,
                true,
                false,
            ));
        }
        seen.push(url_text.clone());

        // Resolved ahead of the request so a name that only points into
        // private space is answered with a sentence rather than a connector
        // error. The connector filters again at connect time, which is the
        // check that actually holds against rebinding.
        probe::resolve(current.host_str().unwrap_or_default()).await?;

        let hop_began = Instant::now();
        let response = send(&current).await?;
        let ms = elapsed_ms(hop_began);
        let status = response.status();
        let location = response
            .headers()
            .get(hyper::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        hops.push(Hop {
            url: url_text,
            status: status.as_u16(),
            location: location.clone(),
            ms,
        });

        let redirecting = status.is_redirection() && location.is_some();
        if !redirecting {
            let (headers, truncated) = collect_headers(response.headers());
            return Ok(finish(
                requested, hops, headers, truncated, began, false, false,
            ));
        }
        if hops.len() >= MAX_HOPS {
            return Ok(finish(
                requested,
                hops,
                Vec::new(),
                false,
                began,
                false,
                true,
            ));
        }

        let raw = location.unwrap_or_default();
        let next = current
            .join(&raw)
            .map_err(|_| ProbeError::Host("The host redirected to something that is not a URL."))?;
        current = validate(next, origin).map_err(ProbeError::Host)?;
    }
}

/// One request, head only. `Full::new(Bytes::new())` is an empty body, not a
/// missing one: some origins answer a bodyless GET differently.
async fn send(url: &Url) -> Result<hyper::Response<hyper::body::Incoming>, ProbeError> {
    let request = Request::get(url.as_str())
        .header(
            hyper::header::USER_AGENT,
            "Uptimepage-HeaderChecker/1.0 (+https://uptimepage.dev/tools/http-header-checker)",
        )
        .header(hyper::header::ACCEPT, "*/*")
        // Nothing here reads a body, so asking for a compressed one would only
        // make the head harder to read for whoever is looking at the wire.
        .header(hyper::header::ACCEPT_ENCODING, "identity")
        .body(Full::new(Bytes::new()))
        .map_err(|_| ProbeError::Host("That URL cannot be turned into a request."))?;

    match tokio::time::timeout(HOP_TIMEOUT, CLIENT.request(request)).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(e)) => Err(describe(&e)),
        Err(_) => Err(ProbeError::Host("The host did not answer in time.")),
    }
}

/// A connector error carries a chain of sources rather than an `io::Error` we
/// can match on, so the classification reads the rendered chain. Losing egress
/// on our side must not be reported as every visitor's site being dead.
fn describe(e: &(dyn std::error::Error + 'static)) -> ProbeError {
    let mut text = String::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = source {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && probe::egress_is_broken(io)
        {
            return ProbeError::Server(
                StatusCode::SERVICE_UNAVAILABLE,
                "The checker could not open an outbound connection.",
            );
        }
        text.push_str(&err.to_string());
        text.push(' ');
        source = err.source();
    }
    ProbeError::Host(classify(&text))
}

/// The visitor gets a sentence about their host, never a connector's internals.
fn classify(chain: &str) -> &'static str {
    let chain = chain.to_ascii_lowercase();
    if chain.contains("certificate") || chain.contains("tls") || chain.contains("handshake") {
        "The host refused the TLS handshake. Its certificate may be expired, self-signed or missing an intermediate."
    } else if chain.contains("dns") || chain.contains("resolve") {
        "That name does not resolve."
    } else if chain.contains("refused") {
        "Nothing accepted a connection on that port."
    } else if chain.contains("timed out") || chain.contains("timeout") {
        "The host did not answer in time."
    } else {
        "The host did not return a response."
    }
}

/// Header values are a stranger's bytes. They are cut to a readable length and
/// capped in number here; the page inserts them as text, never as markup.
fn collect_headers(map: &hyper::HeaderMap) -> (Vec<(String, String)>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    for (name, value) in map {
        if out.len() >= MAX_HEADERS_REPORTED {
            truncated = true;
            break;
        }
        let text = value.to_str().unwrap_or("<not text>");
        let cut: String = text.chars().take(MAX_HEADER_VALUE_CHARS).collect();
        let cut = if cut.len() < text.len() {
            format!("{cut}…")
        } else {
            cut
        };
        out.push((name.as_str().to_owned(), cut));
    }
    (out, truncated)
}

fn finish(
    requested: String,
    hops: Vec<Hop>,
    headers: Vec<(String, String)>,
    headers_truncated: bool,
    began: Instant,
    redirect_loop: bool,
    hop_limit_hit: bool,
) -> ProbeReport {
    let last = hops.last();
    ProbeReport {
        final_url: last.map_or_else(|| requested.clone(), |h| h.url.clone()),
        final_status: last.map_or(0, |h| h.status),
        url: requested,
        hops,
        redirect_loop,
        hop_limit_hit,
        headers,
        headers_truncated,
        total_ms: elapsed_ms(began),
    }
}

fn elapsed_ms(since: Instant) -> u32 {
    u32::try_from(since.elapsed().as_millis()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://uptimepage.dev";

    #[test]
    fn a_bare_name_becomes_https() {
        let url = parse_target("acme.com", ORIGIN).expect("a bare name is a URL");
        assert_eq!(url.as_str(), "https://acme.com/");
    }

    #[test]
    fn a_pasted_url_keeps_its_path_and_query() {
        let url = parse_target("http://acme.com/health?deep=1", ORIGIN).expect("full URL");
        assert_eq!(url.as_str(), "http://acme.com/health?deep=1");
    }

    #[test]
    fn refuses_what_must_never_be_opened() {
        for raw in [
            "file:///etc/passwd",
            "gopher://acme.com",
            "ftp://acme.com",
            "http://127.0.0.1/",
            "http://localhost/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "https://acme.com:22/",
            "https://acme.com:5432/",
            "",
        ] {
            assert!(
                parse_target(raw, ORIGIN).is_err(),
                "{raw} should be refused"
            );
        }
    }

    /// A redirect is a stranger's instruction about what to open next, so it
    /// goes through the same gate as the URL the visitor typed.
    #[test]
    fn a_redirect_target_is_validated_like_the_first_url() {
        let current = Url::parse("https://acme.com/start").unwrap();
        for evil in ["http://169.254.169.254/", "/../..", "//127.0.0.1/"] {
            if let Ok(joined) = current.join(evil) {
                let allowed = validate(joined.clone(), ORIGIN).is_ok();
                let internal = joined.host_str().is_some_and(|h| {
                    h == "127.0.0.1" || h == "169.254.169.254" || h == "localhost"
                });
                assert!(!(internal && allowed), "{evil} joined to an allowed URL");
            }
        }
    }

    #[test]
    fn a_relative_location_resolves_against_the_current_hop() {
        let current = Url::parse("https://acme.com/a/b").unwrap();
        assert_eq!(
            current.join("/login").unwrap().as_str(),
            "https://acme.com/login"
        );
        assert_eq!(current.join("c").unwrap().as_str(), "https://acme.com/a/c");
    }

    /// A redirect naming our own probe would start a nested walk, and each of
    /// its hops another.
    #[test]
    fn refuses_its_own_probe_endpoint() {
        let own = format!("{ORIGIN}{HEADER_PROBE_PATH}?url=https://acme.com");
        assert!(parse_target(&own, ORIGIN).is_err(), "{own} must be refused");
        // The same path elsewhere is a stranger's server, not our amplifier.
        assert!(parse_target("https://acme.com/tools/http-header-checker/probe", ORIGIN).is_ok());
        // The rest of our own site stays checkable.
        assert!(parse_target(ORIGIN, ORIGIN).is_ok());
    }

    #[test]
    fn port_allowlist_is_web_ports_only() {
        for port in [22, 25, 3306, 5432, 6379, 11211, 9200] {
            assert!(!ALLOWED_PORTS.contains(&port), "port {port} must not open");
        }
        assert!(ALLOWED_PORTS.contains(&80));
        assert!(ALLOWED_PORTS.contains(&443));
    }

    #[test]
    fn a_long_header_value_is_cut_not_dropped() {
        let mut map = hyper::HeaderMap::new();
        let long = "x".repeat(MAX_HEADER_VALUE_CHARS * 2);
        map.insert("x-long", long.parse().unwrap());
        let (headers, truncated) = collect_headers(&map);
        assert!(!truncated, "one header does not hit the count cap");
        assert_eq!(headers.len(), 1);
        assert!(headers[0].1.ends_with('…'));
        assert!(headers[0].1.chars().count() <= MAX_HEADER_VALUE_CHARS + 1);
    }

    #[test]
    fn a_flood_of_headers_is_capped_and_flagged() {
        let mut map = hyper::HeaderMap::new();
        for i in 0..(MAX_HEADERS_REPORTED + 10) {
            let name: hyper::header::HeaderName =
                format!("x-{i}").parse().expect("valid header name");
            map.insert(name, "v".parse().unwrap());
        }
        let (headers, truncated) = collect_headers(&map);
        assert_eq!(headers.len(), MAX_HEADERS_REPORTED);
        assert!(truncated);
    }

    /// The hop ceiling is what stops a chain designed to never end.
    #[test]
    fn the_hop_cap_matches_what_a_monitor_walks() {
        assert_eq!(MAX_HOPS, 10);
    }

    #[test]
    fn a_connector_failure_reads_as_the_host_not_as_our_5xx() {
        let err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(matches!(describe(&err), ProbeError::Host(_)));
    }

    #[test]
    fn losing_egress_stays_ours() {
        let err = std::io::Error::from(std::io::ErrorKind::NetworkUnreachable);
        assert!(matches!(describe(&err), ProbeError::Server(..)));
    }

    #[test]
    fn a_tls_failure_names_the_certificate() {
        // The security checker grades this sentence as a validation failure.
        assert!(classify("invalid peer certificate: Expired").contains("TLS handshake"));
        assert!(classify("connection refused").contains("accepted a connection"));
    }
}
