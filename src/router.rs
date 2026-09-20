//! Single source of truth for assembling the merged app router
//! (API + web UI) with the cross-cutting layers applied. `main.rs`
//! and every test-harness call site routes through here so a future
//! site can't silently miss CSRF or host isolation.
//!
//! Layer order (outermost first, runs earliest on request):
//!   1. http metrics — records every matched request, including ones
//!      the inner guards subsequently reject
//!   2. host isolation — 404s the operator surface on tenant and MCP hosts
//!   3. CSRF — rejects state-changing requests without the custom header
//!
//! CSRF wraps the *merged* router so any future state-changing route
//! added to `web::routes` is protected without a separate wiring step.

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use tokio_util::sync::CancellationToken;

use crate::app::AppState;
use crate::request::http_metrics;
use crate::{api, request, web};

/// Build the full app router (API + web UI) with the cross-cutting
/// guards applied. `main.rs` and the test harness both route through
/// this function so a future call site can't silently miss either
/// guard.
pub fn build_app_router(state: AppState, shutdown: CancellationToken) -> Router {
    // Purge expired OAuth codes + refresh tokens on a timer (no-op unless the
    // OAuth connector is enabled and a DB is wired).
    if state.cfg.mcp.oauth_enabled
        && let Some(pool) = state.db.clone()
    {
        crate::oauth::spawn_sweeper(pool, shutdown.clone());
    }
    let merged = api::build_router(state.clone(), shutdown).merge(web::routes(state.clone()));
    // MCP server at `/mcp` (no-op unless `cfg.mcp.enabled`). Mounted before the
    // cross-cutting layers so CSRF (Bearer-exempt) and host isolation, which
    // narrows the MCP host to this mount plus discovery, wrap it like the rest.
    let merged = crate::mcp::mount(merged, state.clone());
    apply_cross_cutting_layers(merged, state)
}

/// API-only variant for tests that need to exercise the JSON surface
/// without the HTML routes. Same cross-cutting layers apply so the
/// test surface mirrors production gating exactly.
pub fn build_app_router_api_only(state: AppState, shutdown: CancellationToken) -> Router {
    apply_cross_cutting_layers(api::build_router(state.clone(), shutdown), state)
}

fn apply_cross_cutting_layers(router: Router, state: AppState) -> Router {
    // Last `.layer()` is OUTERMOST in axum — http_metrics runs first
    // (observes every routed request, including ones the guards below
    // subsequently reject), then host_isolation, then CSRF.
    // 404ing a tenant-host operator route still beats running CSRF's
    // constant-time header compare; reordering reverses request semantics.
    router
        .layer(from_fn_with_state(
            state.clone(),
            request::auth::csrf::middleware,
        ))
        .layer(from_fn_with_state(state, request::host::host_isolation))
        .layer(from_fn(http_metrics::middleware))
}
