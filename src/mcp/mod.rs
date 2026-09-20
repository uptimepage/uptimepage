//! Model Context Protocol server — read tools at `/mcp`.
//!
//! A customer's LLM (Claude Desktop / IDE via `mcp-remote`, or the claude.ai
//! connector once OAuth lands) answers operational questions about **their own
//! org** through typed, authorized, side-effect-free tools. This is another
//! authorized front door to the same stores the web app and `/api/v1` use, not
//! a bypass: tenant isolation, scopes, rate limits, and audit all apply.
//!
//! Transport: Streamable HTTP via the official `rmcp` crate's
//! [`StreamableHttpService`], mounted as a `tower::Service` on the existing
//! axum router. Auth is an axum middleware in front of the service: it resolves
//! a scoped `sm_live_` token, requires an org binding + live membership, and
//! injects [`AuthContext`] into request extensions; tools read it back from the
//! `RequestContext`. The org is always the token's — never a tool argument.

mod audit;
mod auth;
mod card;
mod confirm;
mod cursor;
mod error;
mod schema;
mod server;

use std::sync::Arc;

use axum::Router;
use axum::middleware::from_fn_with_state;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};

use crate::app::AppState;
use server::McpServer;

use crate::request::MCP_PATH;

/// Mount the read MCP server at `/mcp` when `cfg.mcp.enabled`. No-op otherwise,
/// so a deployment without the dedicated host + Caddy route never exposes it.
///
/// Layer order (outermost first): auth runs first and injects [`AuthContext`],
/// then the shared per-plan rate limiter keys on the resolved org/user, then
/// the rmcp transport service handles the JSON-RPC.
pub fn mount(router: Router, state: AppState) -> Router {
    if !state.cfg.mcp.enabled {
        return router;
    }
    let svc = build_service(state.clone());
    let mcp = Router::new()
        .nest_service(MCP_PATH, svc)
        // Added inner→outer: rate-limit first (inner) so auth runs before it
        // and the limiter sees the resolved org/user, never the TCP peer.
        .layer(from_fn_with_state(
            state.clone(),
            crate::request::rate_limit::rate_limit_middleware,
        ))
        .layer(from_fn_with_state(state.clone(), auth::middleware));
    // Public discovery: outside the auth + rate-limit layers, and only once
    // there is an absolute endpoint to advertise.
    if state.cfg.mcp.resource_uri.is_empty() {
        return router.merge(mcp);
    }
    let discovery = Router::new()
        .route(card::PATH, axum::routing::get(card::server_card))
        .with_state(state);
    router.merge(mcp).merge(discovery)
}

/// Build the rmcp Streamable-HTTP service. `allowed_origins` feeds the
/// transport's RFC 6454 Origin check (DNS-rebinding defense); an empty list
/// disables it and a missing `Origin` header always passes (non-browser
/// clients like `mcp-remote` send none). The `MCP-Protocol-Version` 400 and
/// the 202-for-notifications behaviour are handled by the transport itself.
fn build_service(state: AppState) -> StreamableHttpService<McpServer, LocalSessionManager> {
    let mut config = StreamableHttpServerConfig::default()
        .with_allowed_origins(state.cfg.mcp.allowed_origins.clone());
    // rmcp's `allowed_hosts` defaults to localhost-only (DNS-rebinding defense),
    // which 403s the real MCP host (`mcp.{domain}` in prod, `app.lvh.me` in
    // dev). Pin it to the configured resource host — the only Host that should
    // reach `/mcp`. With no resource configured (static-token dev), disable the
    // Host check and rely on Bearer auth + the Origin check + the reverse
    // proxy's own host routing.
    config = match resource_host(&state.cfg.mcp.resource_uri) {
        Some(host) => config.with_allowed_hosts([host]),
        None => config.disable_allowed_hosts(),
    };
    StreamableHttpService::new(
        move || Ok(McpServer::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

/// Authority (`host` or `host:port`) of the configured MCP resource URI, for
/// the transport's Host allow-list. `None` when unset/unparseable.
fn resource_host(resource_uri: &str) -> Option<String> {
    if resource_uri.trim().is_empty() {
        return None;
    }
    let u = url::Url::parse(resource_uri).ok()?;
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    })
}
