mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use chrono::{Duration, Utc};
use common::{
    body_json, build_test_app_state, build_test_app_with_web_and_owner, json_request, test_org_id,
    test_user_id, with_session,
};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::domain::{NewMaintenanceWindow, WriteSource};
use uptimepage::storage::MaintenanceStore;

fn app() -> axum::Router {
    build_test_app_with_web_and_owner(|_| {})
}

/// A router plus the store behind it, for windows the API refuses to create
/// because they already ended.
fn app_with_store() -> (axum::Router, Arc<dyn MaintenanceStore>) {
    let state = build_test_app_state(|_| {});
    let store = state.maintenance_store.clone();
    let router = with_session(
        uptimepage::build_app_router(state, CancellationToken::new()),
        test_user_id(),
        Some(test_org_id()),
        Some("test-owner-session"),
    );
    (router, store)
}

async fn seed_finished(store: &Arc<dyn MaintenanceStore>, title: &str) -> String {
    let now = Utc::now();
    store
        .create(
            test_org_id(),
            NewMaintenanceWindow {
                title: title.into(),
                description: None,
                starts_at: now - Duration::hours(3),
                ends_at: now - Duration::hours(2),
                component_ids: vec![],
                suppress_alerts: true,
            },
            WriteSource::Api,
            None,
        )
        .await
        .unwrap()
        .id
        .to_string()
}

async fn schedule(app: &axum::Router, title: &str, from_hours: i64, to_hours: i64) -> String {
    let now = Utc::now();
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/maintenance",
            json!({
                "title": title,
                "starts_at": (now + Duration::hours(from_hours)).to_rfc3339(),
                "ends_at": (now + Duration::hours(to_hours)).to_rfc3339(),
                "component_ids": [],
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["id"].as_str().unwrap().to_string()
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>, String) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = to_bytes(resp.into_body(), 4 << 20).await.unwrap();
    (status, location, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn current_view_lists_open_windows_soonest_first() {
    let (app, store) = app_with_store();
    seed_finished(&store, "already-over").await;
    schedule(&app, "later-window", 5, 6).await;
    schedule(&app, "running-window", -1, 1).await;

    let (status, _, body) = get(&app, "/maintenance").await;

    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("already-over"), "completed windows stay out");
    let running = body.find("running-window").expect("active window listed");
    let later = body.find("later-window").expect("upcoming window listed");
    assert!(running < later, "the running window comes first");
    assert!(
        body.contains("data-end-now"),
        "an active window offers end now"
    );
    assert!(
        body.contains("hx-delete=\"/api/v1/maintenance/"),
        "an upcoming window offers cancel"
    );
    assert!(
        body.contains("href=\"/maintenance\" aria-current=\"page\""),
        "the nav marks maintenance as current"
    );
}

#[tokio::test]
async fn past_view_lists_completed_windows_without_actions() {
    let (app, store) = app_with_store();
    let finished = seed_finished(&store, "finished-window").await;
    schedule(&app, "running-window", -1, 1).await;

    let (status, _, body) = get(&app, "/maintenance?view=past").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("finished-window"));
    assert!(!body.contains("running-window"));
    assert!(!body.contains("data-end-now"));
    assert!(!body.contains(&format!("/maintenance/{finished}/edit")));
    assert!(!body.contains("hx-delete="));
}

#[tokio::test]
async fn edit_form_prefills_and_targets_patch() {
    let app = app();
    let id = schedule(&app, "prefilled-title", 2, 3).await;

    let (status, _, body) = get(&app, &format!("/maintenance/{id}/edit")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data-method=\"PATCH\""));
    assert!(body.contains(&format!("data-action=\"/api/v1/maintenance/{id}\"")));
    assert!(body.contains("value=\"prefilled-title\""));
}

#[tokio::test]
async fn edit_form_for_a_completed_window_redirects_to_past() {
    let (app, store) = app_with_store();
    let id = seed_finished(&store, "finished-window").await;

    let (status, location, _) = get(&app, &format!("/maintenance/{id}/edit")).await;

    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/maintenance?view=past"));
}

#[tokio::test]
async fn edit_form_for_an_unknown_window_is_404() {
    let (status, _, _) = get(
        &app(),
        &format!("/maintenance/{}/edit", uuid::Uuid::now_v7()),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn new_form_holds_paging_by_default_and_flags_unpublished_monitors() {
    let app = app();
    let created = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/targets",
            json!({
                "name": "pick-me",
                "interval": 60,
                "enabled": true,
                "tags": [],
                "alerts": [],
                "check": { "type": "heartbeat", "period": 300000, "grace": 300000 }
            }),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    let (status, _, body) = get(&app, "/maintenance/new").await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("data-method=\"POST\""));
    assert!(body.contains("name=\"suppress_alerts\" checked"));
    assert!(body.contains("pick-me"));
    assert!(body.contains("data-published=\"false\""));
}

async fn cancel(app: &axum::Router, id: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/maintenance/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_cancelled_window_moves_to_past_without_actions() {
    let app = app();
    let id = schedule(&app, "called-off", 2, 3).await;
    cancel(&app, &id).await;

    let (_, _, current) = get(&app, "/maintenance").await;
    assert!(
        !current.contains("called-off"),
        "it leaves the current view"
    );

    let (status, _, past) = get(&app, "/maintenance?view=past").await;
    assert_eq!(status, StatusCode::OK);
    assert!(past.contains("called-off"));
    assert!(past.contains("status-badge--none\">cancelled"));
    assert!(past.contains("# cancelled"));
    assert!(!past.contains(&format!("/maintenance/{id}/edit")));
    assert!(!past.contains("hx-delete="));
}

#[tokio::test]
async fn edit_form_for_a_cancelled_window_redirects_to_past() {
    let app = app();
    let id = schedule(&app, "called-off", 2, 3).await;
    cancel(&app, &id).await;

    let (status, location, _) = get(&app, &format!("/maintenance/{id}/edit")).await;

    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/maintenance?view=past"));
}
