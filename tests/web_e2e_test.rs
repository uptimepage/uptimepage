//! End-to-end coverage for the server-rendered UI.
//!
//! Exercises every web route via tower::ServiceExt::oneshot, asserting
//! status code, content-type, and the structural anchors that the JS
//! layer relies on (HTMX hooks, chart data-endpoints, form data-action,
//! credential redaction sentinels).

use crate::common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::{body_text, build_test_app_with_web, build_test_app_with_web_and_owner};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> axum::Router {
    build_test_app_with_web_and_owner(|_| {})
}

fn html_ct(resp: &axum::http::Response<Body>) -> &str {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

async fn create_heartbeat_target(router: &axum::Router, name: &str) -> String {
    let body = json!({
        "name": name,
        "interval": 60,
        "enabled": true,
        "tags": [],
        "alerts": [],
        "check": { "type": "heartbeat", "period": 300000, "grace": 300000 }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create target");
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["id"].as_str().expect("id").to_string()
}

async fn create_http_target(router: &axum::Router, name: &str) -> String {
    create_http_target_with_alerts(router, name, json!([])).await
}

async fn create_http_target_with_alerts(
    router: &axum::Router,
    name: &str,
    alerts: Value,
) -> String {
    let body = json!({
        "name": name,
        "interval": 60,
        "enabled": true,
        "tags": ["e2e"],
        "alerts": alerts,
        "check": {
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true,
            "basic_auth": ["alice", "s3cret"],
            "bearer_token": "tok-abc"
        }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create target");
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn dashboard_renders_with_kpi_cards_and_chart_anchors() {
    let resp = app()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(html_ct(&resp).starts_with("text/html"));
    let html = body_text(resp).await;
    assert!(html.contains("Dashboard"));
    assert!(html.contains("nothing to watch yet."));
    assert!(html.contains(r#"href="/targets/new""#));
}

/// Docs live on the marketing host, which an app-only deployment does not
/// run, so the app must link out absolutely rather than to its own `/docs`
/// (that path is the Swagger UI here).
#[tokio::test]
async fn app_chrome_links_out_to_the_docs() {
    let resp = app()
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let html = body_text(resp).await;
    // The account popover renders twice (desktop + mobile), plus the footer.
    assert_eq!(
        html.matches(r#"href="https://uptimepage.dev/docs""#)
            .count(),
        3,
        "expected the docs link in both nav popovers and the footer"
    );
    assert!(
        !html.contains(r#"href="/docs""#),
        "a relative /docs on the app host is the Swagger UI, not the docs"
    );
}

#[tokio::test]
async fn dashboard_partial_returns_chrome_free_fragment() {
    let resp = app()
        .oneshot(
            Request::get("/web/partials/dashboard")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(!html.contains("<!doctype html>"));
    assert!(!html.contains("<nav"));
}

#[tokio::test]
async fn targets_list_empty_org_renders_onboarding_card() {
    // Default test app has zero monitors → /targets renders the
    // onboarding empty state instead of filters + table chrome.
    let resp = app()
        .oneshot(Request::get("/targets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("Monitors"));
    assert!(html.contains("nothing to watch yet."));
    assert!(html.contains("add your first monitor"));
    assert!(!html.contains(r#"id="targets-filter""#));
}

#[tokio::test]
async fn targets_list_partial_returns_tbody_only() {
    let resp = app()
        .oneshot(
            Request::get("/web/targets/list?limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(!html.contains("<!doctype html>"));
    assert!(!html.contains("<nav"));
    assert!(html.contains(r#"id="target-rows""#));
}

#[tokio::test]
async fn new_target_form_renders_create_mode() {
    let resp = app()
        .oneshot(Request::get("/targets/new").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("New monitor"));
    assert!(html.contains(r#"data-action="/api/v1/targets""#));
    assert!(html.contains(r#"data-method="POST""#));
    assert!(html.contains(r#"data-mode="create""#));
    assert!(html.contains(r#"name="check_type" value="http" checked"#));
    assert!(html.contains(r#"name="check_type" value="heartbeat""#));
    // Credentials are supplied via headers with secret variables, not inline auth fields.
    assert!(
        html.contains("data-var-auth-picker"),
        "secret-variable auth picker present"
    );
    assert!(
        html.contains("variable_helpers"),
        "insert-variable helper script linked"
    );
}

/// A monitor bound to nothing pages nobody, so a sole channel is ticked. A
/// second one makes the right routing ambiguous.
#[tokio::test]
async fn create_form_ticks_a_sole_channel_but_not_a_pair() {
    let router = app();
    let add_channel = async |name: &str| {
        let resp = router
            .clone()
            .oneshot(
                Request::post("/api/v1/notification-channels")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "name": name,
                            "config": { "type": "email", "to": format!("{name}@example.com") }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "create channel {name}");
    };

    // Scoped to the channel inputs: verify_tls and the interval rail also
    // render checked.
    let channel_boxes = |html: &str| -> (usize, usize) {
        let mut offered = 0;
        let mut ticked = 0;
        for (i, _) in html.match_indices("data-channel-select") {
            let tail = &html[i..];
            let tag = &tail[..tail.find('>').unwrap_or(tail.len())];
            offered += 1;
            ticked += usize::from(tag.contains(" checked"));
        }
        (offered, ticked)
    };

    add_channel("solo").await;
    let resp = router
        .clone()
        .oneshot(Request::get("/targets/new").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        channel_boxes(&body_text(resp).await),
        (1, 1),
        "the only channel is offered and preselected"
    );

    add_channel("second").await;
    let resp = router
        .clone()
        .oneshot(Request::get("/targets/new").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        channel_boxes(&body_text(resp).await),
        (2, 0),
        "with two channels the form must not guess"
    );
}

#[tokio::test]
async fn edit_form_renders_existing_target_without_leaking_credentials() {
    let router = app();
    let id = create_http_target(&router, "redacted-edit-target").await;

    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/targets/{id}/edit"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains(r#"value="redacted-edit-target""#));
    assert!(html.contains(r#"data-method="PATCH""#));
    assert!(html.contains(r#"data-mode="edit""#));
    // The kind is fixed once the monitor exists: rail is inert, nothing to submit with.
    assert!(!html.contains("data-check-card"));
    assert!(html.contains(r#"<input type="hidden" name="check_type" value="http">"#));
    // Stored credentials must never reach the form HTML.
    assert!(!html.contains("s3cret"));
    assert!(!html.contains("tok-abc"));
}

/// The pending state's own surface: an unwired heartbeat must not borrow the
/// red pill from a monitor that actually failed, nor the "checking…" from one
/// that is genuinely being probed.
#[tokio::test]
async fn a_heartbeat_waiting_for_its_first_ping_says_so_on_every_badge() {
    let router = app();
    let id = create_heartbeat_target(&router, "unwired-cron").await;

    for uri in [
        format!("/targets/{id}"),
        format!("/web/partials/targets/{id}/live"),
        format!("/targets/{id}/incidents"),
    ] {
        let resp = router
            .clone()
            .oneshot(Request::get(uri.clone()).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let html = body_text(resp).await;
        assert!(html.contains("waiting for first ping"), "{uri}");
        assert!(
            html.contains("status-badge--pending"),
            "{uri}: grey, not red"
        );
        assert!(!html.contains(">down<"), "{uri}");
        assert!(!html.contains("checking"), "{uri}: nothing is checking it");
    }

    // One ping ends it. No check result has been written yet, so the badge
    // falls back to the ordinary "checking…" a fresh monitor shows.
    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_text(resp).await;
    assert!(html.contains("waiting for first ping"));

    let ping_url = heartbeat_ping_url(&router, &id).await;
    let path = ping_url.split("/ping/").nth(1).expect("ping url shape");
    let resp = router
        .clone()
        .oneshot(
            Request::post(format!("/ping/{path}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = body_text(resp).await;
    assert!(
        !html.contains("waiting for first ping"),
        "the job has spoken"
    );
}

async fn heartbeat_ping_url(router: &axum::Router, id: &str) -> String {
    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}/heartbeat"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = serde_json::from_str(&body_text(resp).await).unwrap();
    assert_eq!(v["pending"], json!(true), "not pinged yet");
    v["ping_url"].as_str().expect("ping url").to_string()
}

#[tokio::test]
async fn target_detail_renders_charts_and_range_nav() {
    let router = app();
    let id = create_http_target(&router, "detail-target").await;

    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/targets/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("detail-target"));
    assert!(html.contains(r#"aria-label="Time range""#));
    for key in ["1h", "24h", "7d", "30d"] {
        assert!(
            html.contains(&format!("?range={key}")),
            "missing range {key}"
        );
    }
    assert!(html.contains(r#"id="latency-chart""#));
    assert!(html.contains(r#"id="breakdown-chart""#));
    assert!(html.contains("/api/v1/targets/"));
    assert!(html.contains("/static/js/charts/detail_charts.js"));
}

/// Gaps are named while they are gaps, and stop being named once watched.
#[tokio::test]
async fn detail_offers_uncovered_checks_for_the_host() {
    let router = app();
    let id = create_http_target(&router, "coverage-target").await;

    let detail = async || {
        let resp = router
            .clone()
            .oneshot(
                Request::get(format!("/targets/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_text(resp).await
    };

    let html = detail().await;
    assert!(html.contains("also worth watching"));
    // Matched up to the query separator: the `&` is escaped in rendered HTML
    // and the exact entity is the templating engine's call.
    for href in [
        r#"href="/targets/new?kind=tls_cert"#,
        r#"href="/targets/new?kind=domain_expiry"#,
        r#"href="/targets/new?kind=dns"#,
    ] {
        assert!(html.contains(href), "missing suggestion {href}");
    }
    assert_eq!(
        html.matches("host=example.com").count(),
        3,
        "every suggestion points at the monitor's host"
    );

    // Cover one of them; the panel drops that row and keeps the rest.
    let cert = json!({
        "name": "cert",
        "interval": 3600,
        "enabled": true,
        "check": {
            "type": "tls_cert",
            "host": "example.com",
            "port": 443,
            "warn_days": 30,
            "critical_days": 7,
            "timeout": 5000
        }
    });
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(cert.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create cert monitor");

    let html = detail().await;
    assert!(
        !html.contains(r#"href="/targets/new?kind=tls_cert"#),
        "a covered check must stop being suggested"
    );
    assert!(html.contains(r#"href="/targets/new?kind=dns"#));
}

#[tokio::test]
async fn create_form_prefills_from_a_coverage_link() {
    let resp = app()
        .oneshot(
            Request::get("/targets/new?kind=domain_expiry&host=acme.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains(r#"value="acme.com""#), "host prefilled");
    assert!(html.contains(r#"value="acme.com domain expiry""#), "named");
    assert!(html.contains(r#"name="check_type" value="domain_expiry" checked"#));
    // Opens on the suggested cadence for the kind, not on the 60s default.
    assert!(html.contains(r#"data-interval="86400""#));

    // A hand-edited kind is ignored rather than rejected.
    let resp = app()
        .oneshot(
            Request::get("/targets/new?kind=nonsense&host=acme.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!body_text(resp).await.contains(r#"value="acme.com""#));
}

#[tokio::test]
async fn nonexistent_target_detail_returns_html_404() {
    let resp = app()
        .oneshot(
            Request::get("/targets/00000000-0000-0000-0000-000000000000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(html_ct(&resp).starts_with("text/html"));
    let html = body_text(resp).await;
    assert!(html.contains("Not Found"));
}

#[tokio::test]
async fn unknown_web_path_returns_404_html() {
    let resp = app()
        .oneshot(
            Request::get("/this-route-does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Cache-control is only `immutable` when the URL is version-pinned (`?v=`,
/// the only form the `asset` filter emits) to these bytes. A bare URL
/// (hand-typed or an old bookmark) gets a short revalidating cache so a
/// content change can't be hidden for a year, and a `v` naming other bytes is
/// never stored. This is the e2e mirror of the `templates::assets` unit tests;
/// the two must not disagree.
#[tokio::test]
async fn static_assets_cache_control_is_honest() {
    let cache_control = |resp: &axum::http::Response<Body>| {
        resp.headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };

    let bare = app()
        .oneshot(
            Request::get("/static/css/app.css")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::OK);
    assert_eq!(
        cache_control(&bare),
        "public, max-age=300",
        "bare asset URL must be short-lived, not immutable",
    );

    let versioned = app()
        .oneshot(
            Request::get(uptimepage::templates::assets::url("css/app.css"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(versioned.status(), StatusCode::OK);
    assert_eq!(
        cache_control(&versioned),
        "public, max-age=31536000, immutable",
        "version-pinned asset URL must be immutable",
    );

    let stale = app()
        .oneshot(
            Request::get("/static/css/app.css?v=deadbeef")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::OK);
    assert_eq!(
        cache_control(&stale),
        "no-store",
        "a v naming other bytes must never be cached",
    );
}

#[tokio::test]
async fn settings_account_redirects_to_login_when_unauthenticated() {
    let resp = build_test_app_with_web(|_| {})
        .oneshot(
            Request::get("/settings/account")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_redirection(),
        "unauthenticated /settings/account must redirect, got {}",
        resp.status()
    );
    let loc = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(loc.starts_with("/login"), "redirect target was {loc}");
}

/// A signed-in browser that named a destination skips the sign-in form, so the
/// marketing URL box does not bounce a returning visitor through OAuth. Bare
/// `/login` keeps rendering: that is how a second account is reached.
#[tokio::test]
async fn login_honours_a_destination_for_an_already_signed_in_browser() {
    let signed_in = build_test_app_with_web_and_owner(|_| {});
    let resp = signed_in
        .clone()
        .oneshot(
            Request::get("/login?redirect_after=%2Ftargets%2Fnew%3Fkind%3Dhttp%26url%3Dacme.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_redirection(), "got {}", resp.status());
    assert_eq!(
        resp.headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some("/targets/new?kind=http&url=acme.com")
    );

    // An invitation owns its own accept flow, bare /login is how a second
    // account is reached, and every off-site shape stays refused — including
    // the control bytes a browser strips before resolving the URL.
    for query in [
        "",
        "?redirect_after=%2F&invitation=tok",
        "?redirect_after=https%3A%2F%2Fevil.test",
        "?redirect_after=%2F%2Fevil.test",
        "?redirect_after=%2F%5Cevil.test",
        "?redirect_after=%2F%09%2Fevil.test",
        "?redirect_after=%2F%0A%2Fevil.test",
    ] {
        let resp = signed_in
            .clone()
            .oneshot(
                Request::get(format!("/login{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{query}");
    }

    let anon = build_test_app_with_web(|_| {})
        .oneshot(
            Request::get("/login?redirect_after=%2Ftargets%2Fnew")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::OK);
}

/// Operator HTML pages must redirect an anonymous browser to `/login`.
/// The `AuthedBrowser` gate is the single auth model — there is no
/// no-auth pass-through anywhere in the binary.
#[tokio::test]
async fn operator_pages_redirect_to_login_when_unauthenticated_saas() {
    let app = build_test_app_with_web(|_| {});
    for path in [
        "/",
        "/targets",
        "/targets/new",
        "/targets/00000000-0000-0000-0000-000000000000",
        "/targets/00000000-0000-0000-0000-000000000000/edit",
        "/targets/00000000-0000-0000-0000-000000000000/duplicate",
        "/maintenance",
        "/maintenance/new",
        "/maintenance/00000000-0000-0000-0000-000000000000/edit",
        "/web/targets/list",
        "/web/partials/dashboard",
    ] {
        let resp = app
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            resp.status().is_redirection(),
            "unauthenticated {path} must redirect, got {}",
            resp.status()
        );
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            loc.starts_with("/login"),
            "{path} redirect target was {loc}"
        );
    }
}

/// The license bundle is public, renders into the `.legal-doc` shell, and
/// never exposes the authenticated operator nav.
#[tokio::test]
async fn the_license_page_renders_public_without_auth_nav() {
    let resp = app()
        .oneshot(Request::get("/licenses").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(html_ct(&resp).starts_with("text/html"));
    let body = body_text(resp).await;
    assert!(body.contains("<h1>Third-Party Licenses</h1>"));
    assert!(body.contains(r#"<article class="legal-doc">"#));
    assert!(
        !body.contains("hx-post=\"/auth/logout\""),
        "no authenticated nav"
    );
    assert!(
        body.contains("window.smApplyTheme"),
        "paints in the visitor's theme like the sign-in page"
    );
}

/// The operator's policies live wherever `[legal]` points; the app serves
/// none of its own.
#[tokio::test]
async fn the_app_serves_no_policy_pages() {
    for path in ["/terms", "/privacy", "/impressum", "/abuse-policy"] {
        let resp = app()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

/// A link to a policy page the app used to serve lands on the configured one.
#[tokio::test]
async fn a_former_policy_path_redirects_to_the_configured_policy() {
    let app = build_test_app_with_web_and_owner(|cfg| {
        cfg.legal.privacy_url = "https://example.test/privacy".into();
        cfg.legal.privacy_version = "v1".into();
    });
    let resp = app
        .clone()
        .oneshot(Request::get("/privacy").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap(),
        "https://example.test/privacy"
    );
    let resp = app
        .oneshot(Request::get("/terms").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "terms_url is not set");
}

/// The footer links the operator's own policies, and only those it publishes.
#[tokio::test]
async fn the_footer_links_only_the_configured_policies() {
    let app = build_test_app_with_web_and_owner(|cfg| {
        cfg.legal.terms_url = "https://example.test/terms".into();
        cfg.legal.terms_version = "v7".into();
        cfg.legal.abuse_url = "https://example.test/abuse".into();
    });
    let resp = app
        .oneshot(Request::get("/licenses").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = body_text(resp).await;
    assert!(
        body.contains(r#"href="https://example.test/terms" class="hover:text-body">Terms</a>"#)
    );
    assert!(
        body.contains(r#"href="https://example.test/abuse" class="hover:text-body">Abuse</a>"#)
    );
    for unset in [
        ">Privacy</a>",
        ">Refunds</a>",
        ">Cookies</a>",
        ">Impressum</a>",
        ">Security</a>",
    ] {
        assert!(!body.contains(unset), "{unset} is not configured");
    }
    assert!(
        body.contains(r#"href="/licenses""#),
        "the license bundle always stays"
    );
}

/// The channel edit page splits monitors by alert binding: bound ones are
/// linked in the used-by grid, unbound ones appear only as bind-picker
/// buttons.
#[tokio::test]
async fn channel_edit_page_lists_bound_monitors() {
    let router = app();

    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/v1/notification-channels")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "name": "ops-slack",
                        "config": {
                            "type": "slack",
                            "webhook_url": "https://hooks.slack.com/services/T/B/x"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create channel");
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let ch_id = v["id"].as_str().expect("id").to_string();

    let bound_id =
        create_http_target_with_alerts(&router, "bound-api", json!([{ "channel_id": ch_id }]))
            .await;
    let unbound_id = create_http_target(&router, "unbound-api").await;

    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/settings/notifications/{ch_id}/edit").as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("bound-api"), "bound monitor must be listed");
    assert!(
        html.contains(format!(r#"href="/targets/{bound_id}/edit""#).as_str()),
        "bound monitor must link to its edit form"
    );
    assert!(
        html.contains(format!(r#"data-bind-monitor data-target-id="{unbound_id}""#).as_str()),
        "unbound monitor must be offered by the bind picker"
    );
    assert!(
        html.contains("unbound-api"),
        "picker cards must show the monitor name"
    );
    assert!(
        !html.contains(format!(r#"href="/targets/{unbound_id}/edit""#).as_str()),
        "unbound monitor must not appear in the used-by grid"
    );
    assert!(html.contains("# bound to"), "header shows the bound count");
}

#[tokio::test]
async fn variables_page_and_partial_render_with_secret_redacted() {
    let router = app();

    let resp = router
        .clone()
        .oneshot(
            Request::get("/settings/variables")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains("add variable"), "create form must render");

    for body in [
        json!({ "key": "base_url", "is_secret": false, "value": "api.example.com" }),
        json!({ "key": "api_key", "is_secret": true, "value": "sk-super-secret-9f3a" }),
    ] {
        let resp = router
            .clone()
            .oneshot(
                Request::post("/api/v1/variables")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "create variable");
    }

    let resp = router
        .clone()
        .oneshot(
            Request::get("/web/partials/settings/variables")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(
        html.contains("base_url") && html.contains("api.example.com"),
        "plain value visible"
    );
    assert!(html.contains("api_key"), "secret key listed");
    assert!(
        !html.contains("sk-super-secret-9f3a"),
        "secret value must never reach the page"
    );
}

/// RFC 9116: served at the well-known path as `text/plain`, carrying the
/// operator's contact and an `Expires`.
#[tokio::test]
async fn security_txt_served_per_rfc9116() {
    let app = build_test_app_with_web_and_owner(|cfg| {
        cfg.security.disclosure.contact = vec!["mailto:security@example.test".into()];
    });
    let resp = app
        .oneshot(
            Request::get("/.well-known/security.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/plain"), "got {ct}");
    let body = body_text(resp).await;
    assert!(
        body.contains("Contact: mailto:security@example.test"),
        "{body}"
    );
    assert!(body.contains("Expires: "), "{body}");
}

/// With no contact configured there is nobody to report to, so a deployment
/// does not claim one, least of all somebody else's.
#[tokio::test]
async fn no_security_txt_without_a_contact() {
    let resp = app()
        .oneshot(
            Request::get("/.well-known/security.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn channel_form_gates_one_tap_telegram_on_central_bot() {
    // Default config has no bot token: only the BYO "telegram bot" card.
    let resp = app()
        .oneshot(
            Request::get("/settings/notifications/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(!html.contains(r#"value="telegram_app""#));
    assert!(html.contains("telegram bot"));

    let with_bot = build_test_app_with_web_and_owner(|cfg| {
        cfg.telegram.bot_token = "123:abc".to_string().into();
        cfg.telegram.bot_username = "uptimepagebot".into();
        cfg.telegram.webhook_secret = "0123456789abcdef0123456789abcdef".to_string().into();
    });
    let resp = with_bot
        .oneshot(
            Request::get("/settings/notifications/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains(r#"value="telegram_app""#), "one-tap card");
    assert!(html.contains("data-tga-connect"), "connect button");
    assert!(html.contains("data-tga-group"), "group destination toggle");
    assert!(html.contains("one-tap chat link"));
}

/// The host serving the API points agents at the catalog rather than 404ing
/// on the path they are specified to look at.
#[tokio::test]
async fn api_catalog_redirects_to_the_site() {
    let app = build_test_app_with_web_and_owner(|cfg| {
        cfg.site.origin = "https://example.test".into();
    });
    let resp = app
        .oneshot(
            Request::get("/.well-known/api-catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap(),
        "https://example.test/.well-known/api-catalog"
    );
}

/// A heartbeat can be pending and still have stored results: switching an http
/// monitor to `kind = heartbeat` keeps its rows, and pre-gate heartbeats were
/// evaluated before anything pinged them. Every surface has to read that the
/// same way, or the page renders one answer and the first live poll swaps it.
#[tokio::test]
async fn a_pending_heartbeat_with_results_shows_them_on_every_surface() {
    let state = common::build_test_app_state(|_| {});
    let sink = state.result_sink.clone();
    let router = common::with_session(
        uptimepage::build_app_router(state, tokio_util::sync::CancellationToken::new()),
        common::test_user_id(),
        Some(common::test_org_id()),
        Some("test-owner-session"),
    );

    let id = create_heartbeat_target(&router, "switched-over").await;
    sink.write_batch(&[uptimepage::domain::CheckResult {
        target_id: id.parse().expect("target id"),
        org_id: common::test_org_id().0,
        timestamp: chrono::Utc::now(),
        status: uptimepage::domain::CheckStatus::Up,
        duration_ms: 12,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        network: None,
        diagnostic: None,
        error: None,
        details: None,
    }])
    .await
    .expect("seed a stored result");

    for uri in [
        format!("/targets/{id}"),
        format!("/web/partials/targets/{id}/live"),
        format!("/targets/{id}/incidents"),
    ] {
        let resp = router
            .clone()
            .oneshot(Request::get(uri.clone()).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let html = body_text(resp).await;
        // The notice still belongs here — unwired means nothing watches it from
        // now on — but the badge reports the state actually on record.
        assert!(
            html.contains("status-badge--up"),
            "{uri}: stored results decide the badge"
        );
        assert!(
            !html.contains("status-badge--pending"),
            "{uri}: renders one badge, not one per surface"
        );
    }
}

/// The monitors list reads a 30-day rollup at hour grain, so "Last check" has
/// to come from the newest raw result, not the start of that result's bucket.
#[tokio::test]
async fn monitors_list_shows_the_last_check_to_the_second() {
    use chrono::{DurationRound, TimeDelta};

    let state = common::build_test_app_state(|_| {});
    let sink = state.result_sink.clone();
    let router = common::with_session(
        uptimepage::build_app_router(state, tokio_util::sync::CancellationToken::new()),
        common::test_user_id(),
        Some(common::test_org_id()),
        Some("test-owner-session"),
    );

    let id = create_http_target(&router, "last-check").await;
    let checked_at = chrono::Utc::now()
        .duration_trunc(TimeDelta::minutes(1))
        .unwrap()
        - TimeDelta::seconds(97);
    sink.write_batch(&[uptimepage::domain::CheckResult {
        target_id: id.parse().expect("target id"),
        org_id: common::test_org_id().0,
        timestamp: checked_at,
        status: uptimepage::domain::CheckStatus::Up,
        duration_ms: 12,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: None,
        network: None,
        diagnostic: None,
        error: None,
        details: None,
    }])
    .await
    .expect("seed a stored result");

    let resp = router
        .oneshot(Request::get("/targets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    let expected = format!("datetime=\"{}\"", checked_at.format("%Y-%m-%dT%H:%M:%SZ"));
    assert!(html.contains(&expected), "last check renders {expected}");
}

/// The ribbon drill drawer is where an operator reads a failure's network path.
#[tokio::test]
async fn the_check_drawer_shows_the_network_path_of_a_failed_check() {
    use uptimepage::net::server_names::{Dns, LookupFailed, ServerNames};

    struct NamedEdge;

    #[async_trait::async_trait]
    impl Dns for NamedEdge {
        async fn reverse(
            &self,
            _: std::net::IpAddr,
        ) -> Result<Vec<hickory_resolver::proto::rr::Name>, LookupFailed> {
            Ok(vec!["edge.example.net.".parse().unwrap()])
        }
        async fn forward(
            &self,
            _: &hickory_resolver::proto::rr::Name,
        ) -> Result<Vec<std::net::IpAddr>, LookupFailed> {
            Ok(vec!["2001:db8::53".parse().unwrap()])
        }
    }

    let mut state = common::build_test_app_state(|_| {});
    state.server_names = std::sync::Arc::new(ServerNames::new(std::sync::Arc::new(NamedEdge)));
    let sink = state.result_sink.clone();
    let router = common::with_session(
        uptimepage::build_app_router(state, tokio_util::sync::CancellationToken::new()),
        common::test_user_id(),
        Some(common::test_org_id()),
        Some("test-owner-session"),
    );

    let id = create_http_target(&router, "moved-host").await;
    let at = chrono::Utc::now() - chrono::Duration::minutes(5);
    sink.write_batch(&[uptimepage::domain::CheckResult {
        target_id: id.parse().expect("target id"),
        org_id: common::test_org_id().0,
        timestamp: at,
        status: uptimepage::domain::CheckStatus::Down,
        duration_ms: 6,
        dns_ms: Some(1),
        connect_ms: Some(1),
        tls_ms: None,
        ttfb_ms: Some(2),
        response_code: Some(404),
        response_size: Some(6305),
        network: Some(uptimepage::domain::NetworkPath {
            remote_ip: Some("2001:db8::53".parse().unwrap()),
            tried_ips: vec![],
            hop_url: Some("https://example.com/next".into()),
            dns: Some(uptimepage::domain::DnsAnswer {
                resolver: "192.0.2.53".parse().unwrap(),
                addresses: vec!["2001:db8::53".parse().unwrap()],
                ttl_secs: 2340,
            }),
        }),
        diagnostic: None,
        error: Some("unexpected status 404".into()),
        details: None,
    }])
    .await
    .expect("seed a failed result");

    let from =
        (at - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let to = (at + chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let uri = format!("/web/partials/targets/{id}/checks?from={from}&to={to}");
    let resp = router
        .oneshot(Request::get(uri.clone()).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let html = body_text(resp).await;
    assert!(html.contains("server IP:"), "{html}");
    assert!(html.contains("2001:db8::53"), "{html}");
    assert!(html.contains("(edge.example.net)"), "{html}");
    assert!(html.contains("final URL:"), "{html}");
    assert!(html.contains("dns 192.0.2.53:"), "{html}");
    assert!(html.contains("cached until"), "{html}");
}
