//! Integration contract for the heartbeat surface. InMemory-backed, no DB.
//! Ping requests carry no session or CSRF header, proving bare curl/cron works;
//! the rotation routes below are the authenticated half, driven over HTTP so
//! the URL a caller is handed is the one the inbound route accepts.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::domain::{CheckSpec, HeartbeatCheck, NewTarget, OrgId, WriteSource};
use uuid::Uuid;

use common::build_test_app_state;

#[tokio::test]
async fn ping_accepts_get_and_post_and_advances_the_anchor() {
    let state = build_test_app_state(|_| {});
    let org = OrgId(Uuid::new_v4());
    let target_id = Uuid::new_v4();
    let hb = state
        .heartbeat_store
        .ensure(org, target_id)
        .await
        .unwrap()
        .unwrap();
    let token = hb.token.expect("in-memory store returns the raw token");
    let runtime = state.heartbeat_runtime.clone();
    let router = uptimepage::build_app_router(state, CancellationToken::new());

    assert!(runtime.state(target_id).is_none());
    for method in [Method::GET, Method::POST] {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(format!("/ping/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{method} must count");
    }
    let anchor = runtime
        .state(target_id)
        .expect("ping set the state")
        .success_at;
    assert!(
        chrono::Utc::now()
            .signed_duration_since(anchor)
            .num_seconds()
            < 5
    );
}

/// `/start` opens a run without holding the monitor up, `/fail` and a nonzero
/// exit fail it outright, a later success clears that.
#[tokio::test]
async fn signals_report_the_run_the_bare_url_cannot() {
    let state = build_test_app_state(|_| {});
    let org = OrgId(Uuid::new_v4());
    let target_id = Uuid::new_v4();
    let token = state
        .heartbeat_store
        .ensure(org, target_id)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    let runtime = state.heartbeat_runtime.clone();
    let router = uptimepage::build_app_router(state, CancellationToken::new());

    let send = |path: String, body: &'static str| {
        let router = router.clone();
        async move {
            router
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };

    assert_eq!(
        send(format!("/ping/{token}/start"), "").await,
        StatusCode::OK
    );
    let state_after_start = runtime.state(target_id).expect("start recorded");
    assert!(state_after_start.run_open_since().is_some());
    assert!(
        state_after_start.failing().is_none(),
        "a start is not a verdict"
    );

    assert_eq!(
        send(format!("/ping/{token}/137"), "OOM killed").await,
        StatusCode::OK
    );
    let failed = runtime.state(target_id).expect("fail recorded");
    assert_eq!(failed.failing().and_then(|f| f.exit_code), Some(137));
    assert!(failed.run_open_since().is_none(), "the fail closed the run");

    assert_eq!(send(format!("/ping/{token}/0"), "").await, StatusCode::OK);
    assert!(
        runtime.state(target_id).unwrap().failing().is_none(),
        "exit 0 is a success and clears the failure"
    );

    // A word we don't know must not be mistaken for a success.
    assert_eq!(
        send(format!("/ping/{token}/probably"), "").await,
        StatusCode::NOT_FOUND
    );
}

/// The pending gate's exit: the first ping through the real route is what
/// wires a monitor up, whatever signal it carries.
#[tokio::test]
async fn the_first_ping_through_the_route_wires_the_monitor_up() {
    for signal in ["", "/start", "/fail", "/1", "/0"] {
        let state = build_test_app_state(|_| {});
        let org = OrgId(Uuid::new_v4());
        let target_id = Uuid::new_v4();
        let token = state
            .heartbeat_store
            .ensure(org, target_id)
            .await
            .unwrap()
            .unwrap()
            .token
            .unwrap();
        let store = state.heartbeat_store.clone();
        let router = uptimepage::build_app_router(state, CancellationToken::new());

        assert!(
            store
                .get(org, target_id)
                .await
                .unwrap()
                .unwrap()
                .first_ping_at
                .is_none(),
            "minting a token is not the job speaking"
        );

        let res = router
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/ping/{token}{signal}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "signal {signal:?}");
        assert!(
            store
                .get(org, target_id)
                .await
                .unwrap()
                .unwrap()
                .first_ping_at
                .is_some(),
            "signal {signal:?} left the monitor pending"
        );
    }
}

