//! HTTP contract for the in-app help relay: `/help` + `POST /api/v1/support`.
//!
//! Load-bearing: neither surface exists until an address is configured, the
//! mail answers to the customer, the caller cannot dictate who it came from,
//! and the endpoint cannot become a mail cannon.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_test_app_with_owner_and_email, build_test_app_with_web, session_email};
use serde_json::{Value, json};
use tower::ServiceExt;
use uptimepage::email::{EmailTemplate, InMemoryEmailSender};

const SUPPORT: &str = "help@operator.test";

fn configured() -> (Router, std::sync::Arc<InMemoryEmailSender>) {
    build_test_app_with_owner_and_email(|cfg| {
        cfg.email.support_address = SUPPORT.into();
    })
}

async fn post_support(app: &Router, body: Value) -> (StatusCode, String) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/support")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get_status(app: &Router, uri: &str) -> StatusCode {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

fn help_request(topic: &str, message: &str) -> Value {
    json!({ "topic": topic, "message": message })
}

// ── Gating ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn unconfigured_deployment_has_neither_page_nor_endpoint() {
    let app = build_test_app_with_web(|cfg| cfg.email.support_address = String::new());
    assert_eq!(
        get_status(&app, "/help").await,
        StatusCode::NOT_FOUND,
        "a self-host install must not offer a page that goes nowhere"
    );
    let (status, _) = post_support(&app, help_request("question", "hello")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the endpoint is absent, not merely inert"
    );
}

#[tokio::test]
async fn configured_deployment_serves_the_page() {
    let (app, _) = configured();
    assert_eq!(get_status(&app, "/help").await, StatusCode::OK);
}

#[tokio::test]
async fn a_signed_out_visitor_cannot_reach_the_operator_inbox() {
    // Configured, but no session layer: never a public contact form.
    let app = build_test_app_with_web(|cfg| cfg.email.support_address = SUPPORT.into());
    let (status, _) = post_support(&app, help_request("question", "hello")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        get_status(&app, "/help").await,
        StatusCode::SEE_OTHER,
        "the page bounces to login rather than rendering"
    );
}

