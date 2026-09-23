mod common;

use axum::body::Body;
use axum::http::Request;
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use common::{
    body_json, build_test_app_with_owner, build_test_app_with_seedable_incidents, json_request,
};
use serde_json::json;
use tower::ServiceExt;
use uptimepage::domain::{CheckStatus, Incident, IncidentSeverity};
use uuid::Uuid;

fn seed_incident() -> Incident {
    Incident {
        id: Uuid::now_v7(),
        target_id: Some(Uuid::now_v7()),
        started_at: Utc::now() - Duration::minutes(10),
        ended_at: None,
        status: CheckStatus::Down,
        duration_secs: None,
        check_count: 3,
        counts_as_downtime: true,
        error_sample: None,
        severity: IncidentSeverity::Major,
        public_title: None,
        public_description: None,
        created_at: Some(Utc::now() - Duration::minutes(10)),
        updated_at: Some(Utc::now() - Duration::minutes(10)),
        updates: vec![],
        regions_down: Vec::new(),
        regions_up: Vec::new(),
    }
}

#[tokio::test]
async fn patch_narration_sets_title_and_severity() {
    let (app, store) = build_test_app_with_seedable_incidents(|_| {});
    let inc = seed_incident();
    let id = inc.id;
    store.seed(inc);
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/incidents/{id}"),
            json!({
                "public_title": "Latency spike",
                "severity": "critical"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["public_title"], "Latency spike");
    assert_eq!(v["severity"], "critical");
}

#[tokio::test]
async fn patch_narration_null_clears_title() {
    let (app, store) = build_test_app_with_seedable_incidents(|_| {});
    let mut inc = seed_incident();
    inc.public_title = Some("old".into());
    let id = inc.id;
    store.seed(inc);
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/incidents/{id}"),
            json!({ "public_title": null }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert!(v["public_title"].is_null());
}

#[tokio::test]
async fn patch_narration_unknown_id_404() {
    let (app, _) = build_test_app_with_seedable_incidents(|_| {});
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/incidents/{}", Uuid::now_v7()),
            json!({ "public_title": "x" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["error"]["code"], "INCIDENT_NOT_FOUND");
}

#[tokio::test]
async fn patch_narration_empty_title_rejected() {
    let (app, store) = build_test_app_with_seedable_incidents(|_| {});
    let inc = seed_incident();
    let id = inc.id;
    store.seed(inc);
    let resp = app
        .oneshot(json_request(
            "PATCH",
            &format!("/api/v1/incidents/{id}"),
            json!({ "public_title": "   " }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "EMPTY_TITLE");
}

#[tokio::test]
async fn post_update_appends_to_timeline() {
    let (app, store) = build_test_app_with_seedable_incidents(|_| {});
    let inc = seed_incident();
    let id = inc.id;
    store.seed(inc);
    let resp = app
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/incidents/{id}/updates"),
            json!({ "phase": "identified", "message": "rolling back" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let location = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(location, format!("/api/v1/incidents/{id}"));
    let v = body_json(resp).await;
    assert_eq!(v["phase"], "identified");
    assert_eq!(v["message"], "rolling back");
}

#[tokio::test]
async fn post_update_empty_message_rejected() {
    let (app, store) = build_test_app_with_seedable_incidents(|_| {});
    let inc = seed_incident();
    let id = inc.id;
    store.seed(inc);
    let resp = app
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/incidents/{id}/updates"),
            json!({ "phase": "investigating", "message": "  " }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "EMPTY_MESSAGE");
}

#[tokio::test]
async fn post_update_long_message_rejected() {
    let (app, store) = build_test_app_with_seedable_incidents(|_| {});
    let inc = seed_incident();
    let id = inc.id;
    store.seed(inc);
    let resp = app
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/incidents/{id}/updates"),
            json!({ "phase": "monitoring", "message": "x".repeat(2_001) }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], "MESSAGE_TOO_LONG");
}

#[tokio::test]
async fn post_update_unknown_incident_404() {
    let (app, _) = build_test_app_with_seedable_incidents(|_| {});
    let resp = app
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/incidents/{}/updates", Uuid::now_v7()),
            json!({ "phase": "investigating", "message": "x" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["error"]["code"], "INCIDENT_NOT_FOUND");
}

#[tokio::test]
async fn post_update_invalid_phase_rejected_by_serde() {
    let (app, store) = build_test_app_with_seedable_incidents(|_| {});
    let inc = seed_incident();
    let id = inc.id;
    store.seed(inc);
    let resp = app
        .oneshot(json_request(
            "POST",
            &format!("/api/v1/incidents/{id}/updates"),
            json!({ "phase": "loitering", "message": "x" }),
        ))
        .await
        .unwrap();
    // axum's Json extractor returns 422 for malformed JSON / unknown enum
    // variants. 400 is reserved for handler-level validation.
    assert!(
        resp.status() == StatusCode::BAD_REQUEST
            || resp.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "phase validation should be 4xx; got {}",
        resp.status()
    );
}

/// Same as [`json_request`] plus the header the CSRF layer requires of a
/// session-authenticated write.
fn owner_json(method: &str, path: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("X-Requested-With", "uptimepage")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// An incident can name no monitor, and the wire says so rather than
/// inventing an id. Amending one is covered against Postgres
/// (`a_target_less_incident_survives_every_narration_path_pg`): the ops and
/// narration doubles hold separate maps, while the real ones share a row.
#[tokio::test]
async fn a_declared_incident_may_name_no_monitor() {
    let app = build_test_app_with_owner(|_| {});
    let resp = app
        .oneshot(owner_json(
            "POST",
            "/api/v1/incidents",
            json!({ "title": "partner API degraded" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let declared = body_json(resp).await;
    assert!(declared["target_id"].is_null(), "{declared}");
    assert_eq!(declared["origin"], "manual");
}

/// Recording a problem the monitors cannot see must not page anyone or tell
/// customers, unless asked.
#[tokio::test]
async fn a_declared_incident_stays_internal_unless_it_asks_to_be_public() {
    let app = build_test_app_with_owner(|_| {});
    let resp = app
        .clone()
        .oneshot(owner_json(
            "POST",
            "/api/v1/incidents",
            json!({ "title": "partner API degraded" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let quiet = body_json(resp).await;
    assert_eq!(quiet["visibility"], "internal");
    assert_eq!(quiet["origin"], "manual");

    // With no monitor, public means the pages it names; naming none would
    // publish to nobody while answering success.
    let resp = app
        .clone()
        .oneshot(owner_json(
            "POST",
            "/api/v1/incidents",
            json!({ "title": "partner API down", "visibility": "public" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "INCIDENT_STATUS_PAGE_REQUIRED"
    );

    let page = Uuid::now_v7();
    let resp = app
        .clone()
        .oneshot(owner_json(
            "POST",
            "/api/v1/incidents",
            json!({
                "title": "partner API down",
                "visibility": "public",
                "status_page_ids": [page],
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let published = body_json(resp).await;
    assert_eq!(published["visibility"], "public");
    assert!(
        published["public_title"].is_null(),
        "the internal title is not the customers' headline"
    );

    // Publish replaces the list, so a client has to be able to read it back.
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/v1/incidents/{}",
                    published["id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status_page_ids"], json!([page]));
}

/// A note about an outage the checks never saw must not quietly dent uptime.
#[tokio::test]
async fn a_declared_incident_leaves_uptime_alone_unless_it_asks_to_count() {
    let app = build_test_app_with_owner(|_| {});
    let resp = app
        .clone()
        .oneshot(owner_json(
            "POST",
            "/api/v1/incidents",
            json!({ "title": "customer reports checkout failing" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["counts_as_downtime"], false);

    let resp = app
        .oneshot(owner_json(
            "POST",
            "/api/v1/incidents",
            json!({ "title": "payments down, site up", "counts_as_downtime": true }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["counts_as_downtime"], true);
}
