use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use common::{body_json, build_test_app_with_owner, json_request};
use serde_json::{Value, json};
use tower::ServiceExt;

fn make_app() -> axum::Router {
    build_test_app_with_owner(|_| {})
}

fn valid_window() -> Value {
    json!({
        "title": "DB upgrade",
        "description": "Brief read-only window.",
        "starts_at": (Utc::now() + Duration::hours(1)).to_rfc3339(),
        "ends_at":   (Utc::now() + Duration::hours(2)).to_rfc3339(),
        "component_ids": []
    })
}

#[tokio::test]
async fn create_maintenance_returns_201_and_location() {
    let app = make_app();
    let resp = app
        .oneshot(json_request("POST", "/api/v1/maintenance", valid_window()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let location = resp
        .headers()
        .get("location")
        .expect("Location header set")
        .to_str()
        .unwrap()
        .to_string();
    assert!(location.starts_with("/api/v1/maintenance/"));
    let v = body_json(resp).await;
    assert_eq!(v["title"], "DB upgrade");
    assert!(v["id"].is_string());
}

#[tokio::test]
async fn create_maintenance_rejects_empty_title() {
    let app = make_app();
    let mut body = valid_window();
    body["title"] = json!("   ");
    let resp = app
        .oneshot(json_request("POST", "/api/v1/maintenance", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "EMPTY_TITLE");
}

#[tokio::test]
async fn create_maintenance_rejects_inverted_time_range() {
    let app = make_app();
    let mut body = valid_window();
    let s = body["starts_at"].clone();
    let e = body["ends_at"].clone();
    body["starts_at"] = e;
    body["ends_at"] = s;
    let resp = app
        .oneshot(json_request("POST", "/api/v1/maintenance", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "INVALID_TIME_RANGE");
}

#[tokio::test]
async fn create_maintenance_rejects_long_duration() {
    let app = make_app();
    let mut body = valid_window();
    body["ends_at"] = json!((Utc::now() + Duration::days(45)).to_rfc3339());
    let resp = app
        .oneshot(json_request("POST", "/api/v1/maintenance", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "INVALID_DURATION");
}

#[tokio::test]
async fn create_maintenance_rejects_a_repeated_component_id() {
    let app = make_app();
    let mut body = valid_window();
    let id = "00000000-0000-0000-0000-000000000001";
    body["component_ids"] = json!([id, id]);
    let resp = app
        .oneshot(json_request("POST", "/api/v1/maintenance", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "INVALID_COMPONENT_ID"
    );
}

#[tokio::test]
async fn create_maintenance_rejects_unknown_component_ids() {
    let app = make_app();
    let mut body = valid_window();
    body["component_ids"] = json!(["00000000-0000-0000-0000-000000000001"]);
    let resp = app
        .oneshot(json_request("POST", "/api/v1/maintenance", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "INVALID_COMPONENT_ID"
    );
}

#[tokio::test]
async fn list_maintenance_paginates_and_filters() {
    let app = make_app();
    for _ in 0..3 {
        let _ = app
            .clone()
            .oneshot(json_request("POST", "/api/v1/maintenance", valid_window()))
            .await
            .unwrap();
    }
    let resp = app
        .oneshot(
            Request::get("/api/v1/maintenance?status=upcoming&limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert!(v["items"].as_array().unwrap().len() >= 3);
    assert_eq!(v["has_more"], false);
}

#[tokio::test]
async fn get_unknown_maintenance_returns_404() {
    let app = make_app();
    let resp = app
        .oneshot(
            Request::get("/api/v1/maintenance/00000000-0000-0000-0000-000000000099")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "MAINTENANCE_NOT_FOUND"
    );
}

#[tokio::test]
async fn delete_maintenance_round_trip() {
    let app = make_app();
    let create = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/maintenance", valid_window()))
        .await
        .unwrap();
    let id = body_json(create).await["id"].as_str().unwrap().to_string();
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
    // Second delete -> 404
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/maintenance/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn update_maintenance_changes_title() {
    let app = make_app();
    let create = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/maintenance", valid_window()))
        .await
        .unwrap();
    let id = body_json(create).await["id"].as_str().unwrap().to_string();
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/maintenance/{id}"),
            json!({"title": "renamed"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["title"], "renamed");
}

#[tokio::test]
async fn cancelled_window_stays_as_history() {
    let app = make_app();
    let create = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/maintenance", valid_window()))
        .await
        .unwrap();
    let id = body_json(create).await["id"].as_str().unwrap().to_string();
    let cancel = app
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
    assert_eq!(cancel.status(), StatusCode::NO_CONTENT);

    let got = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/maintenance/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    assert!(body_json(got).await["deleted_at"].is_string());

    for (filter, expected) in [("upcoming", 0), ("past", 1), ("all", 1)] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/maintenance?status={filter}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let items = body_json(resp).await["items"].as_array().unwrap().len();
        assert_eq!(items, expected, "status={filter}");
    }

    let patch = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/maintenance/{id}"),
            json!({"title": "too late"}),
        ))
        .await
        .unwrap();
    assert_eq!(patch.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(patch).await["error"]["code"],
        "MAINTENANCE_CANCELLED"
    );
}

async fn create_window(app: &axum::Router, body: Value) -> String {
    let resp = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/maintenance", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["id"].as_str().unwrap().to_string()
}

fn running_window() -> Value {
    json!({
        "title": "running",
        "starts_at": (Utc::now() - Duration::hours(2)).to_rfc3339(),
        "ends_at":   (Utc::now() + Duration::hours(1)).to_rfc3339(),
        "component_ids": []
    })
}

fn long_ago() -> Value {
    json!("1970-01-01T00:00:00Z")
}

async fn patch_window(app: &axum::Router, id: &str, body: Value) -> axum::response::Response {
    app.clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/maintenance/{id}"),
            body,
        ))
        .await
        .unwrap()
}

async fn end_now(app: &axum::Router, id: &str) -> axum::response::Response {
    patch_window(app, id, json!({"ends_at": long_ago()})).await
}

async fn end_running_window(app: &axum::Router, id: &str) {
    let resp = end_now(app, id).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a running window can be ended"
    );
}

#[tokio::test]
async fn create_maintenance_rejects_a_window_that_already_ended() {
    let app = make_app();
    let mut body = valid_window();
    body["starts_at"] = json!((Utc::now() - Duration::hours(3)).to_rfc3339());
    body["ends_at"] = json!((Utc::now() - Duration::hours(2)).to_rfc3339());
    let resp = app
        .oneshot(json_request("POST", "/api/v1/maintenance", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["error"]["code"], "INVALID_TIME_RANGE");
    assert_eq!(v["error"]["field"], "ends_at");
}

#[tokio::test]
async fn an_upcoming_window_cannot_be_moved_into_the_past() {
    let app = make_app();
    let id = create_window(&app, valid_window()).await;
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/maintenance/{id}"),
            json!({
                "starts_at": (Utc::now() - Duration::hours(3)).to_rfc3339(),
                "ends_at": (Utc::now() - Duration::hours(2)).to_rfc3339(),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "INVALID_TIME_RANGE");
}

#[tokio::test]
async fn a_completed_window_cannot_be_cancelled() {
    let app = make_app();
    let id = create_window(&app, running_window()).await;
    end_running_window(&app, &id).await;

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
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "MAINTENANCE_COMPLETED"
    );

    let got = app
        .oneshot(
            Request::get(format!("/api/v1/maintenance/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(body_json(got).await["deleted_at"].is_null());
}

#[tokio::test]
async fn an_empty_patch_changes_nothing() {
    let app = make_app();
    let created = app
        .clone()
        .oneshot(json_request("POST", "/api/v1/maintenance", valid_window()))
        .await
        .unwrap();
    let before = body_json(created).await;
    let id = before["id"].as_str().unwrap();

    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/maintenance/{id}"),
            json!({}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let after = body_json(resp).await;
    assert_eq!(after["updated_at"], before["updated_at"]);
    assert_eq!(after["write_source"], before["write_source"]);
}

#[tokio::test]
async fn a_past_end_on_a_running_window_stamps_the_server_clock() {
    let app = make_app();
    let id = create_window(&app, running_window()).await;
    let before = Utc::now();

    let resp = end_now(&app, &id).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let ended: chrono::DateTime<Utc> = body_json(resp).await["ends_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        ended >= before - Duration::seconds(1) && ended <= Utc::now(),
        "ends_at is the server's now, not something the caller chose"
    );
    let past = app
        .oneshot(
            Request::get("/api/v1/maintenance?status=past")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(past).await["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_past_end_with_the_stored_start_still_ends_the_window() {
    let app = make_app();
    let id = create_window(&app, running_window()).await;
    let stored = body_json(
        app.clone()
            .oneshot(
                Request::get(format!("/api/v1/maintenance/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await["starts_at"]
        .clone();

    let resp = patch_window(
        &app,
        &id,
        json!({"starts_at": stored, "ends_at": long_ago()}),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_past_end_only_ends_a_running_window() {
    let app = make_app();
    let upcoming = create_window(&app, valid_window()).await;
    let resp = end_now(&app, &upcoming).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "INVALID_TIME_RANGE");

    let finished = create_window(&app, running_window()).await;
    end_running_window(&app, &finished).await;
    let resp = end_now(&app, &finished).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "MAINTENANCE_COMPLETED"
    );

    let resp = end_now(&app, "00000000-0000-0000-0000-000000000099").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_running_window_cannot_be_moved_into_the_past() {
    let app = make_app();
    let id = create_window(&app, running_window()).await;
    let resp = patch_window(
        &app,
        &id,
        json!({
            "starts_at": (Utc::now() - Duration::hours(3)).to_rfc3339(),
            "ends_at": (Utc::now() - Duration::hours(1)).to_rfc3339(),
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "INVALID_TIME_RANGE");
}

#[tokio::test]
async fn upcoming_windows_list_soonest_first() {
    let app = make_app();
    let window = |title: &str, hours: i64| {
        json!({
            "title": title,
            "starts_at": (Utc::now() + Duration::hours(hours)).to_rfc3339(),
            "ends_at": (Utc::now() + Duration::hours(hours + 1)).to_rfc3339(),
            "component_ids": []
        })
    };
    for (title, hours) in [("later", 30), ("soonest", 2), ("middle", 10)] {
        create_window(&app, window(title, hours)).await;
    }

    let resp = app
        .oneshot(
            Request::get("/api/v1/maintenance?status=upcoming")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let titles: Vec<String> = body_json(resp).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titles, ["soonest", "middle", "later"]);
}

#[tokio::test]
async fn a_completed_window_cannot_be_edited() {
    let app = make_app();
    let id = create_window(&app, running_window()).await;
    end_running_window(&app, &id).await;

    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/maintenance/{id}"),
            json!({"title": "too late"}),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "MAINTENANCE_COMPLETED"
    );
}
