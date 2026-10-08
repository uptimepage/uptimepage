//! Integration contract for manual monitors over the API. InMemory-backed, no
//! DB: create pins the schedule, the state round-trips, and the routes refuse
//! what a manual monitor cannot be.

use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::domain::{CheckResult, CheckStatus, ManualStatus};
use uptimepage::storage::{ClampedRange, TimeRange};
use uuid::Uuid;

fn authed(state: &uptimepage::app::AppState) -> axum::Router {
    common::with_session(
        uptimepage::build_app_router(state.clone(), CancellationToken::new()),
        common::test_user_id(),
        Some(common::test_org_id()),
        Some("test-owner-session"),
    )
}

async fn send(
    router: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("X-Requested-With", "uptimepage");
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let payload = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let res = router
        .clone()
        .oneshot(req.body(payload).unwrap())
        .await
        .unwrap();
    let status = res.status();
    (status, common::body_json(res).await)
}

async fn create(router: &axum::Router, check: Value) -> Value {
    let (status, body) = send(
        router,
        "POST",
        "/api/v1/targets",
        Some(json!({
            "name": "VoIP SIP trunks",
            "interval": 60,
            "alert_confirmations": 3,
            "check": check,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[tokio::test]
async fn a_manual_monitor_starts_up_and_takes_the_state_it_is_given() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let created = create(&router, json!({ "type": "manual" })).await;
    assert_eq!(created["check"], json!({ "type": "manual" }));
    assert_eq!(created["interval"], 60, "restated once a minute");
    let (status, refused) = send(
        &router,
        "POST",
        "/api/v1/targets",
        Some(json!({ "name": "slow", "interval": 300, "check": { "type": "manual" } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"]["code"], "INVALID_INTERVAL");
    let (status, refused) = send(
        &router,
        "POST",
        "/api/v1/targets",
        Some(json!({
            "name": "held",
            "interval": 60,
            "recovery_period_secs": 300,
            "check": { "type": "manual" },
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"]["field"], "recovery_period_secs");
    assert_eq!(
        created["alert_confirmations"], 1,
        "a set is the confirmation"
    );
    let id = created["id"].as_str().unwrap();
    let path = format!("/api/v1/targets/{id}/state");

    let (status, body) = send(&router, "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "up");
    assert_eq!(body["note"], Value::Null);
    assert!(
        body.get("set_by").is_some_and(Value::is_null),
        "nobody has set it yet, and the key is still there: {body}"
    );

    let (status, body) = send(
        &router,
        "PUT",
        &path,
        Some(json!({ "status": "down", "note": "  carrier reports a trunk outage " })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "down");
    assert_eq!(body["note"], "carrier reports a trunk outage");
    assert_eq!(body["set_by"], common::test_user_id().0.to_string());
    let held = state
        .manual_runtime
        .state(Uuid::parse_str(id).unwrap())
        .expect("the set reached this node's evaluator");
    assert_eq!(held.status, ManualStatus::Down);

    let (_, read_back) = send(&router, "GET", &path, None).await;
    assert_eq!(read_back, body);
}

#[tokio::test]
async fn a_bad_set_is_refused_and_changes_nothing() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create(&router, json!({ "type": "manual" })).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/api/v1/targets/{id}/state");

    let long = "x".repeat(201);
    for (body, want, code) in [
        (
            json!({ "status": "down", "note": long }),
            StatusCode::BAD_REQUEST,
            "INVALID_MANUAL_STATE",
        ),
        (
            json!({ "status": "down", "note": "two\nlines" }),
            StatusCode::BAD_REQUEST,
            "INVALID_MANUAL_STATE",
        ),
        (
            json!({ "status": "error" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_JSON",
        ),
        (
            json!({ "status": "down", "reason": "typo" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_JSON",
        ),
    ] {
        let (status, res) = send(&router, "PUT", &path, Some(body.clone())).await;
        assert_eq!(status, want, "{body} -> {res}");
        assert_eq!(res["error"]["code"], code, "{body}");
    }
    let (_, state_now) = send(&router, "GET", &path, None).await;
    assert_eq!(state_now["status"], "up");
}

#[tokio::test]
async fn only_a_manual_monitor_has_a_state_and_it_has_nothing_to_probe() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let probed = create(
        &router,
        json!({
            "type": "http",
            "url": "https://example.com/",
            "method": "GET",
            "timeout": 5000,
            "follow_redirects": false,
            "max_redirects": 0,
            "expected_status": { "kind": "exact", "value": 200 },
            "headers": {},
            "verify_tls": true
        }),
    )
    .await;
    let probed_id = probed["id"].as_str().unwrap();
    for (method, body) in [("GET", None), ("PUT", Some(json!({ "status": "down" })))] {
        let (status, res) = send(
            &router,
            method,
            &format!("/api/v1/targets/{probed_id}/state"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(res["error"]["code"], "MANUAL_NOT_CONFIGURED");
    }

    let manual_id = create(&router, json!({ "type": "manual" })).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, res) = send(
        &router,
        "PUT",
        &format!("/api/v1/targets/{manual_id}/regions"),
        Some(json!({ "regions": ["eu-helsinki"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
    assert_eq!(res["error"]["code"], "REGION_INVALID");
    assert!(
        res["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("manual monitors are set by hand"),
        "{res}"
    );
}

async fn results_for(state: &uptimepage::app::AppState, id: Uuid) -> Vec<CheckResult> {
    let now = chrono::Utc::now();
    let range = ClampedRange::unclamped(TimeRange {
        from: now - chrono::Duration::hours(1),
        to: now + chrono::Duration::minutes(1),
    });
    for _ in 0..50 {
        let rows = state
            .results_store
            .recent_results_for_targets(&[(common::test_org_id(), id)], range, 10)
            .await
            .unwrap();
        if !rows.is_empty() {
            return rows.into_iter().map(|(_, r)| r).collect();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Vec::new()
}

/// Up from the moment it exists, rather than no data until the scheduler's
/// next pass; a set lands in history at once.
#[tokio::test]
async fn a_new_manual_monitor_reports_up_and_a_set_reports_at_once() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create(&router, json!({ "type": "manual" })).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let target = Uuid::parse_str(&id).unwrap();
    let first = results_for(&state, target).await;
    assert_eq!(first.len(), 1, "one result on create");
    assert_eq!(first[0].status, CheckStatus::Up);

    let (status, _) = send(
        &router,
        "PUT",
        &format!("/api/v1/targets/{id}/state"),
        Some(json!({ "status": "degraded", "note": "one trunk of two" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut rows = Vec::new();
    for _ in 0..50 {
        rows = results_for(&state, target).await;
        if rows.len() > 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let newest = rows.iter().max_by_key(|r| r.timestamp).expect("results");
    assert_eq!(newest.status, CheckStatus::Degraded);
    assert_eq!(
        newest.error.as_deref(),
        Some("marked degraded: one trunk of two")
    );
}

/// A cadence and a confirmation count it would not follow are refused, not
/// stored and ignored; the values it runs on still save.
#[tokio::test]
async fn a_manual_monitor_refuses_a_schedule_it_would_not_follow() {
    let state = common::build_test_app_state(|_| {});
    let router = authed(&state);
    let id = create(&router, json!({ "type": "manual" })).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/api/v1/targets/{id}");
    for (body, code) in [
        (json!({ "interval": 300 }), "INVALID_INTERVAL"),
        (json!({ "alert_confirmations": 3 }), "INVALID_ALERT_CONFIG"),
        (
            json!({ "recovery_period_secs": 300 }),
            "INVALID_ALERT_CONFIG",
        ),
    ] {
        let (status, res) = send(&router, "PATCH", &path, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {res}");
        assert_eq!(res["error"]["code"], code);
    }
    let (status, res) = send(
        &router,
        "PATCH",
        &path,
        Some(json!({
            "name": "SIP trunks",
            "interval": 60,
            "alert_confirmations": 1,
            "recovery_period_secs": 0,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["name"], "SIP trunks");
}
