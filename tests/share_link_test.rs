//! Integration contract for the public share surface (`/m/{token}`).
//!
//! No DB needed — InMemory stores back the router. Requests carry NO session
//! (the router is built without `with_session`), proving the surface needs no
//! login. Covers: the read-only detail/incidents pages render; the check config
//! is shown with credentials redacted to `***`; bad / revoked / expired tokens
//! all 404 (uniform, no enumeration); a token for one monitor never yields
//! another's data; no write method is accepted under `/m/`; the head-less
//! sub-resources carry the crawl directive the page states in its own head; and
//! a status page's host answers only for that page's own detail links.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::custom_domains::CustomDomainRow;
use uptimepage::domain::{
    CheckSpec, CheckStatus, CreatedShare, ExpectedStatus, Incident, IncidentSeverity,
    NewMonitorShare, NewStatusPage, NewStatusPageComponent, NewTarget, OrgId, PageRef,
    StatusPageComponentUpdate, StatusPageId, UserId, WriteSource,
};
use uptimepage::storage::{
    CreateShareOutcome, InMemoryIncidentNarrationStore, MonitorShareStore, TargetStore,
};
use uuid::Uuid;

use common::{build_test_app_state, default_http_check};

const SECRET: &str = "SUPERSECRET-bearer-do-not-leak";

/// Build a web+API router with no session layer, returning the store handles so
/// the test can seed a monitor + share directly and then hit `/m/{token}`
/// unauthenticated.
fn app_with_stores() -> (
    axum::Router,
    Arc<dyn TargetStore>,
    Arc<dyn MonitorShareStore>,
) {
    let (router, targets, shares, _) = app_with_seedable_incidents();
    (router, targets, shares)
}

fn app_with_seedable_incidents() -> (
    axum::Router,
    Arc<dyn TargetStore>,
    Arc<dyn MonitorShareStore>,
    Arc<InMemoryIncidentNarrationStore>,
) {
    let mut state: AppState = build_test_app_state(|_| {});
    let narration = Arc::new(InMemoryIncidentNarrationStore::new());
    state.incident_narration_store = narration.clone();
    let target_store = state.target_store.clone();
    let share_store = state.monitor_share_store.clone();
    let router = uptimepage::build_app_router(state, CancellationToken::new());
    (router, target_store, share_store, narration)
}

