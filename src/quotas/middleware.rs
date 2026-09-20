//! Per-account / per-user rate-limit middleware.
//!
//! Runs *after* the API-token auth middleware (so `AuthContext` is present
//! for the Bearer path) and resolves the subject via the same `CurrentUser`
//! / `CurrentOrg` extractors the handlers use — no duplicated auth logic and
//! no TCP-peer keying. Unauthenticated requests fall through untouched;
//! Caddy applies the per-IP tier to those.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{Method, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use metrics::counter;

use crate::app::AppState;
use crate::metric_names;
use crate::quotas::ratelimit::{Denied, RateLimitCategory, RateLimitKey};
use crate::quotas::service::record_quota_event;
use crate::request::auth::{CurrentOrg, CurrentUser};

fn categorize(parts: &Parts) -> RateLimitCategory {
    let path = parts.uri.path();
    // MCP is JSON-RPC over POST — the tool name isn't in the URL, so the call
    // is bucketed as a read here. Write and probe-spawning tools (e.g.
    // run_check_now) enforce their own stricter category at the tool layer.
    if path == "/mcp" || path.starts_with("/mcp/") {
        return RateLimitCategory::ApiReads;
    }
    if path.contains("/bulk") {
        return RateLimitCategory::BulkOps;
    }
    if path.ends_with("/test") {
        return RateLimitCategory::TestNow;
    }
    if path.ends_with("/check-now") {
        return RateLimitCategory::CheckNow;
    }
    if path.ends_with("/support") {
        return RateLimitCategory::Support;
    }
    match parts.method {
        Method::GET | Method::HEAD | Method::OPTIONS => RateLimitCategory::ApiReads,
        _ => RateLimitCategory::ApiWrites,
    }
}

pub async fn rate_limit_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = req.into_parts();

    // Resolve the authenticated subject with the real extractors. A failure
    // means unauthenticated (or no org) — pass through; Caddy rate-limits
    // those per-IP. Never reject here.
    let user = CurrentUser::from_request_parts(&mut parts, &state)
        .await
        .ok()
        .map(|CurrentUser(u)| u);
    let org = CurrentOrg::from_request_parts(&mut parts, &state)
        .await
        .ok()
        .map(|CurrentOrg(o)| o);

    if user.is_none() && org.is_none() {
        return next.run(Request::from_parts(parts, body)).await;
    }

    let category = categorize(&parts);

    // Plan drives the limits. Resolve via the org when known; fall back to
    // the user's effective plan otherwise. A resolution error must not block
    // traffic — degrade to "no app-side limit" and let Caddy hold the line.
    let plan = match org {
        Some(o) => state.quotas.limit_for_org(o).await.ok(),
        None => None,
    };
    let Some(plan) = plan else {
        return next.run(Request::from_parts(parts, body)).await;
    };

    // Per-account first (protects shared resources), then per-user. Whichever
    // is hit first produces the 429. The account tier is keyed on the account
    // rather than the org so the plan's budget stays one budget however many
    // workspaces the customer holds — the same reason resource caps pool.
    let account = match org {
        Some(o) => state.quotas.account_for_org(o).await.ok().flatten(),
        None => None,
    };
    if let Some(a) = account
        && let Err(d) =
            state
                .rate_limits
                .check(RateLimitKey::Account(a, category), "per_account", &plan)
    {
        record_quota_event(
            state.db.clone(),
            org,
            user,
            "rate_limited",
            None,
            serde_json::json!({ "scope": d.scope }),
            None,
        );
        return denied_response(d);
    }
    if let Some(u) = user
        && let Err(d) = state
            .rate_limits
            .check(RateLimitKey::User(u, category), "per_user", &plan)
    {
        record_quota_event(
            state.db.clone(),
            org,
            Some(u),
            "rate_limited",
            None,
            serde_json::json!({ "scope": d.scope }),
            None,
        );
        return denied_response(d);
    }

    next.run(Request::from_parts(parts, body)).await
}

/// Wraps a `Denied` into a 429 response and increments the rate-limit
/// drop counter on the way out. Counter is labelled by the same `scope`
/// string carried in the error body so a dashboard can join the two —
/// label set is bounded (2 tiers × 6 categories = 12 series).
fn denied_response(d: Denied) -> Response {
    counter!(metric_names::RATELIMIT_DROPS, "scope" => d.scope.clone()).increment(1);
    crate::error::AppError::RateLimited {
        scope: d.scope,
        retry_after_secs: d.retry_after_secs,
    }
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts(method: &str, uri: &str) -> Parts {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[test]
    fn categorize_maps_each_endpoint_to_its_bucket() {
        use RateLimitCategory::*;
        // The /bulk, /test, /check-now suffixes win over the method so a
        // bursty POST /targets/bulk can't drain the steady api_writes budget.
        assert_eq!(categorize(&parts("POST", "/api/v1/targets/bulk")), BulkOps);
        assert_eq!(
            categorize(&parts("POST", "/api/v1/targets/bulk-action")),
            BulkOps
        );
        assert_eq!(
            categorize(&parts("POST", "/api/v1/targets/abc/test")),
            TestNow
        );
        // The registered outbound-send primitives (ad-hoc target probe +
        // both channel tests) must stay in the test_now bucket, not the
        // api_writes one.
        assert_eq!(categorize(&parts("POST", "/api/v1/targets/test")), TestNow);
        assert_eq!(
            categorize(&parts("POST", "/api/v1/notification-channels/abc/test")),
            TestNow
        );
        assert_eq!(
            categorize(&parts("POST", "/api/v1/notification-channels/test")),
            TestNow
        );
        assert_eq!(
            categorize(&parts("POST", "/api/v1/targets/abc/check-now")),
            CheckNow
        );
        // The help relay gets its own bucket, not the shared api_writes one:
        // its ceiling is fixed and far tighter than any plan's write budget.
        assert_eq!(categorize(&parts("POST", "/api/v1/support")), Support);
        // Method classifies everything else.
        assert_eq!(categorize(&parts("GET", "/api/v1/targets")), ApiReads);
        assert_eq!(categorize(&parts("HEAD", "/api/v1/targets")), ApiReads);
        assert_eq!(categorize(&parts("OPTIONS", "/api/v1/targets")), ApiReads);
        assert_eq!(categorize(&parts("POST", "/api/v1/targets")), ApiWrites);
        assert_eq!(
            categorize(&parts("PATCH", "/api/v1/targets/abc")),
            ApiWrites
        );
        assert_eq!(
            categorize(&parts("DELETE", "/api/v1/targets/abc")),
            ApiWrites
        );
    }
}
