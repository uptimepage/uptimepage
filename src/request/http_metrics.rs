//! Per-route HTTP metrics middleware.
//!
//! Route label is `MatchedPath` — the path-pattern, not the concrete URL —
//! so cardinality is bounded by the router's static route table. Method is
//! clamped to canonical verbs and status is bucketed by class; neither can
//! be used as a cardinality vector by an attacker spraying junk values.
//!
//! `/healthz` + `/readyz` short-circuit. Caddy active-health polls them in
//! a tight loop and would otherwise dominate every SLO ratio — same
//! suppression as access_log + the trace span factory.
//!
//! Duration is measured up to response head, not body completion. No
//! streaming routes today; if one is added, the metric measures
//! head-build latency, not wall time the client experiences.

use std::sync::LazyLock;
use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use metrics::{Gauge, counter, gauge, histogram};

use super::is_health_path;
use crate::metric_names;

static INFLIGHT: LazyLock<Gauge> = LazyLock::new(|| gauge!(metric_names::HTTP_RESPONSES_INFLIGHT));

/// RAII so the gauge tracks `?`-propagation and early-return paths.
/// Release builds set `panic = "abort"`, so a panicking handler restarts
/// the process and the gauge resets — Drop covers the non-panic paths.
struct InflightGuard;

impl InflightGuard {
    fn new() -> Self {
        INFLIGHT.increment(1.0);
        Self
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT.decrement(1.0);
    }
}

pub async fn middleware(req: Request, next: Next) -> Response {
    if is_health_path(req.uri().path()) {
        return next.run(req).await;
    }

    let method = method_label(req.method());
    // Fallback (non-matched) requests carry no MatchedPath and collapse
    // to one `<unmatched>` series — bot-scan noise stays bounded.
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "<unmatched>".to_string());

    let _guard = InflightGuard::new();
    let started = Instant::now();
    let response = next.run(req).await;
    // `as_secs_f64() * 1000.0` keeps sub-ms precision; `as_millis()`
    // would floor every fast handler to the zero bucket.
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let status = status_class(response.status());

    histogram!(
        metric_names::HTTP_REQUEST_DURATION_MS,
        "method" => method,
        "route" => route.clone(),
    )
    .record(elapsed_ms);
    counter!(
        metric_names::HTTP_REQUESTS_TOTAL,
        "method" => method,
        "route" => route,
        "status" => status,
    )
    .increment(1);

    response
}

/// Clamps to a fixed verb set so `Method::from_bytes(b"AAAA…")` reaching
/// an `any(handler)` route can't open new label series.
fn method_label(method: &Method) -> &'static str {
    match method.as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        _ => "other",
    }
}

fn status_class(status: StatusCode) -> &'static str {
    match status.as_u16() / 100 {
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        5 => "5xx",
        _ => "other",
    }
}

/// Both default-deny fences emit this: `RouteByHost` when marketing is wired
/// up, `host_isolation` otherwise. It lives here rather than at either denial
/// site because `tests/marketing_coupling_test.rs` allows the marketing module
/// to name only `crate::{marketing, templates, security, request,
/// http_outbound}`.
pub(crate) fn record_unrecognised_host() {
    counter!(metric_names::UNRECOGNISED_HOST_REQUESTS).increment(1);
}

#[cfg(test)]
mod tests {
    use super::{method_label, status_class};
    use axum::http::{Method, StatusCode};

    #[test]
    fn status_class_buckets_by_hundreds() {
        assert_eq!(status_class(StatusCode::OK), "2xx");
        assert_eq!(status_class(StatusCode::CREATED), "2xx");
        assert_eq!(status_class(StatusCode::FOUND), "3xx");
        assert_eq!(status_class(StatusCode::BAD_REQUEST), "4xx");
        assert_eq!(status_class(StatusCode::NOT_FOUND), "4xx");
        assert_eq!(status_class(StatusCode::INTERNAL_SERVER_ERROR), "5xx");
        assert_eq!(status_class(StatusCode::SERVICE_UNAVAILABLE), "5xx");
        assert_eq!(status_class(StatusCode::CONTINUE), "other");
    }

    #[test]
    fn method_label_clamps_to_canonical_verbs() {
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(method_label(&Method::POST), "POST");
        assert_eq!(method_label(&Method::PUT), "PUT");
        assert_eq!(method_label(&Method::PATCH), "PATCH");
        assert_eq!(method_label(&Method::DELETE), "DELETE");
        assert_eq!(method_label(&Method::HEAD), "HEAD");
        assert_eq!(method_label(&Method::OPTIONS), "OPTIONS");
    }

    #[test]
    fn method_label_buckets_unknown_verbs_under_other() {
        let custom = Method::from_bytes(b"PROPFIND").expect("valid token");
        assert_eq!(method_label(&custom), "other");
        let fuzz = Method::from_bytes(b"AAAAAAAA").expect("valid token");
        assert_eq!(method_label(&fuzz), "other");
    }
}
