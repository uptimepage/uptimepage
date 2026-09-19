//! The single dispatch seam. One `tower::Service` that routes a request
//! to the marketing router or the app router based on `Host`. The seam
//! exists in code as exactly two things: this service and the one
//! `marketing::router(...)` call in `main.rs`. Anything else is a
//! coupling violation.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::http::header::HOST;
use axum::http::{Request, Response};
use tower::Service;

use crate::request::host::{HostClass, HostScheme, classify_host};
use crate::request::is_health_path;

/// Routes a request to one of two `axum::Router`s based on classified
/// `Host`. `Marketing` and `Unknown` go to the marketing router (so
/// garbage hosts get a marketing 404 — they must NOT fall through to a
/// tenant); `App` and `TenantPublic` go to the app router which already
/// does per-host org resolution. `/healthz` and `/readyz` short-circuit
/// to the app router regardless of `Host` so opaque-Host probes (Caddy
/// active health check, Docker healthcheck) can't mark the upstream
/// down by hitting the marketing 404.
#[derive(Clone)]
pub struct RouteByHost {
    pub scheme: HostScheme,
    pub marketing: Router,
    pub app: Router,
}

impl Service<Request<Body>> for RouteByHost {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Probes use opaque Hosts (IP / container name → Unknown →
        // marketing 404 → upstream marked down). Path is the stable
        // signal; route health endpoints to the app regardless of Host.
        if is_health_path(req.uri().path()) {
            let mut svc = self.app.clone();
            return Box::pin(async move { svc.call(req).await });
        }
        let host = req
            .headers()
            .get(HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let class = classify_host(host, &self.scheme);
        let mut svc = match class {
            HostClass::Marketing | HostClass::Unknown => self.marketing.clone(),
            HostClass::App | HostClass::TenantPublic => self.app.clone(),
        };
        Box::pin(async move { svc.call(req).await })
    }
}