/// A rejected ping must not wire anything up: an unknown token resolves to no
/// row, so there is nothing to stamp.
#[tokio::test]
async fn a_refused_ping_wires_nothing_up() {
    let state = build_test_app_state(|_| {});
    let org = OrgId(Uuid::new_v4());
    let target_id = Uuid::new_v4();
    state.heartbeat_store.ensure(org, target_id).await.unwrap();
    let store = state.heartbeat_store.clone();
    let router = uptimepage::build_app_router(state, CancellationToken::new());

    for uri in ["/ping/not-a-token", "/ping/not-a-token/start"] {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(
        store
            .get(org, target_id)
            .await
            .unwrap()
            .unwrap()
            .first_ping_at
            .is_none()
    );
}

#[tokio::test]
async fn unknown_token_is_a_uniform_404() {
    let state = build_test_app_state(|_| {});
    let router = uptimepage::build_app_router(state, CancellationToken::new());
    let res = router
        .oneshot(
            Request::builder()
                .uri("/ping/not-a-real-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ping_flood_trips_the_per_monitor_rate_limit() {
    let state = build_test_app_state(|_| {});
    let org = OrgId(Uuid::new_v4());
    let target_id = Uuid::new_v4();
    let token = state
        .heartbeat_store
        .ensure(org, target_id)
        .await
        .unwrap()
        .unwrap()
        .token
        .unwrap();
    let router = uptimepage::build_app_router(state, CancellationToken::new());

    let mut saw_429 = false;
    for _ in 0..40 {
        let res = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/ping/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if res.status() == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            break;
        }
        assert_eq!(res.status(), StatusCode::OK);
    }
    assert!(saw_429, "a tight ping loop must eventually 429");
}

fn authed(state: &uptimepage::app::AppState) -> axum::Router {
    common::with_session(
        uptimepage::build_app_router(state.clone(), CancellationToken::new()),
        common::test_user_id(),
        Some(common::test_org_id()),
        Some("test-owner-session"),
    )
}

async fn create_heartbeat(router: &axum::Router, name: &str) -> String {
    let body = serde_json::json!({
        "name": name,
        "interval": 60,
        "enabled": true,
        "tags": [],
        "alerts": [],
        "check": { "type": "heartbeat", "period": 300000, "grace": 300000 }
    });
    let res = router
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .header("X-Requested-With", "uptimepage")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "create heartbeat target");
    common::body_json(res).await["id"]
        .as_str()
        .expect("id")
        .to_string()
}

async fn heartbeat_of(router: &axum::Router, id: &str) -> serde_json::Value {
    let res = router
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/targets/{id}/heartbeat"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    common::body_json(res).await
}

async fn rotate(
    router: &axum::Router,
    id: &str,
    body: Option<serde_json::Value>,
) -> axum::http::Response<Body> {
    let mut req = Request::post(format!("/api/v1/targets/{id}/heartbeat/rotate"))
        .header("X-Requested-With", "uptimepage");
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let payload = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    router
        .clone()
        .oneshot(req.body(payload).unwrap())
        .await
        .unwrap()
}

async fn end_overlap(router: &axum::Router, id: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/targets/{id}/heartbeat/previous"))
                .header("X-Requested-With", "uptimepage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The ping URL is absolute; the inbound route only wants its last segment.
fn token_of(ping_url: &serde_json::Value) -> String {
    ping_url
        .as_str()
        .expect("ping_url")
        .rsplit('/')
        .next()
        .expect("token segment")
        .to_string()
}

async fn ping(router: &axum::Router, token: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::get(format!("/ping/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn rotation_moves_the_url_and_the_old_one_pings_through_the_overlap() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create_heartbeat(&router, "rotating").await;
    let old = token_of(&heartbeat_of(&router, &id).await["ping_url"]);

    let res = rotate(&router, &id, Some(serde_json::json!({}))).await;
    assert_eq!(res.status(), StatusCode::OK);
    let info = common::body_json(res).await;
    let new = token_of(&info["ping_url"]);
    assert_ne!(new, old, "rotation mints a different URL");
    assert!(
        info["previous_url_expires_at"].is_string(),
        "the overlap is open and dated"
    );
    assert!(info["rotated_at"].is_string());

    assert_eq!(ping(&router, &old).await, StatusCode::OK, "overlap accepts");
    assert_eq!(ping(&router, &new).await, StatusCode::OK);
    assert!(
        heartbeat_of(&router, &id).await["previous_url_last_used_at"].is_string(),
        "the card can say the old URL is still carried"
    );

    assert_eq!(end_overlap(&router, &id).await, StatusCode::NO_CONTENT);
    assert_eq!(
        ping(&router, &old).await,
        StatusCode::NOT_FOUND,
        "ended overlap 404s like an unknown token"
    );
    assert_eq!(ping(&router, &new).await, StatusCode::OK);
    assert_eq!(
        end_overlap(&router, &id).await,
        StatusCode::NO_CONTENT,
        "ending nothing is still a 204"
    );
}

#[tokio::test]
async fn revoke_now_kills_the_old_url_in_the_same_commit() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create_heartbeat(&router, "leaked").await;
    let old = token_of(&heartbeat_of(&router, &id).await["ping_url"]);

    let res = rotate(
        &router,
        &id,
        Some(serde_json::json!({ "revoke_previous_immediately": true })),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let info = common::body_json(res).await;
    assert!(
        info["previous_url_expires_at"].is_null(),
        "no overlap was opened"
    );
    assert_eq!(ping(&router, &old).await, StatusCode::NOT_FOUND);
    assert_eq!(
        ping(&router, &token_of(&info["ping_url"])).await,
        StatusCode::OK
    );
}

/// Reading a typeless body as "take the defaults" would answer 200 while
/// downgrading a leaked URL's revoke into a 24-hour overlap.
#[tokio::test]
async fn a_rotate_without_a_json_content_type_is_refused_not_defaulted() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create_heartbeat(&router, "bodyless").await;
    let before = token_of(&heartbeat_of(&router, &id).await["ping_url"]);

    let res = rotate(&router, &id, None).await;
    assert_eq!(res.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        token_of(&heartbeat_of(&router, &id).await["ping_url"]),
        before,
        "a refused rotation rotates nothing"
    );
}

/// A mistyped flag must not read as "take the overlap": the caller asked to
/// kill a leaked URL, and a 200 would leave it live for a day.
#[tokio::test]
async fn rotate_refuses_an_unknown_field_rather_than_ignoring_it() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create_heartbeat(&router, "typo").await;
    let before = token_of(&heartbeat_of(&router, &id).await["ping_url"]);

    let res = rotate(
        &router,
        &id,
        Some(serde_json::json!({ "revoke_immediately": true })),
    )
    .await;
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        token_of(&heartbeat_of(&router, &id).await["ping_url"]),
        before,
        "a refused rotation rotates nothing"
    );
}

#[tokio::test]
async fn rotation_routes_refuse_a_monitor_that_is_not_a_heartbeat() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let body = serde_json::json!({
        "name": "probed",
        "interval": 180,
        "enabled": true,
        "tags": [],
        "alerts": [],
        "check": {
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        }
    });
    let res = router
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("content-type", "application/json")
                .header("X-Requested-With", "uptimepage")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let id = common::body_json(res).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = rotate(&router, &id, Some(serde_json::json!({}))).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        common::body_json(res).await["error"]["code"],
        "HEARTBEAT_NOT_CONFIGURED"
    );
    assert_eq!(end_overlap(&router, &id).await, StatusCode::NOT_FOUND);
}

