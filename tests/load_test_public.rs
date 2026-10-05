//! Load test harness for the public surface.
//!
//! Marked `#[ignore]` so it never runs in the default `cargo test` pass —
//! the production-target case (5 000 connections × 5 min over a live stack)
//! needs Postgres + ClickHouse and a dedicated host. Run explicitly:
//!
//! ```bash
//! STATUS_LOAD_CONCURRENCY=500 \
//! STATUS_LOAD_DURATION_SECS=10 \
//! STATUS_LOAD_P99_MS=200       \
//!     cargo nextest run --test it --run-ignored only --no-capture -E 'test(/^load_test_public::/)'
//! ```
//!
//! **Scope.** The harness fires concurrent in-process requests via
//! `tower::ServiceExt::oneshot` against `NoopPublicSource`. That means it
//! measures axum + askama render + tokio scheduling overhead — not the
//! Postgres/ClickHouse path that the production p99 budget ultimately
//! cares about. That budget must be validated end-to-end against
//! a deployed stack (e.g. `wrk` against the staging URL); the assertion here
//! is a *regression canary* on the in-process fast path, not the
//! production-budget gate.
//!
//! Traffic mix: 80 % `GET /status`, 20 %
//! `GET /api/public/v1/status`. The harness asserts zero panics, zero
//! transport errors, and a p99 ceiling drawn from `STATUS_LOAD_P99_MS`.

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::build_test_app_with_web;
use tower::ServiceExt;

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[idx.clamp(1, sorted.len()) - 1]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load test — opt-in via `cargo test -- --ignored`"]
async fn public_surface_handles_concurrent_load() {
    let concurrency = env_u64("STATUS_LOAD_CONCURRENCY", 200);
    let duration_secs = env_u64("STATUS_LOAD_DURATION_SECS", 5);
    let p99_budget_ms = env_u64("STATUS_LOAD_P99_MS", 200);

    let app = build_test_app_with_web(|_| {});
    let deadline = Instant::now() + Duration::from_secs(duration_secs);

    // Separate counters so a failure message points at the actual class —
    // status errors (handler returned non-2xx) vs transport errors (poll_ready
    // failed) read very differently when triaging a regression.
    let status_errors = Arc::new(AtomicU64::new(0));
    let transport_errors = Arc::new(AtomicU64::new(0));
    let html_count = Arc::new(AtomicU64::new(0));
    let json_count = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::with_capacity(concurrency as usize);
    for worker in 0..concurrency {
        let app = app.clone();
        let status_errors = status_errors.clone();
        let transport_errors = transport_errors.clone();
        let html_count = html_count.clone();
        let json_count = json_count.clone();
        handles.push(tokio::spawn(async move {
            let mut local: Vec<u64> = Vec::with_capacity(256);
            let mut counter: u64 = worker;
            while Instant::now() < deadline {
                // 80/20 mix — every 5th request hits the JSON endpoint.
                let json = counter.is_multiple_of(5);
                let path = if json {
                    "/api/public/v1/status"
                } else {
                    "/status"
                };
                let started = Instant::now();
                let resp = app
                    .clone()
                    .oneshot(Request::get(path).body(Body::empty()).unwrap())
                    .await;
                let elapsed_us = started.elapsed().as_micros() as u64;
                match resp {
                    Ok(r) if r.status() == StatusCode::OK => {
                        local.push(elapsed_us);
                        if json {
                            json_count.fetch_add(1, Ordering::Relaxed);
                        } else {
                            html_count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(_) => {
                        status_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        transport_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                counter = counter.wrapping_add(1);
            }
            local
        }));
    }

    let mut lat: Vec<u64> = Vec::new();
    let mut worker_panics = 0u64;
    for h in handles {
        match h.await {
            Ok(local) => lat.extend(local),
            Err(_) => worker_panics += 1,
        }
    }
    lat.sort_unstable();
    let p50_us = percentile(&lat, 0.50);
    let p99_us = percentile(&lat, 0.99);
    let html = html_count.load(Ordering::Relaxed);
    let json = json_count.load(Ordering::Relaxed);
    let status_errors = status_errors.load(Ordering::Relaxed);
    let transport_errors = transport_errors.load(Ordering::Relaxed);

    eprintln!(
        "load: {html} html + {json} json ({total} total), \
         status_errors={status_errors}, transport_errors={transport_errors}, \
         worker_panics={worker_panics}, \
         p50={p50_ms:.2}ms p99={p99_ms:.2}ms over {duration_secs}s @ concurrency={concurrency}",
        total = html + json,
        p50_ms = (p50_us as f64) / 1_000.0,
        p99_ms = (p99_us as f64) / 1_000.0,
    );

    assert_eq!(worker_panics, 0, "worker task(s) panicked");
    assert_eq!(status_errors, 0, "non-OK responses from in-process router");
    assert_eq!(transport_errors, 0, "transport-level service errors");
    assert!(html + json > 0, "no requests completed");
    assert!(
        p99_us / 1_000 <= p99_budget_ms,
        "p99 {p99_ms:.2}ms exceeds budget {p99_budget_ms}ms",
        p99_ms = (p99_us as f64) / 1_000.0,
    );
}
