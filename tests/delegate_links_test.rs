//! Delegation links end to end on the in-memory app: mint/list/revoke via
//! the API, the public /c/<code> page, the manual create (single-use,
//! kind-pinned, managed-kind-proof), and the consumed poll.

use crate::common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, build_test_app_with_web_and_owner, json_request};
use secrecy::SecretString;
use serde_json::{Value, json};
use tower::ServiceExt;

const BOT: &str = "uptimepagebot";
const WEBHOOK_SECRET: &str = "telegram-webhook-secret-for-delegate-tests";

fn app() -> Router {
    build_test_app_with_web_and_owner(|_| {})
}

fn app_with_bot() -> Router {
    build_test_app_with_web_and_owner(|cfg| {
        cfg.telegram.bot_token = SecretString::from("123:delegate-test-token");
        cfg.telegram.bot_username = BOT.into();
        cfg.telegram.webhook_secret = SecretString::from(WEBHOOK_SECRET);
    })
}

/// Send the bot `/start <code>` from a private chat.
async fn start(app: &Router, chat: i64, code: &str) {
    let update = json!({
        "message": {
            "message_id": 1,
            "text": format!("/start {code}"),
            "chat": { "id": chat, "type": "private" },
            "from": { "id": chat, "first_name": "Olena" },
        }
    });
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hooks/telegram")
                .header("content-type", "application/json")
                .header("x-telegram-bot-api-secret-token", WEBHOOK_SECRET)
                .body(Body::from(update.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn link_status(app: &Router, code: &str) -> String {
    let (st, body) = send(app, "GET", &format!("/c/{code}/status"), Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body["status"].as_str().unwrap().to_string()
}

/// The bot links off the request, so poll until `code` reads `want`.
async fn await_status(app: &Router, code: &str, want: &str) {
    for _ in 0..100 {
        if link_status(app, code).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("link never reached {want}");
}

async fn send(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, path, body))
        .await
        .unwrap();
    let status = resp.status();
    let v = if status == StatusCode::NO_CONTENT {
        Value::Null
    } else {
        body_json(resp).await
    };
    (status, v)
}

async fn get_html(app: &Router, path: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = String::from_utf8_lossy(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .into_owned();
    (status, body)
}

/// Mint a link and return its raw code (tail of the returned URL).
async fn mint(app: &Router, body: Value) -> (Value, String) {
    let (st, resp) = send(app, "POST", "/api/v1/notification-channels/delegate", body).await;
    assert_eq!(st, StatusCode::CREATED, "{resp}");
    let url = resp["url"].as_str().unwrap();
    let code = url.rsplit("/c/").next().unwrap().to_string();
    (resp, code)
}

#[tokio::test]
async fn mint_list_revoke_round_trip() {
    let app = app();
    let (minted, code) = mint(&app, json!({ "name": "Ops Slack", "kind": "slack" })).await;
    assert!(minted["url"].as_str().unwrap().contains("/c/"));

    let (st, list) = send(
        &app,
        "GET",
        "/api/v1/notification-channels/delegate",
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "pending");
    assert_eq!(rows[0]["kind"], "slack");
    assert_eq!(rows[0]["name"], "Ops Slack");

    let (st, page) = get_html(&app, &format!("/c/{code}")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("connect an alert channel"));

    let id = rows[0]["id"].as_str().unwrap();
    let (st, _) = send(
        &app,
        "DELETE",
        &format!("/api/v1/notification-channels/delegate/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Revoked link is dead: page 404s, second revoke 404s.
    let (st, _) = get_html(&app, &format!("/c/{code}")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = send(
        &app,
        "DELETE",
        &format!("/api/v1/notification-channels/delegate/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn manual_create_is_single_use_and_lands_in_the_org() {
    let app = app();
    let (_, code) = mint(&app, json!({ "name": "Ops" })).await;

    let (st, status) = get_html(&app, &format!("/c/{code}/status")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(status.contains("pending"));

    let (st, created) = send(
        &app,
        "POST",
        &format!("/c/{code}/create"),
        json!({ "config": { "type": "slack",
                "webhook_url": "https://hooks.slack.com/services/T000/B000/XXXX" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{created}");
    let channel_id = created["channel_id"].as_str().unwrap();

    // The channel exists in the inviting org, named from the link hint.
    let (st, list) = send(&app, "GET", "/api/v1/notification-channels", Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == channel_id)
        .expect("delegated channel listed");
    assert_eq!(row["name"], "Ops");

    // Spent: poll says consumed, page 404s, a second create 404s.
    let (_, status) = get_html(&app, &format!("/c/{code}/status")).await;
    assert!(status.contains("consumed"));
    let (st, _) = get_html(&app, &format!("/c/{code}")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, body) = send(
        &app,
        "POST",
        &format!("/c/{code}/create"),
        json!({ "config": { "type": "slack",
                "webhook_url": "https://hooks.slack.com/services/T000/B000/YYYY" } }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn create_honours_kind_pin_and_rejects_managed_kinds() {
    let app = app();
    let (_, code) = mint(&app, json!({ "kind": "email" })).await;

    let (st, body) = send(
        &app,
        "POST",
        &format!("/c/{code}/create"),
        json!({ "config": { "type": "slack",
                "webhook_url": "https://hooks.slack.com/services/T000/B000/XXXX" } }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "DELEGATE_KIND_INVALID");

    let (_, open_code) = mint(&app, json!({})).await;
    let (st, body) = send(
        &app,
        "POST",
        &format!("/c/{open_code}/create"),
        json!({ "config": { "type": "telegram_app", "chat_id": "-1", "chat_title": "x" } }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "DELEGATE_KIND_INVALID");

    // A failed create keeps the pin-link alive.
    let (st, _) = get_html(&app, &format!("/c/{code}")).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn mint_caps_outstanding_links_and_rejects_a_kind_it_cannot_create() {
    let app = app();
    for _ in 0..5 {
        mint(&app, json!({})).await;
    }
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/delegate",
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "DELEGATE_LINK_LIMIT");

    for kind in [
        "carrier-pigeon",
        "sms",
        "pagerduty",
        "mattermost",
        "telegram",
        "whatsapp_app",
        "slack_app",
        "discord_app",
    ] {
        let (st, body) = send(
            &app,
            "POST",
            "/api/v1/notification-channels/delegate",
            json!({ "kind": kind }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{kind}: {body}");
        assert_eq!(body["error"]["code"], "DELEGATE_KIND_INVALID", "{kind}");
    }
}

#[tokio::test]
async fn a_link_pins_the_one_tap_telegram_kind_only_where_the_bot_runs() {
    let with_bot = app_with_bot();
    let (_, code) = mint(&with_bot, json!({ "kind": "telegram_app" })).await;
    let (st, html) = get_html(&with_bot, &format!("/c/{code}")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(html.contains(&format!("https://t.me/{BOT}?start={code}")));

    let without_bot = build_test_app_with_web_and_owner(|cfg| {
        cfg.telegram.bot_token = SecretString::from("");
    });
    let (st, body) = send(
        &without_bot,
        "POST",
        "/api/v1/notification-channels/delegate",
        json!({ "kind": "telegram_app" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "DELEGATE_KIND_INVALID");
}

#[tokio::test]
async fn the_bot_spends_only_a_link_that_may_make_a_telegram_channel() {
    let app = app_with_bot();
    let (_, email) = mint(&app, json!({ "kind": "email" })).await;
    let (_, telegram) = mint(&app, json!({ "kind": "telegram_app" })).await;
    let (_, open) = mint(&app, json!({})).await;

    // The bot handles updates in spawn order, so once the later links are
    // consumed it has already answered the email one.
    for (chat, code) in [(1, &email), (2, &telegram), (3, &open)] {
        start(&app, chat, code).await;
    }
    for code in [&telegram, &open] {
        await_status(&app, code, "consumed").await;
    }
    assert_eq!(link_status(&app, &email).await, "pending");
    let (st, _) = get_html(&app, &format!("/c/{email}")).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn create_takes_only_a_kind_the_page_offers() {
    let app = app();
    let (_, code) = mint(&app, json!({})).await;

    for config in [
        json!({ "type": "sms", "provider": "twilio", "to": "+15551234567",
                "from": "+15559876543", "account_sid": "AC00000000000000000000000000000000",
                "auth_token": "tok" }),
        json!({ "type": "pagerduty", "routing_key": "0123456789abcdef0123456789abcdef" }),
        json!({ "type": "telegram", "bot_token": "123456:abc", "chat_id": "-100" }),
    ] {
        let (st, body) = send(
            &app,
            "POST",
            &format!("/c/{code}/create"),
            json!({ "config": config }),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"]["code"], "DELEGATE_KIND_INVALID");
    }

    // Refused before the claim, so the link still works.
    let (st, body) = send(
        &app,
        "POST",
        &format!("/c/{code}/create"),
        json!({ "config": { "type": "webhook", "url": "https://example.com/hook" } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn unknown_code_is_a_generic_404_everywhere() {
    let app = app();
    let (st, _) = get_html(&app, "/c/not-a-real-code").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = get_html(&app, "/c/not-a-real-code/status").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = send(
        &app,
        "POST",
        "/c/not-a-real-code/create",
        json!({ "config": { "type": "slack",
                "webhook_url": "https://hooks.slack.com/services/T000/B000/XXXX" } }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
