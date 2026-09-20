//! Apex-wildcard isolation: with the flat status host
//! (`{slug}.{base_domain}`), the operator surface lives at `app.{base}` and
//! every tenant page at `{slug}.{base}` — the same parent label. A session
//! cookie that carries an apex `Domain=` would be sent by the browser to
//! every tenant subdomain, leaking operator identity into public-page
//! requests.
//!
//! Two complementary guards live here:
//!
//! * Boot-time: `assert_cookie_scope_safe` panics if `cookie_domain` covers
//!   the public surface base. The flatten work makes this assertion strictly
//!   more conservative (the parent is now `uptimepage.dev`, not
//!   `status.uptimepage.dev`).
//! * Runtime: with the safe default (`cookie_domain = ""`), every session
//!   cookie minted by the operator router has no `Domain=` attribute, so the
//!   browser scopes it to the exact request host.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use uptimepage::auth::session;
use uptimepage::config::SessionConfig;

/// Test base domain — mirrors the SaaS layout where the operator host
/// (`app.test.local`) and every public page (`{slug}.test.local`) share
/// `test.local` as their parent.
const BASE_DOMAIN: &str = "test.local";

fn saas_subdomain(cfg: &mut uptimepage::config::AppConfig) {
    cfg.tenancy.subdomain_public_routes = true;
    cfg.tenancy.path_based_public_routes = false;
    cfg.public_status.base_domain = BASE_DOMAIN.into();
    cfg.auth.session.cookie_domain = String::new();
}

/// Apex `Domain=` (or any parent of the wildcard) trips the boot guard.
/// Belongs here because the new shape *broadens* the parent and would
/// otherwise silently widen the cookie scope.
#[test]
fn apex_cookie_domain_refused_at_boot() {
    let mut cfg = uptimepage::config::AppConfig::load().expect("config");
    saas_subdomain(&mut cfg);
    cfg.auth.session.cookie_domain = format!(".{BASE_DOMAIN}");
    let panicked = std::panic::catch_unwind(|| cfg.assert_cookie_scope_safe());
    assert!(
        panicked.is_err(),
        "apex cookie_domain must panic so the operator session can't ride into tenant pages"
    );
}

/// Host-only cookie scope: with `cookie_domain` left empty, the rendered
/// `Set-Cookie` carries no `Domain=` attribute. The browser then scopes to
/// the exact host that minted the cookie (`app.{base}`), so a request to
/// `acme.{base}` ships nothing.
#[test]
fn session_cookie_has_no_domain_attribute_by_default() {
    let cfg = SessionConfig::default();
    assert!(
        cfg.cookie_domain.is_empty(),
        "default cookie_domain must stay empty so the cookie is host-only"
    );
    let cookie = session::build_cookie(&cfg, "tok".into());
    assert!(
        cookie.domain().is_none(),
        "empty cookie_domain must produce no Domain= attribute (cookie was: {cookie})"
    );
    let header = cookie.to_string();
    assert!(
        !header.to_ascii_lowercase().contains("domain="),
        "rendered Set-Cookie must not include Domain= (was: {header})"
    );
}

/// End-to-end smoke: a request to the SaaS-subdomain router that doesn't
/// resolve to a known slug 404s at the host-aware extractor and never
/// returns a session-bearing response. We pass a host that doesn't end in
/// `BASE_DOMAIN` so the parser short-circuits without a DB lookup — the
/// in-memory test rig has no PG handle. The contract documented here is:
/// public-surface request paths do not mint operator session cookies, so
/// even a misconfigured proxy can't fabricate one out of a public hit.
#[tokio::test]
async fn public_surface_request_yields_no_session_set_cookie() {
    let app = common::build_test_app_with_web(saas_subdomain);
    let req = Request::builder()
        .uri("/status")
        .header("host", "ghost.other.example")
        .body(Body::empty())
        .expect("request");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "non-matching host must 404 — public surface does not synthesise sessions"
    );
    for (name, value) in resp.headers().iter() {
        if name.as_str().eq_ignore_ascii_case("set-cookie") {
            let v = value.to_str().unwrap_or("");
            assert!(
                !v.to_ascii_lowercase().contains("_sm_session"),
                "public surface response must not carry a session cookie (got: {v})"
            );
        }
    }
}
