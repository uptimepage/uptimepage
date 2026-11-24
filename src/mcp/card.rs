//! Server Card: what this MCP server is and where to reach it, for agents
//! that have a domain but no connection yet.
//!
//! Built from the same [`ServerHandler::get_info`] the protocol's
//! `initialize` returns, so the advertised identity cannot drift from the
//! running server.

use axum::extract::State;
use axum::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rmcp::ServerHandler;
use rmcp::model::ProtocolVersion;
use serde_json::json;

use crate::app::AppState;

use super::server::McpServer;

/// `<streamable-http-url>/server-card`, the location MCP reserves for the card.
pub(crate) const PATH: &str = "/mcp/server-card";
/// Where agents found the card before the reserved location existed.
pub(super) const WELL_KNOWN_PATH: &str = "/.well-known/mcp/server-card.json";

const SCHEMA: &str = "https://static.modelcontextprotocol.io/schemas/v1/server-card.schema.json";
const REPOSITORY: &str = "https://github.com/uptimepage/uptimepage";

const CARD_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/mcp-server-card+json");
const CARD_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=3600");
const ANY_ORIGIN: HeaderValue = HeaderValue::from_static("*");

pub(super) async fn server_card(State(state): State<AppState>) -> Response {
    let endpoint = state.cfg.mcp.resource_uri.clone();
    if endpoint.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let info = McpServer::new(state.clone()).get_info();
    let website = state.cfg.site.origin.clone();
    let host = state
        .cfg
        .site
        .host()
        .unwrap_or_else(|| host_of(&endpoint).to_owned());
    let namespace = reverse_dns(&host);
    // Streamable HTTP first shipped in 2025-03-26; older versions only spoke HTTP+SSE.
    let protocol_versions: Vec<&str> = ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .filter(|v| *v >= &ProtocolVersion::V_2025_03_26)
        .map(ProtocolVersion::as_str)
        .collect();

    let mut card = json!({
        "$schema": SCHEMA,
        "name": format!("{namespace}/{}", info.server_info.name),
        "title": info.server_info.title.unwrap_or(info.server_info.name),
        "version": info.server_info.version,
        "description": info.server_info.description,
        "repository": { "url": REPOSITORY, "source": "github" },
        "remotes": [{
            "type": "streamable-http",
            "url": endpoint,
            "supportedProtocolVersions": protocol_versions,
        }],
    });
    if !website.is_empty() {
        card["websiteUrl"] = json!(website);
    }

    (
        [
            (CONTENT_TYPE, CARD_CONTENT_TYPE),
            (CACHE_CONTROL, CARD_CACHE_CONTROL),
            (ACCESS_CONTROL_ALLOW_ORIGIN, ANY_ORIGIN),
        ],
        serde_json::to_vec(&card).expect("server card serialises"),
    )
        .into_response()
}

fn host_of(url: &str) -> &str {
    let after_scheme = url.split_once("//").map_or(url, |(_, rest)| rest);
    after_scheme
        .split(['/', ':'])
        .next()
        .unwrap_or(after_scheme)
}

/// `uptimepage.dev` → `dev.uptimepage`, the reverse-DNS namespace the
/// card's `name` is built from.
fn reverse_dns(host: &str) -> String {
    host.split('.').rev().collect::<Vec<_>>().join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `server.json` is what the MCP registry publishes, and every downstream
    /// directory mirrors from there. Drift leaves one server wearing two
    /// identities, so a release that bumps the crate must bump the entry too.
    #[test]
    fn the_published_registry_entry_matches_the_card() {
        let entry: serde_json::Value =
            serde_json::from_str(include_str!("../../server.json")).expect("server.json parses");
        let namespace = reverse_dns(host_of("https://uptimepage.dev"));

        assert_eq!(
            entry["name"],
            json!(format!("{namespace}/{}", super::super::server::SERVER_NAME))
        );
        assert_eq!(entry["title"], json!(super::super::server::SERVER_TITLE));
        assert_eq!(
            entry["description"],
            json!(super::super::server::SERVER_DESCRIPTION)
        );
        assert_eq!(entry["repository"]["url"], json!(REPOSITORY));
        assert_eq!(entry["version"], json!(env!("CARGO_PKG_VERSION")));
        assert_eq!(
            entry["remotes"][0]["url"],
            json!(format!(
                "https://mcp.uptimepage.dev{}",
                crate::request::MCP_PATH
            )),
            "the registry would hand every client an endpoint we no longer serve"
        );
        assert!(
            entry["description"]
                .as_str()
                .is_some_and(|d| d.len() <= 100),
            "the registry caps description at 100 characters and rejects at publish time"
        );
    }

    #[test]
    fn card_lives_under_the_transport_endpoint() {
        assert_eq!(PATH, format!("{}/server-card", crate::request::MCP_PATH));
    }

    #[test]
    fn namespace_is_the_domain_reversed() {
        assert_eq!(
            reverse_dns(host_of("https://uptimepage.dev")),
            "dev.uptimepage"
        );
        assert_eq!(
            reverse_dns(host_of("https://mcp.uptimepage.dev/mcp")),
            "dev.uptimepage.mcp"
        );
        assert_eq!(reverse_dns(host_of("http://app.lvh.me:8080")), "me.lvh.app");
    }
}
