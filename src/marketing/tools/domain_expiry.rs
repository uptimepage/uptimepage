//! Public RDAP lookup. No worker state or database handles are involved.
//! Registry destinations come from IANA, never from user-supplied URLs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{ConnectInfo, Query, State, rejection::QueryRejection};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use url::Url;

use crate::http_outbound::{OutboundHttpClient, build_outbound_client, get_json};
use crate::marketing::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_faqpage, json_ld_web_application,
    json_ld_webpage,
};
use crate::security::{
    SsrfGuard,
    rdap::{DomainResponse, override_url},
};
use crate::templates::filters;

use super::super::config::{BRAND, MarketingCfg};
use super::super::pages::{CachedRender, cached_render, serve_cached};
use super::probe::{self, Answer, ProbeLimiter};
use super::{TOOL_CACHE_CONTROL, TOOLS, ToolMeta};

pub const DOMAIN_CHECKER_PATH: &str = "/tools/domain-expiry-checker";
pub const DOMAIN_PROBE_PATH: &str = "/tools/domain-expiry-checker/probe";
pub const DOMAIN_CHECKER_TITLE: &str = "Domain Expiry Checker: Date and Registrar";
pub const DOMAIN_CHECKER_LABEL: &str = "domain expiry checker";
pub const DOMAIN_CHECKER_DESCRIPTION: &str = "Check a domain's registration expiry date, days remaining and registrar using public RDAP data. Free, no sign-up. Set up monitoring before renewal is due.";
pub const DOMAIN_CHECKER_LASTMOD: &str = "2026-09-05";
const BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";
const DEADLINE: Duration = Duration::from_secs(20);
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

const FAQS: &[(&str, &str)] = &[
    (
        "What does this domain expiry checker look up?",
        "It reads the expiration event from public RDAP registration data and shows the registrar when provided. It checks the registered domain, so app.example.com becomes example.com. It does not test the website or its SSL certificate.",
    ),
    (
        "Why is the expiry date unavailable?",
        "Some registries do not publish an expiry date through RDAP. A lookup can also fail because of rate limits or a temporary registry problem. An unavailable result does not mean the domain is unregistered or available to buy. Check your registrar account for its renewal deadline.",
    ),
    (
        "Is the registry expiry date my payment deadline?",
        "Not necessarily. Your registrar may require payment earlier, and registry auto-renewal can change the public date before your own renewal is settled. Check your registrar account and billing details rather than treating this result as proof of payment.",
    ),
    (
        "Does an expired domain become available immediately?",
        "No. Expiration is not the same as deletion or availability for registration. Grace periods and recovery rules vary by registry and registrar. Contact your registrar promptly if your renewal is overdue.",
    ),
    (
        "Can I receive domain expiry alerts?",
        "Yes. The monitor-this-domain link starts Uptimepage's domain-monitor setup with the registered domain filled in. Choose your warning thresholds and notification channel. The public lookup itself does not schedule checks or send alerts.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_domain_expiry.html")]
