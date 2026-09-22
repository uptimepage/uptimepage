//! Surface and default-deny, driven through the merged app router. No
//! database: the custom-domain path resolves from the snapshot before it
//! reaches a pool.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::util::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::domain::{OrgId, PageRef, StatusPageId};
use uptimepage::request::custom_domains::CustomDomainRow;
use uptimepage::request::host::published_page_origin;

use crate::common::build_test_app_state;

const SERVED: &str = "status.acme.test";

const OPERATOR_PATHS: &[&str] = &[
    "/login",
    "/settings",
    "/settings/account",
    "/api/v1/targets",
    "/dashboard",
];

fn saas_state() -> AppState {
    build_test_app_state(|cfg| {
        cfg.tenancy.path_based_public_routes = false;
        cfg.tenancy.subdomain_public_routes = true;
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.public_base_url = "https://app.example.com".into();
        cfg.marketing.enabled = false;
    })
}

fn row(domain: &str, n: u128, activated: bool) -> CustomDomainRow {
    CustomDomainRow {
        domain: domain.into(),
        page: PageRef {
            page: StatusPageId(uuid::Uuid::from_u128(n)),
            org: OrgId(uuid::Uuid::from_u128(n + 1000)),
        },
        slug: format!("page{n}"),
        activated,
    }
}

fn app_serving(rows: Vec<CustomDomainRow>) -> axum::Router {
    let state = saas_state();
    state.custom_domains.install(rows);
    uptimepage::build_app_router(state, tokio_util::sync::CancellationToken::new())
}

fn host_header(host: &str) -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    h.insert(header::HOST, host.parse().unwrap());
    h
}

async fn status_for(app: &axum::Router, host: &str, path: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router call")
        .status()
}

#[tokio::test]
async fn a_verified_custom_domain_reaches_the_public_surface() {
    let app = app_serving(vec![row(SERVED, 1, true)]);
    for path in ["/", "/status", "/subscribe"] {
        let status = status_for(&app, SERVED, path).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{path} must reach a handler on a served custom domain"
        );
    }
}

#[tokio::test]
async fn a_verified_custom_domain_reaches_nothing_else() {
    let app = app_serving(vec![row(SERVED, 1, true)]);
    for path in OPERATOR_PATHS {
        assert_eq!(
            status_for(&app, SERVED, path).await,
            StatusCode::NOT_FOUND,
            "{path} must not answer on a custom domain"
        );
    }
}

#[tokio::test]
async fn an_unverified_domain_reaches_nothing() {
    let app = app_serving(vec![row(SERVED, 1, true)]);
    for path in ["/", "/status", "/login", "/api/v1/targets"] {
        assert_eq!(
            status_for(&app, "status.unverified.test", path).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn an_unrecognised_host_reaches_nothing() {
    let app = app_serving(Vec::new());
    for path in ["/", "/status", "/login", "/settings", "/api/v1/targets"] {
        assert_eq!(
            status_for(&app, "evil.test", path).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn health_probes_still_answer_on_any_host() {
    let app = app_serving(Vec::new());
    for host in ["evil.test", "uptimepage_blue:8080", "1.2.3.4", SERVED] {
        assert_eq!(
            status_for(&app, host, "/healthz").await,
            StatusCode::OK,
            "{host}"
        );
    }
}

#[tokio::test]
async fn the_operator_and_tenant_hosts_are_unchanged() {
    let app = app_serving(vec![row(SERVED, 1, true)]);
    assert_ne!(
        status_for(&app, "app.example.com", "/login").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status_for(&app, "acme.example.com", "/login").await,
        StatusCode::NOT_FOUND
    );
    assert_ne!(
        status_for(&app, "acme.example.com", "/").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_surface_follows_the_host_header_and_nothing_else() {
    let app = app_serving(vec![row(SERVED, 1, true)]);
    assert_eq!(
        status_for(&app, SERVED, "/login").await,
        StatusCode::NOT_FOUND
    );
    assert_ne!(
        status_for(&app, "app.example.com", "/login").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_row_under_our_own_base_domain_is_refused_on_install() {
    let state = saas_state();
    state.custom_domains.install(vec![
        row("app.example.com", 1, true),
        row("acme.example.com", 2, true),
    ]);
    assert!(state.custom_domains.is_empty());

    let app =
        uptimepage::build_app_router(state.clone(), tokio_util::sync::CancellationToken::new());
    assert_ne!(
        status_for(&app, "app.example.com", "/login").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_path_based_self_host_deploy_keeps_serving_every_host() {
    let state = build_test_app_state(|cfg| {
        cfg.tenancy.path_based_public_routes = true;
        cfg.tenancy.subdomain_public_routes = false;
        cfg.public_status.base_domain = String::new();
    });
    let app = uptimepage::build_app_router(state, tokio_util::sync::CancellationToken::new());
    for host in ["status.internal", "192.168.1.10:8080", "localhost:8080"] {
        assert_ne!(
            status_for(&app, host, "/login").await,
            StatusCode::NOT_FOUND,
            "{host}"
        );
    }
}

#[tokio::test]
async fn published_links_stay_on_the_subdomain_until_activation() {
    let state = saas_state();
    state.custom_domains.install(vec![row(SERVED, 1, false)]);
    let page = StatusPageId(uuid::Uuid::from_u128(1));
    let headers = host_header(SERVED);

    assert_eq!(
        published_page_origin(&state, &headers, page).as_deref(),
        Some("https://page1.example.com")
    );

    state.custom_domains.install(vec![row(SERVED, 1, true)]);
    assert_eq!(
        published_page_origin(&state, &headers, page).as_deref(),
        Some("https://status.acme.test")
    );
}

#[tokio::test]
async fn an_activated_domain_canonicalises_the_subdomain_too() {
    let state = saas_state();
    state.custom_domains.install(vec![row(SERVED, 1, true)]);
    assert_eq!(
        published_page_origin(
            &state,
            &host_header("page1.example.com"),
            StatusPageId(uuid::Uuid::from_u128(1))
        )
        .as_deref(),
        Some("https://status.acme.test")
    );
}

#[tokio::test]
async fn an_empty_snapshot_serves_no_custom_domain() {
    let app = app_serving(Vec::new());
    assert_eq!(status_for(&app, SERVED, "/").await, StatusCode::NOT_FOUND);
}
