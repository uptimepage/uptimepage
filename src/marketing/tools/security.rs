//! Combined security report. Browser orchestration reuses the existing, guarded
//! SSL and header probes so request limits and SSRF protections remain shared.

use std::sync::{Arc, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::marketing::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_faqpage, json_ld_web_application,
    json_ld_webpage,
};
use crate::templates::filters;

use super::super::config::{BRAND, MarketingCfg};
use super::super::pages::{CachedRender, cached_render, serve_cached};
use super::{TOOL_CACHE_CONTROL, TOOLS, ToolMeta};
use super::{http_headers, ssl};

pub const SECURITY_CHECKER_PATH: &str = "/tools/website-security-checker";
const SECURITY_CHECKER_CREATED: &str = "2026-09-18";
pub const SECURITY_CHECKER_LASTMOD: &str = "2026-09-18";
pub const SECURITY_CHECKER_TITLE: &str = "Website Security Checker: SSL, HTTPS & Headers";
pub const SECURITY_CHECKER_LABEL: &str = "website security checker";
pub const SECURITY_CHECKER_DESCRIPTION: &str = "Check your website's SSL certificate, HTTPS redirects and security headers. See observed issues and practical fixes in one free report. No sign-up.";

const SECURITY_CHECKER_FAQS: &[(&str, &str)] = &[
    (
        "What does this website security checker test?",
        "It reads the requested host's certificate, follows the HTTPS URL and its HTTP equivalent, and reviews security headers on the final HTTPS response. Each finding includes the observed evidence and a suggested next step. Checks use public web ports 80 and 443.",
    ),
    (
        "Does a passing report mean my website is secure?",
        "No. A pass applies only to the configuration check shown. This tool does not test application vulnerabilities, malware, authenticated pages, cookies, mixed content or every TLS protocol and cipher. It does not crawl your website or run a browser.",
    ),
    (
        "Why are some checks marked not checked?",
        "A timeout, blocked request, rate limit or incomplete response can prevent a check. Truncated headers and complex policies may also need manual review. Missing evidence is never counted as a pass or treated as proof that your website is vulnerable.",
    ),
    (
        "Which page do the security headers describe?",
        "The final response reached from your HTTPS URL. If your site redirects to another host, the report names that destination. Headers on other pages, redirect responses and signed-in sessions can differ. Certificate details describe the hostname you entered.",
    ),
    (
        "Why do you check HTTP as well as HTTPS?",
        "A working HTTPS page does not show what happens to someone following an HTTP link. The separate HTTP check shows whether that URL redirects to HTTPS and whether the observed chain ever downgrades back to HTTP.",
    ),
    (
        "Can I monitor these results automatically?",
        "Uptimepage can monitor uptime and certificate expiry. This free configuration report is a one-time check; the monitoring links set up uptime or certificate monitoring, not scheduled security-header scans.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_security_checker.html")]
struct SecurityCheckerPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    ssl_probe_path: &'static str,
    header_probe_path: &'static str,
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

static SECURITY_CHECKER_CACHED: OnceLock<CachedRender> = OnceLock::new();

pub(super) fn render(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, SECURITY_CHECKER_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{SECURITY_CHECKER_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = SECURITY_CHECKER_DESCRIPTION.to_string();
    let page = SecurityCheckerPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            SECURITY_CHECKER_TITLE,
            SECURITY_CHECKER_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            SECURITY_CHECKER_TITLE,
            SECURITY_CHECKER_PATH,
            SECURITY_CHECKER_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            SECURITY_CHECKER_PATH,
            SECURITY_CHECKER_TITLE,
            SECURITY_CHECKER_CREATED,
            SECURITY_CHECKER_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(SECURITY_CHECKER_FAQS),
        faqs: SECURITY_CHECKER_FAQS,
        ssl_probe_path: ssl::SSL_PROBE_PATH,
        header_probe_path: http_headers::HEADER_PROBE_PATH,
        canonical_url,
        og,
        tools: TOOLS,
        self_path: SECURITY_CHECKER_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- security-checker render failed: {e} -->"));
    cached_render(body)
}

pub(super) fn warm(cfg: &MarketingCfg) {
    SECURITY_CHECKER_CACHED.get_or_init(|| render(cfg));
}

pub(super) async fn page(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = SECURITY_CHECKER_CACHED.get_or_init(|| render(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

#[cfg(test)]
mod tests {
    /// The edge must keep serving what this checker grades as a pass, or
    /// the dogfood report on our own hosts regresses without a test failing.
    #[test]
    fn the_edge_csp_passes_our_own_checker() {
        let caddy =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/deployment/Caddyfile"))
                .expect("read Caddyfile");
        let policies: Vec<&str> = caddy
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("Content-Security-Policy "))
            .collect();
        assert_eq!(
            policies.len(),
            2,
            "marketing apex and tenant hosts each carry a CSP"
        );
        for policy in policies {
            assert!(policy.contains("object-src 'none'"), "{policy}");
            assert!(policy.contains("base-uri 'none'"), "{policy}");
            assert!(policy.contains("script-src 'self'"), "{policy}");
            assert!(
                !policy.contains("script-src 'self' 'unsafe-inline'"),
                "{policy}"
            );
        }
    }
}