async fn make_target(store: &dyn TargetStore, org: OrgId, name: &str, secret: bool) -> Uuid {
    // When `secret`, plant the same marker in every place an HTTP check can hide
    // a credential: bearer token, a custom header value, the body, and the URL
    // query. The public share page must surface none of them.
    let raw_url = if secret {
        format!("https://example.com/health?token={SECRET}")
    } else {
        "https://example.com/".to_string()
    };
    let url = url::Url::parse(&raw_url).unwrap();
    let mut http = default_http_check(url, ExpectedStatus::Exact(200));
    if secret {
        http.bearer_token = Some(SECRET.to_string());
        http.headers
            .insert("X-Api-Key".to_string(), SECRET.to_string());
        http.body = Some(format!("payload={SECRET}"));
    }
    let nt = NewTarget {
        name: name.into(),
        check: CheckSpec::Http(http),
        interval: Duration::from_secs(30),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        recovery_period_secs: None,
        group_name: None,
        owner_user_id: None,
        regions: None,
    };
    store
        .create(org, nt, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .unwrap()
        .id
}

async fn get(router: &axum::Router, path: &str) -> (StatusCode, String) {
    let resp = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn org() -> OrgId {
    OrgId(Uuid::from_u128(0x5ade))
}
fn user() -> UserId {
    UserId(Uuid::from_u128(0x5e7))
}

/// Mint a share with generous caps (these tests don't exercise the plan limits).
async fn mk_share(
    store: &dyn MonitorShareStore,
    org: OrgId,
    target: Uuid,
    new: NewMonitorShare,
) -> CreatedShare {
    match store
        .create(
            org,
            target,
            new,
            Some(user()),
            Some(i64::MAX),
            Some(i64::MAX),
        )
        .await
        .unwrap()
    {
        CreateShareOutcome::Created(c) => c,
        other => panic!("expected Created, got {other:?}"),
    }
}

#[tokio::test]
async fn share_page_renders_read_only_and_redacts_credentials() {
    let (router, targets, shares) = app_with_stores();
    let target = make_target(&*targets, org(), "redact-me", true).await;
    let created = mk_share(&*shares, org(), target, NewMonitorShare::default()).await;

    let (status, body) = get(&router, &format!("/m/{}", created.token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("redact-me"), "monitor name should render");
    assert!(
        body.contains("example.com"),
        "safe host should still render"
    );
    // R1: NO secret — bearer token, header value, body, or URL query — may reach
    // the page; only the sentinel.
    assert!(body.contains("***"), "redaction sentinel present");
    assert!(
        !body.contains(SECRET),
        "no credential (bearer/header/body/url-query) may leak"
    );
    // R2: no operator write controls or nav on the read-only shell.
    assert!(!body.contains("run check now"));
    assert!(!body.contains("hx-delete"));
    assert!(
        !body.contains("data-share-open"),
        "no operator Share button"
    );
    assert!(!body.contains("/targets/"), "no operator monitor links");
    // The page's own sub-resources are token-scoped, never /api/v1.
    assert!(body.contains(&format!("/m/{}/latency", created.token)));
    assert!(!body.contains("/api/v1/targets/"));
}

#[tokio::test]
async fn share_sub_resources_render() {
    let (router, targets, shares) = app_with_stores();
    let target = make_target(&*targets, org(), "subres", false).await;
    let token = mk_share(&*shares, org(), target, NewMonitorShare::default())
        .await
        .token;

    for path in [
        format!("/m/{token}/incidents"),
        format!("/m/{token}/live"),
        format!("/m/{token}/latency"),
        format!("/m/{token}/results"),
    ] {
        let (status, _) = get(&router, &path).await;
        assert_eq!(status, StatusCode::OK, "{path} should be 200");
    }

    // The page says noindex in its head; these three have no head.
    for path in [
        format!("/m/{token}/live"),
        format!("/m/{token}/latency"),
        format!("/m/{token}/results"),
    ] {
        let resp = router
            .clone()
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.headers()
                .get("x-robots-tag")
                .map(|v| v.to_str().unwrap()),
            Some("noindex, nofollow"),
            "{path}"
        );
    }

    // An anonymous, token-scoped view must not sit in a shared cache.
    let live = router
        .oneshot(
            Request::get(format!("/m/{token}/live"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        live.headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap()),
        Some("no-store")
    );
}

/// The incident page sits behind sign-in, so a share reader gets the row and
/// its timeline but no link into it.
#[tokio::test]
async fn share_incidents_rows_do_not_link_to_the_incident_page() {
    let (router, targets, shares, narration) = app_with_seedable_incidents();
    let target = make_target(&*targets, org(), "linkless", false).await;
    let token = mk_share(&*shares, org(), target, NewMonitorShare::default())
        .await
        .token;
    let started = Utc::now() - chrono::Duration::minutes(10);
    let incident = Uuid::now_v7();
    narration.seed(Incident {
        id: incident,
        target_id: Some(target),
        target_name: None,
        started_at: started,
        ended_at: None,
        status: CheckStatus::Down,
        duration_secs: None,
        check_count: 3,
        counts_as_downtime: true,
        error_sample: None,
        severity: IncidentSeverity::Major,
        public_title: None,
        public_description: None,
        created_at: Some(started),
        updated_at: Some(started),
        updates: vec![],
        regions_down: Vec::new(),
        regions_up: Vec::new(),
        origin: Default::default(),
        recovering_since: None,
        recovered: Vec::new(),
    });

    let (status, body) = get(&router, &format!("/m/{token}/incidents")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!(r#"data-incident-id="{incident}""#)),
        "{body}"
    );
    assert!(!body.contains(&format!("/incidents/{incident}")), "{body}");
}

#[tokio::test]
async fn unknown_revoked_and_expired_tokens_all_404() {
    let (router, targets, shares) = app_with_stores();
    let target = make_target(&*targets, org(), "gone", false).await;

    // Unknown token.
    let (status, _) = get(&router, "/m/this-token-does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Revoked token.
    let created = mk_share(&*shares, org(), target, NewMonitorShare::default()).await;
    assert!(
        shares
            .revoke(org(), target, created.share.id, None)
            .await
            .unwrap()
    );
    let (status, _) = get(&router, &format!("/m/{}", created.token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "revoked token must 404");

    // Expired token.
    let expired = NewMonitorShare {
        label: None,
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
    };
    let exp = mk_share(&*shares, org(), target, expired).await;
    let (status, _) = get(&router, &format!("/m/{}", exp.token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "expired token must 404");
    // Sub-resources of a dead token 404 too.
    let (status, _) = get(&router, &format!("/m/{}/latency", exp.token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn token_only_yields_its_own_monitor() {
    let (router, targets, shares) = app_with_stores();
    let a = make_target(&*targets, org(), "monitor-alpha", false).await;
    let _b = make_target(&*targets, org(), "monitor-bravo", false).await;
    let token_a = mk_share(&*shares, org(), a, NewMonitorShare::default())
        .await
        .token;

    let (status, body) = get(&router, &format!("/m/{token_a}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("monitor-alpha"));
    assert!(
        !body.contains("monitor-bravo"),
        "must not leak another monitor"
    );
}

#[tokio::test]
async fn no_write_method_under_m() {
    let (router, targets, shares) = app_with_stores();
    let target = make_target(&*targets, org(), "ro", false).await;
    let token = mk_share(&*shares, org(), target, NewMonitorShare::default())
        .await
        .token;

    let resp = router
        .clone()
        .oneshot(
            Request::post(format!("/m/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // Either CSRF (403, state-changing without the header) or method-not-allowed
    // (405) — never a 2xx. No write handler exists on the share surface.
    assert!(
        matches!(
            resp.status(),
            StatusCode::FORBIDDEN | StatusCode::METHOD_NOT_ALLOWED
        ),
        "POST /m/{{token}} must be rejected, got {}",
        resp.status()
    );
}

const APP_HOST: &str = "app.example.com";
const PAGE_HOST: &str = "status.acme.test";
const OTHER_PAGE_HOST: &str = "status.other.test";

struct PageLinks {
    router: axum::Router,
    state: AppState,
    page: StatusPageId,
    target: Uuid,
    page_link: String,
    other_page_link: String,
    monitor_link: String,
}

/// A SaaS deploy with two pages on custom domains, each showing one monitor's
/// detail link, plus a link minted from the first monitor itself.
async fn saas_page_links() -> PageLinks {
    let state = build_test_app_state(|cfg| {
        cfg.tenancy.path_based_public_routes = false;
        cfg.tenancy.subdomain_public_routes = true;
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.public_base_url = format!("https://{APP_HOST}");
        cfg.marketing.enabled = false;
    });
    let (page, target, page_link) = page_with_link(&state, "acme").await;
    let (other_page, _, other_page_link) = page_with_link(&state, "other").await;
    state.custom_domains.install(vec![
        domain_row(PAGE_HOST, page, "acme"),
        domain_row(OTHER_PAGE_HOST, other_page, "other"),
    ]);
    let monitor_link = mk_share(
        &*state.monitor_share_store,
        org(),
        target,
        NewMonitorShare::default(),
    )
    .await
    .token;
    let router = uptimepage::build_app_router(state.clone(), CancellationToken::new());
    PageLinks {
        router,
        state,
        page,
        target,
        page_link,
        other_page_link,
        monitor_link,
    }
}

fn domain_row(domain: &str, page: StatusPageId, slug: &str) -> CustomDomainRow {
    CustomDomainRow {
        domain: domain.into(),
        page: PageRef { page, org: org() },
        slug: slug.into(),
        activated: true,
    }
}

async fn page_with_link(state: &AppState, slug: &str) -> (StatusPageId, Uuid, String) {
    let target = make_target(&*state.target_store, org(), slug, false).await;
    let page = state
        .status_page_store
        .create(
            org(),
            NewStatusPage {
                slug: slug.into(),
                name: slug.into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    state
        .status_page_store
        .add_component(
            org(),
            page.id,
            NewStatusPageComponent {
                target_id: target,
                public_name: None,
                public_description: None,
                public_group: None,
                sort_order: 0,
                detail_link_enabled: true,
            },
            i64::MAX,
            None,
        )
        .await
        .unwrap();
    let share = mk_share(
        &*state.monitor_share_store,
        org(),
        target,
        NewMonitorShare::default(),
    )
    .await;
    assert!(
        state
            .status_page_store
            .attach_share(org(), page.id, target, None, share.share.id)
            .await
            .unwrap()
    );
    (page.id, target, share.token)
}

async fn get_on(router: &axum::Router, host: &str, path: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(
            Request::get(path)
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_page_host_serves_its_own_detail_link() {
    let links = saas_page_links().await;
    for path in [
        format!("/m/{}", links.page_link),
        format!("/m/{}/incidents", links.page_link),
        format!("/m/{}/live", links.page_link),
        format!("/m/{}/latency", links.page_link),
        format!("/m/{}/results", links.page_link),
    ] {
        let resp = get_on(&links.router, PAGE_HOST, &path).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path} on its page's host");
        assert_eq!(
            resp.headers()[header::CACHE_CONTROL],
            "no-store",
            "{path} must stay out of a page host's shared caches"
        );
    }
}

#[tokio::test]
async fn a_page_host_answers_for_no_other_link() {
    let links = saas_page_links().await;
    for token in [&links.other_page_link, &links.monitor_link] {
        for path in [
            format!("/m/{token}"),
            format!("/m/{token}/incidents"),
            format!("/m/{token}/live"),
            format!("/m/{token}/latency"),
            format!("/m/{token}/results"),
        ] {
            assert_eq!(
                get_on(&links.router, PAGE_HOST, &path).await.status(),
                StatusCode::NOT_FOUND,
                "{path} must not resolve on another page's host"
            );
        }
    }
}

#[tokio::test]
async fn a_page_link_on_the_app_host_moves_to_its_page() {
    let links = saas_page_links().await;
    for path in [
        format!("/m/{}?range=7d", links.page_link),
        format!("/m/{}/incidents", links.page_link),
    ] {
        let resp = get_on(&links.router, APP_HOST, &path).await;
        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT, "{path}");
        assert_eq!(
            resp.headers()[header::LOCATION],
            format!("https://{PAGE_HOST}{path}").as_str()
        );
    }
    // An open tab's polls keep answering where they are.
    assert_eq!(
        get_on(
            &links.router,
            APP_HOST,
            &format!("/m/{}/live", links.page_link)
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        get_on(
            &links.router,
            APP_HOST,
            &format!("/m/{}", links.monitor_link)
        )
        .await
        .status(),
        StatusCode::OK,
        "a link minted from the monitor keeps the app host"
    );
}

#[tokio::test]
async fn unticking_the_detail_link_takes_it_off_the_page_host() {
    let links = saas_page_links().await;
    links
        .state
        .status_page_store
        .update_component(
            org(),
            links.page,
            links.target,
            StatusPageComponentUpdate {
                detail_link_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let path = format!("/m/{}", links.page_link);
    assert_eq!(
        get_on(&links.router, PAGE_HOST, &path).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_on(&links.router, APP_HOST, &path).await.status(),
        StatusCode::OK,
        "the token itself lives on until revoked"
    );
}