// ── Delivery ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn relayed_mail_goes_to_the_operator_and_answers_to_the_customer() {
    let (app, mail) = configured();
    let (status, body) = post_support(
        &app,
        json!({
            "topic": "regions",
            "message": "please add a probe in sao paulo",
            "page_url": "/targets/42",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");

    let sent = mail.sent();
    assert_eq!(sent.len(), 1, "exactly one mail per request");
    assert_eq!(sent[0].to.address, SUPPORT, "delivered to the operator");
    assert_eq!(
        sent[0].template.reply_to(),
        Some(session_email(common::test_user_id()).as_str()),
        "replying reaches the customer, not the no-reply sender"
    );

    let rendered = sent[0].template.render("Uptimepage");
    assert!(rendered.subject.starts_with("[help/regions #"));
    assert!(
        rendered
            .text_body
            .contains("please add a probe in sao paulo")
    );
    assert!(
        rendered.text_body.contains("/targets/42"),
        "carries the page the user came from"
    );
}

#[tokio::test]
async fn the_reference_ties_the_screen_the_subject_and_the_headers_together() {
    let (app, mail) = configured();
    let (status, body) = post_support(&app, help_request("bug", "checks are flapping")).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let ack: Value = serde_json::from_str(&body).expect("json ack");
    let reference = ack["reference"].as_str().expect("reference in ack");
    assert_eq!(reference.len(), 12, "the UUID's random tail");

    let sent = mail.sent();
    let headers = sent[0].template.custom_headers();
    let canonical = headers
        .iter()
        .find(|(name, _)| *name == "Uptimepage-Request-Id")
        .map(|(_, v)| v.clone())
        .expect("request id header");
    assert!(
        canonical.ends_with(reference),
        "the screen reference must resolve to the canonical id: {canonical} vs {reference}"
    );

    let rendered = sent[0].template.render("Uptimepage");
    assert!(
        rendered.subject.contains(&format!("#{reference}")),
        "subject: {}",
        rendered.subject
    );
    assert!(rendered.text_body.contains(&canonical), "exact lookup form");
}

#[tokio::test]
async fn each_request_gets_its_own_reference() {
    let (app, mail) = configured();
    for _ in 0..2 {
        let (status, _) = post_support(&app, help_request("bug", "again")).await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    let sent = mail.sent();
    let ids: Vec<_> = sent
        .iter()
        .map(|e| {
            e.template
                .custom_headers()
                .into_iter()
                .find(|(n, _)| *n == "Uptimepage-Request-Id")
                .map(|(_, v)| v)
                .expect("request id")
        })
        .collect();
    assert_ne!(ids[0], ids[1], "a reference identifies one request");
}

#[tokio::test]
async fn identity_comes_from_the_session_not_the_request_body() {
    let (app, mail) = configured();
    // A caller trying to file a request as somebody else, from another org:
    // the body has no such keys, so the attempt is refused, not ignored.
    let (status, body) = post_support(
        &app,
        json!({
            "topic": "billing",
            "message": "refund me",
            "from_email": "ceo@victim.test",
            "org_slug": "victim",
            "plan": "enterprise",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("INVALID_JSON"), "{body}");
    assert!(body.contains("unknown field `from_email`"), "{body}");
    assert!(
        mail.sent().is_empty(),
        "nothing is filed from a refused body"
    );

    let (status, _) = post_support(&app, help_request("billing", "refund me")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let sent = mail.sent();
    let EmailTemplate::SupportRequest {
        from_email, plan, ..
    } = &sent[0].template
    else {
        panic!("expected a support request template");
    };
    assert_eq!(*from_email, session_email(common::test_user_id()));
    assert_ne!(plan, "enterprise", "plan is server-resolved");
}

#[tokio::test]
async fn an_off_site_page_url_never_reaches_the_operator_inbox() {
    let (app, mail) = configured();
    let (status, _) = post_support(
        &app,
        json!({
            "topic": "bug",
            "message": "broken",
            "page_url": "https://evil.test/collect-credentials",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let rendered = mail.sent()[0].template.render("Uptimepage");
    assert!(
        !rendered.text_body.contains("evil.test"),
        "an attacker-chosen link must not render as if it were ours"
    );
    assert!(!rendered.html_body.contains("evil.test"));
}

// ── Validation ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_topic_is_refused() {
    let (app, mail) = configured();
    let (status, body) = post_support(&app, help_request("urgent-escalation", "hi")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("INVALID_TOPIC"), "body: {body}");
    assert!(mail.is_empty(), "a rejected request sends nothing");
}

#[tokio::test]
async fn a_blank_message_is_refused() {
    let (app, mail) = configured();
    let (status, body) = post_support(&app, help_request("question", "   \n  ")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("EMPTY_MESSAGE"), "body: {body}");
    assert!(mail.is_empty());
}

#[tokio::test]
async fn an_oversized_message_is_refused() {
    let (app, mail) = configured();
    let (status, body) = post_support(&app, help_request("bug", &"a".repeat(4_001))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("MESSAGE_TOO_LONG"), "body: {body}");
    assert!(mail.is_empty());
}

// ── Abuse ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_wedged_client_cannot_flood_the_operator_inbox() {
    let (app, mail) = configured();
    for i in 0..2 {
        let (status, body) = post_support(&app, help_request("question", "still here?")).await;
        assert_eq!(status, StatusCode::ACCEPTED, "request {i} body: {body}");
    }
    let (status, body) = post_support(&app, help_request("question", "still here?")).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the fixed per-minute ceiling holds regardless of plan; body: {body}"
    );
    assert_eq!(mail.len(), 2, "the throttled request never became mail");
}