/// A row lost to a partial create has never shown a URL, so rotate mints one
/// and stops rather than spending the overlap slot on a token nobody holds.
/// The target is written straight to the store, the state a create that died
/// before minting leaves behind.
#[tokio::test]
async fn rotating_a_healed_row_mints_without_parking_a_phantom_overlap() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = state
        .target_store
        .create(
            common::test_org_id(),
            NewTarget {
                name: "healed".into(),
                check: CheckSpec::Heartbeat(HeartbeatCheck {
                    period: Duration::from_secs(300),
                    grace: Duration::from_secs(300),
                    max_runtime: None,
                }),
                interval: Duration::from_secs(60),
                enabled: true,
                tags: vec![],
                alerts: Default::default(),
                region_policy: Default::default(),
                alert_confirmations: 2,
                notify_recovery: true,
                renotify_interval_secs: 3600,
                group_name: None,
                owner_user_id: None,
                regions: None,
            },
            WriteSource::Api,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap()
        .id
        .to_string();

    let res = rotate(&router, &id, Some(serde_json::json!({}))).await;
    assert_eq!(res.status(), StatusCode::OK);
    let info = common::body_json(res).await;
    assert!(info["ping_url"].is_string(), "the heal minted a URL");
    assert!(
        info["previous_url_expires_at"].is_null(),
        "nothing was superseded, so no overlap is advertised"
    );
    assert!(info["rotated_at"].is_null(), "a mint is not a rotation");
    assert_eq!(
        ping(&router, &token_of(&info["ping_url"])).await,
        StatusCode::OK
    );
}

/// A refused kind swap leaves the token in place, and a same-kind edit never
/// touches it, so a cron that pings the old URL keeps counting.
#[tokio::test]
async fn a_kind_swap_is_refused_and_the_ping_token_survives_a_check_edit() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create_heartbeat(&router, "cron").await;
    let token = token_of(&heartbeat_of(&router, &id).await["ping_url"]);

    let patch = |body: serde_json::Value| {
        let router = router.clone();
        let mut req = common::json_request("PATCH", &format!("/api/v1/targets/{id}"), body);
        req.headers_mut()
            .insert("X-Requested-With", HeaderValue::from_static("uptimepage"));
        async move { router.oneshot(req).await.unwrap() }
    };

    let res = patch(serde_json::json!({
        "check": {
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        }
    }))
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        common::body_json(res).await["error"]["code"],
        "CHECK_KIND_IMMUTABLE"
    );
    assert_eq!(ping(&router, &token).await, StatusCode::OK);

    let res = patch(serde_json::json!({
        "check": { "type": "heartbeat", "period": 600000, "grace": 300000 }
    }))
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        token_of(&heartbeat_of(&router, &id).await["ping_url"]),
        token,
        "a same-kind edit keeps the token"
    );
    assert_eq!(ping(&router, &token).await, StatusCode::OK);
}