struct DomainExpiryPage {
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

static PAGE: OnceLock<CachedRender> = OnceLock::new();
static LIMITER: LazyLock<ProbeLimiter> = LazyLock::new(|| probe::limiter(6, 3));
static LOOKUP: LazyLock<Lookup> = LazyLock::new(|| {
    Lookup::new(
        build_outbound_client(SsrfGuard::strict()),
        BOOTSTRAP_URL.to_owned(),
    )
});

pub(super) fn render(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, DOMAIN_CHECKER_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{DOMAIN_CHECKER_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = DOMAIN_CHECKER_DESCRIPTION.to_owned();
    let page = DomainExpiryPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            DOMAIN_CHECKER_TITLE,
            DOMAIN_CHECKER_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            DOMAIN_CHECKER_TITLE,
            DOMAIN_CHECKER_PATH,
            DOMAIN_CHECKER_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            DOMAIN_CHECKER_PATH,
            DOMAIN_CHECKER_TITLE,
            DOMAIN_CHECKER_LASTMOD,
            DOMAIN_CHECKER_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(FAQS),
        faqs: FAQS,
        probe_path: DOMAIN_PROBE_PATH,
        canonical_url,
        og,
        tools: TOOLS,
        self_path: DOMAIN_CHECKER_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    cached_render(
        page.render()
            .unwrap_or_else(|e| format!("<!-- domain-expiry render failed: {e} -->")),
    )
}

pub(super) fn warm(cfg: &MarketingCfg) {
    PAGE.get_or_init(|| render(cfg));
}

pub(super) async fn page(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    serve_cached(
        &headers,
        PAGE.get_or_init(|| render(&cfg)),
        &TOOL_CACHE_CONTROL,
    )
}

#[derive(Debug, Deserialize)]
pub(super) struct ProbeQuery {
    domain: Option<String>,
}

pub(super) async fn probe(
    State(cfg): State<Arc<MarketingCfg>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    query: Result<Query<ProbeQuery>, QueryRejection>,
) -> Response {
    let domain = query
        .ok()
        .and_then(|Query(q)| q.domain)
        .and_then(|s| clean_domain(&s));
    let Some(domain) = domain else {
        return probe::fail(
            StatusCode::BAD_REQUEST,
            "Enter a domain with a recognized public suffix, such as example.com. IP addresses and private names cannot be checked.",
        );
    };
    if !probe::spend_budget(&cfg, &headers, peer, &LIMITER, 1) {
        return probe::fail(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many checks from this address. Wait a minute and try again.",
        );
    }
    match tokio::time::timeout(DEADLINE, LOOKUP.lookup(&domain)).await {
        Ok(Ok(answer)) => (
            probe::probe_headers(),
            Json(Answer {
                ok: true,
                body: report(domain, answer, Utc::now()),
            }),
        )
            .into_response(),
        Ok(Err(e)) => e.response(),
        Err(_) => Failure::Unavailable.response(),
    }
}

/// Ignore private hosting suffixes: a tenant on github.io does not own a
/// separate registry registration. Keep multi-label ICANN suffixes intact.
fn clean_domain(raw: &str) -> Option<String> {
    if raw.len() > 2048 {
        return None;
    }
    let host = probe::clean_host(raw)?;
    let mut candidate = host.as_str();
    loop {
        let suffix = psl::suffix(candidate.as_bytes())?;
        match suffix.typ()? {
            psl::Type::Icann => {
                let suffix = std::str::from_utf8(suffix.as_bytes()).ok()?;
                let before = host.strip_suffix(suffix)?.strip_suffix('.')?;
                return Some(format!("{}.{}", before.rsplit('.').next()?, suffix));
            }
            psl::Type::Private => candidate = candidate.split_once('.')?.1,
        }
    }
}

#[derive(Clone, Debug)]
struct Registration {
    expires_at: DateTime<Utc>,
    registrar: Option<String>,
    source_url: String,
    checked_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct Report {
    domain: String,
    expires_at: DateTime<Utc>,
    registrar: Option<String>,
    days_remaining: i64,
    expired: bool,
    source_url: String,
    checked_at: DateTime<Utc>,
}

fn report(domain: String, answer: Registration, now: DateTime<Utc>) -> Report {
    Report {
        domain,
        days_remaining: (answer.expires_at - now).num_days(),
        expired: answer.expires_at <= now,
        expires_at: answer.expires_at,
        registrar: answer.registrar,
        source_url: answer.source_url,
        checked_at: answer.checked_at,
    }
}

#[derive(Debug)]
enum Failure {
    Busy,
    Unsupported,
    NoExpiry,
    Unavailable,
}

impl Failure {
    fn response(&self) -> Response {
        let (status, message) = match self {
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "The checker is busy. Wait a minute and try again.",
            ),
            Self::Unsupported => (
                StatusCode::OK,
                "No supported public RDAP service was found for this domain ending. Check the expiry date in your registrar account. This does not mean the domain is available to buy.",
            ),
            Self::NoExpiry => (
                StatusCode::OK,
                "The registry did not provide a usable expiry date. Check your registrar account for its renewal deadline. This does not mean the domain is available to buy.",
            ),
            Self::Unavailable => (
                StatusCode::OK,
                "The registration lookup could not be completed. The registry may be unavailable, rate-limited, or have no record for this name. Try later or check your registrar account; this is not an availability check.",
            ),
        };
        probe::fail(status, message)
    }
}

struct Lookup {
    http: OutboundHttpClient,
    bootstrap_url: String,
    bootstrap: Cache<(), Arc<HashMap<String, String>>>,
    answers: Cache<String, Registration>,
    upstream_budget: DefaultDirectRateLimiter,
    inflight: Semaphore,
}

impl Lookup {
    fn new(http: OutboundHttpClient, bootstrap_url: String) -> Self {
        Self {
            http,
            bootstrap_url,
            bootstrap: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(86400))
                .build(),
            answers: Cache::builder()
                .max_capacity(1024)
                .time_to_live(CACHE_TTL)
                .build(),
            upstream_budget: RateLimiter::direct(
                Quota::per_minute(NonZeroU32::new(30).unwrap())
                    .allow_burst(NonZeroU32::new(4).unwrap()),
            ),
            inflight: Semaphore::new(4),
        }
    }

    async fn lookup(&self, domain: &str) -> Result<Registration, Arc<Failure>> {
        // Moka coalesces misses for one domain and bounds cache growth. Only
        // misses consume the process-wide registry budget or a socket slot.
        self.answers
            .try_get_with(domain.to_owned(), async {
                let _permit = self.inflight.try_acquire().map_err(|_| Failure::Busy)?;
                self.upstream_budget.check().map_err(|_| Failure::Busy)?;
                let tld = domain.rsplit('.').next().ok_or(Failure::Unsupported)?;
                let base = if let Some(base) = override_url(tld) {
                    base.to_owned()
                } else {
                    let map = self
                        .bootstrap
                        .try_get_with((), async {
                            let url = Url::parse(&self.bootstrap_url)
                                .map_err(|_| Failure::Unavailable)?;
                            let raw: Bootstrap = get_json(&self.http, &url)
                                .await
                                .map_err(|_| Failure::Unavailable)?;
                            Ok::<_, Failure>(Arc::new(bootstrap_map(raw)))
                        })
                        .await
                        .map_err(|_| Failure::Unavailable)?;
                    map.get(tld).cloned().ok_or(Failure::Unsupported)?
                };
                let url = domain_url(&base, domain).ok_or(Failure::Unsupported)?;
                // get_json caps the body at 1 MiB and never follows redirects;
                // the connector rejects private/reserved IPs at connect time.
                let raw: DomainResponse = get_json(&self.http, &url)
                    .await
                    .map_err(|_| Failure::Unavailable)?;
                registration(domain, &url, raw)
            })
            .await
    }
}

fn registration(domain: &str, url: &Url, raw: DomainResponse) -> Result<Registration, Failure> {
    if raw.object_class.as_deref() != Some("domain")
        || raw
            .name
            .as_deref()
            .is_some_and(|name| !name.trim_end_matches('.').eq_ignore_ascii_case(domain))
    {
        return Err(Failure::Unavailable);
    }
    Ok(Registration {
        expires_at: raw.expiration().ok_or(Failure::NoExpiry)?,
        registrar: raw.registrar(),
        source_url: url.to_string(),
        checked_at: Utc::now(),
    })
}

#[derive(Deserialize)]
struct Bootstrap {
    services: Vec<(Vec<String>, Vec<String>)>,
}

fn bootstrap_map(raw: Bootstrap) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (tlds, urls) in raw.services {
        if let Some(base) = urls
            .into_iter()
            .find(|base| domain_url(base, "example.com").is_some())
        {
            for tld in tlds {
                map.insert(tld.to_ascii_lowercase(), base.clone());
            }
        }
    }
    map
}

