//! RFC 9727 API catalog + the RFC 8288 `Link` header advertising it.
//!
//! URLs come from `MarketingCfg`, never the request, so a self-hosted
//! deployment advertises its own API. Public read-only surfaces only.

use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use bytes::Bytes;
use serde_json::json;

use super::config::MarketingCfg;

pub const CATALOG_PATH: &str = "/.well-known/api-catalog";

/// SEP-1649. The card itself is served by the MCP host, which owns the
/// server's real name, version and capabilities.
pub const CARD_PATH: &str = "/.well-known/mcp/server-card.json";

const LINKSET_JSON: HeaderValue = HeaderValue::from_static(
    "application/linkset+json; profile=\"https://www.rfc-editor.org/info/rfc9727\"",
);

const CATALOG_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=86400");

// Uncredentialed public metadata: readable by browser-side agents.
const ANY_ORIGIN: HeaderValue = HeaderValue::from_static("*");

static CATALOG_CACHED: OnceLock<Bytes> = OnceLock::new();

pub async fn api_catalog(State(cfg): State<Arc<MarketingCfg>>) -> Response {
    let body = CATALOG_CACHED.get_or_init(|| build_catalog(&cfg));
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, LINKSET_JSON),
            (CACHE_CONTROL, CATALOG_CACHE_CONTROL),
            (ACCESS_CONTROL_ALLOW_ORIGIN, ANY_ORIGIN),
        ],
        body.clone(),
    )
        .into_response()
}

/// An agent that only knows the site looks here; the card lives on the MCP
/// host, so point at it rather than restating it out of a second source.
pub async fn mcp_server_card(State(cfg): State<Arc<MarketingCfg>>) -> Response {
    match cfg.mcp_url.as_deref() {
        Some(mcp) => Redirect::temporary(&format!("{}{CARD_PATH}", origin_of(mcp))).into_response(),
        None => super::pages::not_found_page(&cfg),
    }
}

pub(crate) fn warm(cfg: &MarketingCfg) {
    CATALOG_CACHED.get_or_init(|| build_catalog(cfg));
}

fn build_catalog(cfg: &MarketingCfg) -> Bytes {
    let origin = trim_slash(&cfg.canonical_origin);
    let app = trim_slash(&cfg.app_url);
    let rest = format!("{app}/api/v1");

    let mut items = vec![json!({ "href": rest, "title": "REST API" })];
    let mut resources = vec![json!({
        "anchor": rest,
        "service-desc": [{ "href": format!("{app}/api/openapi.json"), "type": "application/json" }],
        "service-doc": [{ "href": format!("{origin}/docs/api"), "type": "text/html" }],
    })];

    if let Some(mcp) = cfg.mcp_url.as_deref() {
        let mcp_origin = origin_of(mcp);
        items.push(json!({ "href": mcp, "title": "MCP server" }));
        resources.push(json!({
            "anchor": mcp,
            "service-doc": [{ "href": format!("{origin}/docs/mcp"), "type": "text/html" }],
            "service-desc": [{
                "href": format!("{mcp_origin}{CARD_PATH}"),
                "type": "application/json",
            }],
            "describedby": [{
                "href": format!("{mcp_origin}/.well-known/oauth-protected-resource"),
                "type": "application/json",
            }],
        }));
    }

    let mut linkset = vec![json!({
        "anchor": format!("{origin}{CATALOG_PATH}"),
        "item": items,
    })];
    linkset.append(&mut resources);

    Bytes::from(serde_json::to_vec(&json!({ "linkset": linkset })).expect("catalog serialises"))
}

/// `None` when a configured URL cannot be a header value — an operator
/// typo must not inject a second header.
pub(crate) fn link_header(cfg: &MarketingCfg) -> Option<HeaderValue> {
    let origin = trim_slash(&cfg.canonical_origin);
    let app = trim_slash(&cfg.app_url);
    let value = format!(
        "<{origin}{CATALOG_PATH}>; rel=\"api-catalog\", \
         <{app}/api/openapi.json>; rel=\"service-desc\"; type=\"application/json\", \
         <{origin}/docs/api>; rel=\"service-doc\"; type=\"text/html\""
    );
    match HeaderValue::from_str(&value) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "discovery Link header not emitted: unusable URL in config");
            None
        }
    }
}

fn trim_slash(url: &str) -> &str {
    url.trim_end_matches('/')
}

fn origin_of(url: &str) -> &str {
    match url.split_once("//") {
        Some((_, rest)) => match rest.find('/') {
            Some(i) => &url[..url.len() - (rest.len() - i)],
            None => url,
        },
        None => url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MarketingCfg {
        MarketingCfg {
            app_url: "https://app.uptimepage.dev/".into(),
            canonical_origin: "https://uptimepage.dev".into(),
            blog_enabled: false,
            mcp_url: Some("https://mcp.uptimepage.dev/mcp".into()),
            checkout_open: false,
            trusted_proxies: Vec::new(),
        }
    }

    fn hrefs(v: &serde_json::Value) -> Vec<String> {
        let mut out = Vec::new();
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    if k == "href" || k == "anchor" {
                        out.push(val.as_str().unwrap_or_default().to_string());
                    } else {
                        out.extend(hrefs(val));
                    }
                }
            }
            serde_json::Value::Array(items) => out.extend(items.iter().flat_map(hrefs)),
            _ => {}
        }
        out
    }

    #[test]
    fn catalog_is_a_linkset_with_absolute_https_urls() {
        let doc: serde_json::Value = serde_json::from_slice(&build_catalog(&cfg())).unwrap();
        let set = doc["linkset"].as_array().expect("linkset array");
        assert_eq!(
            set[0]["anchor"],
            "https://uptimepage.dev/.well-known/api-catalog"
        );
        assert_eq!(set[0]["item"].as_array().unwrap().len(), 2);
        for href in hrefs(&doc) {
            assert!(href.starts_with("https://"), "relative or insecure: {href}");
            assert!(
                !href.contains("//api"),
                "double slash from trailing-slash origin: {href}"
            );
        }
    }

    #[test]
    fn mcp_entry_is_omitted_when_unconfigured() {
        let doc: serde_json::Value = serde_json::from_slice(&build_catalog(&MarketingCfg {
            mcp_url: None,
            trusted_proxies: Vec::new(),
            ..cfg()
        }))
        .unwrap();
        assert!(!hrefs(&doc).iter().any(|h| h.contains("/mcp")));
    }

    #[test]
    fn mcp_metadata_href_drops_the_endpoint_path() {
        assert_eq!(
            origin_of("https://mcp.uptimepage.dev/mcp"),
            "https://mcp.uptimepage.dev"
        );
        assert_eq!(
            origin_of("https://mcp.uptimepage.dev"),
            "https://mcp.uptimepage.dev"
        );
    }

    #[test]
    fn link_header_carries_every_registered_relation() {
        let v = link_header(&cfg()).expect("valid header");
        let s = v.to_str().unwrap();
        assert!(
            s.contains("<https://uptimepage.dev/.well-known/api-catalog>; rel=\"api-catalog\"")
        );
        assert!(s.contains("<https://app.uptimepage.dev/api/openapi.json>; rel=\"service-desc\""));
        assert!(s.contains("<https://uptimepage.dev/docs/api>; rel=\"service-doc\""));
    }

    #[test]
    fn link_header_refuses_a_config_url_that_would_split_the_header() {
        let bad = MarketingCfg {
            canonical_origin: "https://uptimepage.dev\r\nX-Injected: 1".into(),
            ..cfg()
        };
        assert!(link_header(&bad).is_none());
    }
}