fn domain_url(base: &str, domain: &str) -> Option<Url> {
    let mut url = Url::parse(base).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    // Literal/private destinations are refused here as well as at connect.
    probe::clean_host(url.host_str()?)?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push("domain")
        .push(domain);
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_registration_names() {
        for (input, want) in [
            ("https://APP.Example.COM/path", "example.com"),
            ("www.example.co.uk.", "example.co.uk"),
            ("https://bücher.de/", "xn--bcher-kva.de"),
            ("alice.github.io", "github.io"),
            ("github.io", "github.io"),
        ] {
            assert_eq!(clean_domain(input).as_deref(), Some(want), "{input}");
        }
        for input in [
            "",
            "com",
            "co.uk",
            "127.0.0.1",
            "[::1]",
            "localhost",
            "x.internal",
            "example.com:43",
            "a\r\n.com",
            "example..com",
        ] {
            assert!(clean_domain(input).is_none(), "{input}");
        }
    }

    #[test]
    fn provider_urls_are_https_and_cannot_become_arbitrary_targets() {
        assert_eq!(
            domain_url("https://rdap.example.com/base", "example.com")
                .unwrap()
                .as_str(),
            "https://rdap.example.com/base/domain/example.com"
        );
        for base in [
            "http://rdap.example.com/",
            "https://127.0.0.1/",
            "https://localhost/",
            "https://rdap.example.com:8443/",
            "https://user@rdap.example.com/",
            "https://rdap.example.com/?x=1",
        ] {
            assert!(domain_url(base, "example.com").is_none(), "{base}");
        }
    }

    #[test]
    fn reports_exact_expiry_even_when_whole_days_is_zero() {
        let now = Utc::now();
        for seconds in [-1, 0, 1] {
            let answer = Registration {
                expires_at: now + chrono::Duration::seconds(seconds),
                registrar: None,
                source_url: String::new(),
                checked_at: now,
            };
            let r = report("example.com".into(), answer, now);
            assert_eq!(r.days_remaining, 0);
            assert_eq!(r.expired, seconds <= 0);
        }
    }

    #[tokio::test]
    async fn unavailable_results_are_json_and_never_claim_availability() {
        for error in [
            Failure::Unsupported,
            Failure::NoExpiry,
            Failure::Unavailable,
        ] {
            let response = error.response();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["x-robots-tag"], "noindex");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["ok"], false);
            assert!(body["error"].as_str().unwrap().contains("registrar"));
        }
    }

    #[test]
    fn registry_response_requires_a_domain_and_a_real_expiry() {
        let url = Url::parse("https://rdap.example.com/domain/example.com").unwrap();
        let response = |body| serde_json::from_value::<DomainResponse>(body).unwrap();
        for body in [
            serde_json::json!({"objectClassName": "entity"}),
            serde_json::json!({"objectClassName": "domain", "ldhName": "other.com"}),
        ] {
            assert!(matches!(
                registration("example.com", &url, response(body)),
                Err(Failure::Unavailable)
            ));
        }
        assert!(matches!(
            registration(
                "example.com",
                &url,
                response(serde_json::json!({"objectClassName": "domain"}))
            ),
            Err(Failure::NoExpiry)
        ));
        let raw = response(
            serde_json::json!({"objectClassName": "domain", "ldhName": "EXAMPLE.COM.", "events": [{"eventAction": "expiration", "eventDate": "2027-01-01T00:00:00Z"}]}),
        );
        let answer = registration("example.com", &url, raw).unwrap();
        assert_eq!(answer.source_url, url.as_str());
        assert!(answer.registrar.is_none());
    }

    #[tokio::test]
    async fn cached_answers_work_without_a_registry_budget_or_socket_slot() {
        let lookup = Lookup::new(
            build_outbound_client(SsrfGuard::strict()),
            "invalid bootstrap".into(),
        );
        let now = Utc::now();
        let answer = Registration {
            expires_at: now + chrono::Duration::days(10),
            registrar: None,
            source_url: "https://rdap.example.com/".into(),
            checked_at: now,
        };
        lookup.answers.insert("example.com".into(), answer).await;
        let _permits = lookup.inflight.acquire_many(4).await.unwrap();
        while lookup.upstream_budget.check().is_ok() {}
        assert!(lookup.lookup("example.com").await.is_ok());
        assert!(matches!(
            &*lookup.lookup("other.com").await.unwrap_err(),
            Failure::Busy
        ));
    }

    #[tokio::test]
    async fn absent_bootstrap_entry_is_not_cached_as_a_success() {
        let lookup = Lookup::new(
            build_outbound_client(SsrfGuard::strict()),
            "invalid bootstrap".into(),
        );
        lookup.bootstrap.insert((), Arc::new(HashMap::new())).await;
        assert!(matches!(
            &*lookup.lookup("example.com").await.unwrap_err(),
            Failure::Unsupported
        ));
        assert!(lookup.answers.get("example.com").await.is_none());
    }

    #[tokio::test]
    #[ignore = "contacts IANA and a live public registry"]
    async fn live_domain_lookup() {
        let answer = tokio::time::timeout(DEADLINE, LOOKUP.lookup("uptimepage.dev"))
            .await
            .unwrap()
            .unwrap();
        let result = report("uptimepage.dev".into(), answer, Utc::now());
        assert!(result.source_url.starts_with("https://"));
        println!("{}", serde_json::to_string(&result).unwrap());
    }
}
