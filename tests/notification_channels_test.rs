//! API contract for `/api/v1/notification-channels`: CRUD, secret redaction
//! on every read path, the `GET → PATCH` sentinel guard, and the test-send
//! action. In-memory store (no Postgres); cross-tenant isolation and the
//! quota cap are exercised against the live Postgres store separately.

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use common::{body_json, build_test_app_with_owner, json_request};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    build_test_app_with_owner(|_| {})
}

async fn send(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(json_request(method, path, body))
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn send_empty(app: &Router, method: &str, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
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

fn slack_body(name: &str) -> Value {
    json!({
        "name": name,
        "config": { "type": "slack", "webhook_url": "https://hooks.slack.com/services/T/B/secret" }
    })
}

#[tokio::test]
async fn create_redacts_secret_and_sets_location() {
    let app = app();
    let resp = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/v1/notification-channels",
            slack_body("Ops"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    assert!(loc.starts_with("/api/v1/notification-channels/"));
    let body = body_json(resp).await;
    assert_eq!(body["name"], "Ops");
    assert_eq!(body["kind"], "slack");
    // The webhook URL carries a token, so it must come back masked.
    assert_eq!(body["config"]["webhook_url"], "***");
    assert!(!body.to_string().contains("secret"));
}

#[tokio::test]
async fn list_and_get_keep_secrets_redacted() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Ops"),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap();

    let (st, list) = send_empty(&app, "GET", "/api/v1/notification-channels").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["config"]["webhook_url"], "***");

    let (st, one) = send_empty(&app, "GET", &format!("/api/v1/notification-channels/{id}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(one["config"]["webhook_url"], "***");
}

#[tokio::test]
async fn rejects_resubmitted_redaction_sentinel_on_create() {
    let app = app();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "Bad",
            "config": { "type": "slack", "webhook_url": "***" }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "REDACTION_SENTINEL");
}

#[tokio::test]
async fn patch_name_only_keeps_stored_secret() {
    let app = app();
    let (_, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Ops"),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    // Round-trip the redacted config back as a PATCH: must be rejected, not
    // written over the real webhook.
    let (st, body) = send(
        &app,
        "PATCH",
        &format!("/api/v1/notification-channels/{id}"),
        json!({ "config": { "type": "slack", "webhook_url": "***" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "REDACTION_SENTINEL");

    // Name-only PATCH (config omitted) succeeds and leaves the secret intact.
    let (st, patched) = send(
        &app,
        "PATCH",
        &format!("/api/v1/notification-channels/{id}"),
        json!({ "name": "Ops renamed", "enabled": false }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(patched["name"], "Ops renamed");
    assert_eq!(patched["enabled"], false);
    assert_eq!(patched["config"]["webhook_url"], "***");
}

/// The Acknowledge button is on unless a create says otherwise, and a save
/// that leaves it out keeps what was set.
#[tokio::test]
async fn the_acknowledge_button_defaults_on_and_survives_a_save_without_it() {
    let app = app();
    let (_, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Ops"),
    )
    .await;
    assert_eq!(created["acknowledge_button"], true);
    let path = format!(
        "/api/v1/notification-channels/{}",
        created["id"].as_str().unwrap()
    );

    let (st, off) = send(&app, "PATCH", &path, json!({ "acknowledge_button": false })).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(off["acknowledge_button"], false);
    let (_, renamed) = send(&app, "PATCH", &path, json!({ "name": "Customer room" })).await;
    assert_eq!(renamed["acknowledge_button"], false);

    let mut quiet = slack_body("Quiet");
    quiet["acknowledge_button"] = json!(false);
    let (st, created) = send(&app, "POST", "/api/v1/notification-channels", quiet).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(created["acknowledge_button"], false);
}

/// The Resolve button is off unless a create says otherwise, and a save that
/// leaves it out keeps what was set, in both directions.
#[tokio::test]
async fn the_resolve_button_defaults_off_and_survives_a_save_without_it() {
    let app = app();
    let (_, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Ops"),
    )
    .await;
    assert_eq!(created["resolve_button"], false);
    assert_eq!(created["acknowledge_button"], true);
    let path = format!(
        "/api/v1/notification-channels/{}",
        created["id"].as_str().unwrap()
    );

    let (st, on) = send(&app, "PATCH", &path, json!({ "resolve_button": true })).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(on["resolve_button"], true);
    assert_eq!(
        on["acknowledge_button"], true,
        "the other switch is untouched"
    );
    let (_, renamed) = send(&app, "PATCH", &path, json!({ "name": "Ops renamed" })).await;
    assert_eq!(renamed["resolve_button"], true);
    let (_, quiet) = send(&app, "PATCH", &path, json!({ "acknowledge_button": false })).await;
    assert_eq!(
        quiet["resolve_button"], true,
        "switching acknowledge off leaves it"
    );
    let (_, off) = send(&app, "PATCH", &path, json!({ "resolve_button": false })).await;
    assert_eq!(off["resolve_button"], false);

    let mut eager = slack_body("Eager");
    eager["resolve_button"] = json!(true);
    let (st, created) = send(&app, "POST", "/api/v1/notification-channels", eager).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(created["resolve_button"], true);
}

#[tokio::test]
async fn rejects_non_https_config_and_empty_name() {
    let app = app();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "Insecure", "config": { "type": "slack", "webhook_url": "http://x" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "INVALID_CHANNEL_CONFIG");

    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "  ", "config": { "type": "slack", "webhook_url": "https://hooks.slack.com/x" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "CHANNEL_NAME_INVALID");
}

#[tokio::test]
async fn duplicate_name_is_unprocessable() {
    let app = app();
    let (st, _) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Dup"),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Dup"),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_NAME_TAKEN");
}

#[tokio::test]
async fn whatsapp_channel_round_trips_with_token_masked() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-whatsapp",
            "config": {
                "type": "whatsapp",
                "access_token": "EAAGsupersecret",
                "phone_number_id": "106540352242922",
                "to": "15551234567",
                "template_name": "uptime_alert"
            }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "whatsapp");
    assert_eq!(created["config"]["type"], "whatsapp");
    assert_eq!(created["config"]["access_token"], "***");
    assert_eq!(created["config"]["to"], "15551234567");
    assert_eq!(created["config"]["template_name"], "uptime_alert");
    assert!(!created.to_string().contains("supersecret"));
}

#[tokio::test]
async fn provider_webhook_kinds_round_trip_with_url_masked() {
    let app = app();
    for (kind, url) in [
        (
            "discord",
            "https://discord.com/api/webhooks/123456/zzPROVIDERSECRETzz",
        ),
        (
            "msteams",
            "https://prod-77.westus.logic.azure.com/workflows/zzPROVIDERSECRETzz/triggers/manual",
        ),
        (
            "google_chat",
            "https://chat.googleapis.com/v1/spaces/AAAA/messages?key=k&token=zzPROVIDERSECRETzz",
        ),
    ] {
        let (st, created) = send(
            &app,
            "POST",
            "/api/v1/notification-channels",
            json!({
                "name": format!("ops-{kind}"),
                "config": { "type": kind, "webhook_url": url }
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{kind}: {created}");
        assert_eq!(created["kind"], kind);
        assert_eq!(created["config"]["type"], kind);
        assert_eq!(created["config"]["webhook_url"], "***", "{kind}");
        assert!(
            !created.to_string().contains("zzPROVIDERSECRETzz"),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn pagerduty_round_trips_with_routing_key_masked() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-pagerduty",
            "config": { "type": "pagerduty", "routing_key": "zzPDSECRETzz0123456789abcdef0123" }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "pagerduty");
    assert_eq!(created["config"]["routing_key"], "***");
    assert!(!created.to_string().contains("zzPDSECRETzz"));
}

#[tokio::test]
async fn ntfy_round_trips_with_token_masked_and_topic_visible() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-ntfy",
            "config": {
                "type": "ntfy",
                "server_url": "https://ntfy.example.com",
                "topic": "uptime-alerts",
                "access_token": "tk_zzNTFYSECRETzz"
            }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "ntfy");
    assert_eq!(created["config"]["server_url"], "https://ntfy.example.com");
    assert_eq!(created["config"]["topic"], "uptime-alerts");
    assert_eq!(created["config"]["access_token"], "***");
    assert!(!created.to_string().contains("zzNTFYSECRETzz"));

    // server_url defaults to ntfy.sh and a token-less config stays open.
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-ntfy-default",
            "config": { "type": "ntfy", "topic": "ops" }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["config"]["server_url"], "https://ntfy.sh");
    assert!(created["config"].get("access_token").is_none());
}

#[tokio::test]
async fn gotify_round_trips_with_token_masked_and_server_visible() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-gotify",
            "config": {
                "type": "gotify",
                "server_url": "https://push.example.com/gotify/",
                "token": "zzGOTIFYSECRETzz"
            }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "gotify");
    // A subpath install keeps its path; only the trailing slash is dropped.
    assert_eq!(
        created["config"]["server_url"],
        "https://push.example.com/gotify"
    );
    assert_eq!(created["config"]["token"], "***");
    assert!(!created.to_string().contains("zzGOTIFYSECRETzz"));
}

#[tokio::test]
async fn mattermost_round_trips_with_the_hook_url_masked_and_the_ping_visible() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-mattermost",
            "config": {
                "type": "mattermost",
                "webhook_url": "https://mm.example.com/hooks/zzMMSECRETKEYzz",
                "mention": "@Here, OnCall-SRE"
            }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "mattermost");
    assert_eq!(created["config"]["webhook_url"], "***");
    assert_eq!(created["config"]["mention"], "@here, oncall-sre");
    assert!(!created.to_string().contains("zzMMSECRETKEYzz"));
}

#[tokio::test]
async fn pushover_round_trips_with_both_keys_masked() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-pushover",
            "config": {
                "type": "pushover",
                "token": "zzPOTOKENzz8gMaC0QOYAMyEEuzJny",
                "user": "zzPOUSERzzDXghDmr9QzzfQu27cmVR",
                "device": "droid2"
            }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "pushover");
    assert_eq!(created["config"]["token"], "***");
    assert_eq!(created["config"]["user"], "***");
    assert_eq!(created["config"]["device"], "droid2");
    assert!(!created.to_string().contains("zzPOTOKENzz"));
    assert!(!created.to_string().contains("zzPOUSERzz"));
}

#[tokio::test]
async fn sms_round_trips_with_only_the_gateway_secret_masked() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({
            "name": "oncall-sms",
            "config": {
                "type": "sms",
                "provider": "twilio",
                "to": "+15551234567",
                "from": "+15557654321",
                "account_sid": "AC0123456789ABCDEF0123456789ABCDEF",
                "auth_token": "zzTWILIOSECRETzz"
            }
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "sms");
    assert_eq!(created["config"]["provider"], "twilio");
    // Identifiers and routing survive; only the auth token is masked.
    assert_eq!(created["config"]["to"], "+15551234567");
    assert_eq!(
        created["config"]["account_sid"],
        "AC0123456789ABCDEF0123456789ABCDEF"
    );
    assert_eq!(created["config"]["auth_token"], "***");
    assert!(!created.to_string().contains("zzTWILIOSECRETzz"));
}

#[tokio::test]
async fn paging_kinds_reject_malformed_configs() {
    let app = app();
    for (name, config) in [
        (
            "pd-short-key",
            json!({ "type": "pagerduty", "routing_key": "short" }),
        ),
        (
            "ntfy-topic-url",
            json!({ "type": "ntfy", "server_url": "https://ntfy.sh/mytopic", "topic": "ops" }),
        ),
        (
            "ntfy-plain-http",
            json!({ "type": "ntfy", "server_url": "http://ntfy.internal", "topic": "ops" }),
        ),
        (
            "gotify-plain-http",
            json!({ "type": "gotify", "server_url": "http://push.internal", "token": "tok" }),
        ),
        (
            "gotify-no-token",
            json!({ "type": "gotify", "server_url": "https://push.example.com", "token": "" }),
        ),
        (
            "mm-plain-http",
            json!({ "type": "mattermost", "webhook_url": "http://mm.internal/hooks/abc" }),
        ),
        (
            "mm-rest-api-url",
            json!({ "type": "mattermost", "webhook_url": "https://mm.example.com/api/v4/posts" }),
        ),
        (
            "mm-unpingable-mention",
            json!({ "type": "mattermost", "webhook_url": "https://mm.example.com/hooks/abc", "mention": "@ops team!" }),
        ),
        (
            "po-bad-user",
            json!({ "type": "pushover", "token": "azGDORePK8gMaC0QOYAMyEEuzJnyUi", "user": "nope" }),
        ),
        (
            "sms-not-e164",
            json!({ "type": "sms", "provider": "twilio", "to": "5551234567", "from": "+15557654321", "account_sid": "AC0123456789ABCDEF0123456789ABCDEF", "auth_token": "tok" }),
        ),
        (
            "sms-bad-sid",
            json!({ "type": "sms", "provider": "twilio", "to": "+15551234567", "from": "+15557654321", "account_sid": "AC123", "auth_token": "tok" }),
        ),
    ] {
        let (st, body) = send(
            &app,
            "POST",
            "/api/v1/notification-channels",
            json!({ "name": name, "config": config }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{name}: {body}");
        assert_eq!(body["error"]["code"], "INVALID_CHANNEL_CONFIG", "{name}");
    }
}

#[tokio::test]
async fn provider_webhook_kinds_reject_offsite_urls() {
    let app = app();
    for (kind, url) in [
        ("slack", "https://example.com/services/T/B/x"),
        ("discord", "https://example.com/api/webhooks/1/x"),
        ("msteams", "https://example.com/workflows/x"),
        ("google_chat", "https://example.com/v1/spaces/A/messages"),
    ] {
        let (st, body) = send(
            &app,
            "POST",
            "/api/v1/notification-channels",
            json!({
                "name": format!("bad-{kind}"),
                "config": { "type": kind, "webhook_url": url }
            }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{kind}: {body}");
        assert_eq!(body["error"]["code"], "INVALID_CHANNEL_CONFIG", "{kind}");
        let msg = body["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("use the webhook type"), "{kind}: {body}");
    }
}

#[tokio::test]
async fn email_channel_starts_unverified_and_blocks_tests() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "oncall-mail", "config": { "type": "email", "to": "oncall@example.com" } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "email");
    assert_eq!(created["config"]["to"], "oncall@example.com");
    assert!(created.get("verified_at").is_none(), "starts unverified");
    let id = created["id"].as_str().unwrap();

    // Stored-channel test refuses until the address is verified.
    let (st, body) = send_empty(
        &app,
        "POST",
        &format!("/api/v1/notification-channels/{id}/test"),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "CHANNEL_UNVERIFIED");

    // Ad-hoc config test can never be verified — refused outright.
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/test",
        json!({ "config": { "type": "email", "to": "anyone@example.com" } }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "CHANNEL_UNVERIFIED");

    // A case difference is not a bad address: it is normalized on the way in,
    // the same as the owner address signup stores.
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "shouty-mail", "config": { "type": "email", "to": " Not-Lower@Example.COM " } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["config"]["to"], "not-lower@example.com", "{body}");

    // Address-shape validation still runs like every transport's.
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "bad-mail", "config": { "type": "email", "to": "no-at-sign.example.com" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "INVALID_CHANNEL_CONFIG");
}

#[tokio::test]
async fn resend_verification_rejects_non_email_channels() {
    let app = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Ops"),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap();
    let (st, body) = send_empty(
        &app,
        "POST",
        &format!("/api/v1/notification-channels/{id}/resend-verification"),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "CHANNEL_NOT_VERIFIABLE");
}

/// Deleting a channel must scrub its bindings from `targets.alerts` — a
/// dangling `{channel_id}` fails channel-existence validation on the next
/// whole-array alerts update of that monitor.
#[tokio::test]
async fn delete_scrubs_alert_bindings_from_targets() {
    let app = app();
    let (_, ch_a) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("a"),
    )
    .await;
    let ch_a = ch_a["id"].as_str().unwrap().to_string();

    let (status, target) = send(
        &app,
        "POST",
        "/api/v1/targets",
        json!({
            "name": "api",
            "interval": 60,
            "alerts": [{ "channel_id": ch_a }],
            "check": { "type": "tcp", "host": "example.com", "port": 443, "timeout": 5000 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create target: {target}");
    let target_id = target["id"].as_str().unwrap().to_string();

    let (status, _) = send_empty(
        &app,
        "DELETE",
        &format!("/api/v1/notification-channels/{ch_a}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send_empty(&app, "GET", &format!("/api/v1/targets/{target_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["alerts"], json!([]), "binding must be scrubbed");

    // The regression this guards: with a dangling binding, any later
    // round-tripped alerts update would 400 on the deleted channel id.
    let (_, ch_b) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("b"),
    )
    .await;
    let (status, body) = send(
        &app,
        "PATCH",
        &format!("/api/v1/targets/{target_id}"),
        json!({ "alerts": [{ "channel_id": ch_b["id"].as_str().unwrap() }] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rebind must succeed: {body}");
}

#[tokio::test]
async fn delete_then_get_is_404() {
    let app = app();
    let (_, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("Ops"),
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let (st, _) = send_empty(
        &app,
        "DELETE",
        &format!("/api/v1/notification-channels/{id}"),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, body) = send_empty(&app, "GET", &format!("/api/v1/notification-channels/{id}")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "CHANNEL_NOT_FOUND");
}

#[tokio::test]
async fn test_config_rejects_invalid_transport() {
    let app = app();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/test",
        json!({ "config": { "type": "slack", "webhook_url": "not a url" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "INVALID_CHANNEL_CONFIG");
}

#[tokio::test]
async fn test_config_rejects_redaction_sentinel() {
    let app = app();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/test",
        json!({ "config": { "type": "slack", "webhook_url": "***" } }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "REDACTION_SENTINEL");
}

#[tokio::test]
async fn test_send_unknown_channel_is_404() {
    let app = app();
    let (st, body) = send_empty(
        &app,
        "POST",
        "/api/v1/notification-channels/01919b9e-0000-7000-8000-000000000000/test",
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "CHANNEL_NOT_FOUND");
}

fn with_bot() -> Router {
    build_test_app_with_owner(|cfg| {
        cfg.telegram.bot_token = "123:abc".to_string().into();
        cfg.telegram.bot_username = "uptimepagebot".into();
        cfg.telegram.webhook_secret = "0123456789abcdef0123456789abcdef".to_string().into();
    })
}

#[tokio::test]
async fn telegram_app_config_is_rejected_from_request_bodies() {
    let app = app();
    let tg = json!({ "type": "telegram_app", "chat_id": "-100123" });
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "tg", "config": tg }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");

    // Swapping an existing channel's config to the managed kind is the same
    // alert-spam vector; PATCH must refuse it too.
    let (_, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("ops"),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let (st, body) = send(
        &app,
        "PATCH",
        &format!("/api/v1/notification-channels/{id}"),
        json!({ "config": tg }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
}

/// A caller-supplied channel id would point our Slack app's presses at a
/// channel the webhook does not post to.
#[tokio::test]
async fn slack_app_config_is_rejected_from_request_bodies() {
    let app = app();
    let slack_app = json!({
        "type": "slack_app",
        "webhook_url": "https://hooks.slack.com/services/T/B/x",
        "channel": "#ops",
        "channel_id": "C0AB12CD3",
        "team_id": "T0AB12CD3",
    });
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "slack", "config": slack_app }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Add to Slack"),
        "{body}"
    );

    let (_, created) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        slack_body("ops"),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let (st, body) = send(
        &app,
        "PATCH",
        &format!("/api/v1/notification-channels/{id}"),
        json!({ "config": slack_app }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
}

#[tokio::test]
async fn telegram_link_is_absent_without_bot() {
    let app = app();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/telegram-link",
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "TELEGRAM_LINK_NOT_FOUND");

    let (st, body) = send_empty(
        &app,
        "GET",
        "/api/v1/notification-channels/telegram-link/01919b9e-0000-7000-8000-000000000000",
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "TELEGRAM_LINK_NOT_FOUND");
}

#[tokio::test]
async fn telegram_link_mint_and_poll_round_trip() {
    let app = with_bot();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/telegram-link",
        json!({ "name": "Ops Telegram" }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let code = body["code"].as_str().unwrap();
    let deep_link = body["deep_link"].as_str().unwrap();
    assert!(deep_link.starts_with("https://t.me/uptimepagebot?start="));
    assert!(deep_link.ends_with(code));
    let group_link = body["group_deep_link"].as_str().unwrap();
    assert!(group_link.starts_with("https://t.me/uptimepagebot?startgroup="));
    assert!(group_link.ends_with(code));
    let id = body["id"].as_str().unwrap();

    let (st, body) = send_empty(
        &app,
        "GET",
        &format!("/api/v1/notification-channels/telegram-link/{id}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["status"], "pending");
    assert!(body.get("channel_id").is_none());

    let (st, body) = send_empty(
        &app,
        "GET",
        "/api/v1/notification-channels/telegram-link/01919b9e-0000-7000-8000-000000000000",
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "TELEGRAM_LINK_NOT_FOUND");
}

#[tokio::test]
async fn telegram_link_mint_caps_outstanding_codes() {
    let app = with_bot();
    for _ in 0..5 {
        let (st, _) = send(
            &app,
            "POST",
            "/api/v1/notification-channels/telegram-link",
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
    }
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/telegram-link",
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "TELEGRAM_LINK_LIMIT");
}

#[tokio::test]
async fn telegram_link_mint_validates_name_hint() {
    let app = with_bot();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/telegram-link",
        json!({ "name": "x".repeat(101) }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "CHANNEL_NAME_INVALID");
}

#[tokio::test]
async fn test_config_rejects_telegram_app_kind() {
    // The ad-hoc test endpoint would message a caller-supplied chat id with
    // the operator bot — the exact spam vector the managed-kind guard closes.
    let app = app();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/test",
        json!({ "config": { "type": "telegram_app", "chat_id": "-100123" } }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
}

// ── resend bounce/complaint webhook ─────────────────────────────────────

const RESEND_SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

fn with_resend_hook() -> Router {
    common::build_test_app_with_web_and_owner(|cfg| {
        cfg.email.resend.webhook_secret = RESEND_SECRET.to_string().into();
    })
}

async fn post_resend_hook(app: &Router, body: &str, signed: bool) -> StatusCode {
    use base64::Engine as _;
    use hmac::{KeyInit, Mac};
    let ts = chrono::Utc::now().timestamp().to_string();
    let sig = if signed {
        let key = base64::engine::general_purpose::STANDARD
            .decode(RESEND_SECRET.strip_prefix("whsec_").unwrap())
            .unwrap();
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
        mac.update(format!("msg_1.{ts}.").as_bytes());
        mac.update(body.as_bytes());
        format!(
            "v1,{}",
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
        )
    } else {
        "v1,bm90LXRoZS1zaWduYXR1cmU=".to_string()
    };
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/hooks/resend")
        .header("svix-id", "msg_1")
        .header("svix-timestamp", &ts)
        .header("svix-signature", sig)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn resend_hard_bounce_and_complaint_disable_matching_email_channels() {
    let app = with_resend_hook();
    let (st, bounced) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "Mail", "config": { "type": "email", "to": "oncall@example.com" } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let bounced_id = bounced["id"].as_str().unwrap().to_string();
    let (st, other) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "Backup", "config": { "type": "email", "to": "backup@example.com" } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let other_id = other["id"].as_str().unwrap().to_string();

    // Unsigned delivery: rejected, nothing disabled.
    let permanent = json!({
        "type": "email.bounced",
        "data": { "to": ["Oncall@Example.COM"], "bounce": { "type": "Permanent" } }
    })
    .to_string();
    assert_eq!(
        post_resend_hook(&app, &permanent, false).await,
        StatusCode::FORBIDDEN
    );
    // Transient bounce: acknowledged untouched.
    let transient = json!({
        "type": "email.bounced",
        "data": { "to": ["oncall@example.com"], "bounce": { "type": "Transient" } }
    })
    .to_string();
    assert_eq!(
        post_resend_hook(&app, &transient, true).await,
        StatusCode::OK
    );
    let (_, got) = send_empty(
        &app,
        "GET",
        &format!("/api/v1/notification-channels/{bounced_id}"),
    )
    .await;
    assert_eq!(got["enabled"], true);

    // Permanent bounce (provider may echo any case) disables only the match.
    assert_eq!(
        post_resend_hook(&app, &permanent, true).await,
        StatusCode::OK
    );
    let (_, got) = send_empty(
        &app,
        "GET",
        &format!("/api/v1/notification-channels/{bounced_id}"),
    )
    .await;
    assert_eq!(got["enabled"], false);
    assert_eq!(got["disabled_reason"], "the email address hard-bounced");
    let (_, untouched) = send_empty(
        &app,
        "GET",
        &format!("/api/v1/notification-channels/{other_id}"),
    )
    .await;
    assert_eq!(untouched["enabled"], true);

    // A spam complaint disables too, with its own note.
    let complaint = json!({
        "type": "email.complained",
        "data": { "to": ["backup@example.com"] }
    })
    .to_string();
    assert_eq!(
        post_resend_hook(&app, &complaint, true).await,
        StatusCode::OK
    );
    let (_, got) = send_empty(
        &app,
        "GET",
        &format!("/api/v1/notification-channels/{other_id}"),
    )
    .await;
    assert_eq!(got["enabled"], false);
    assert_eq!(
        got["disabled_reason"],
        "the recipient reported our mail as spam"
    );

    // Unknown event types are acknowledged without effect.
    let delivered = json!({ "type": "email.delivered", "data": { "to": ["x@example.com"] } });
    assert_eq!(
        post_resend_hook(&app, &delivered.to_string(), true).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn resend_hook_is_absent_without_a_secret() {
    let app = common::build_test_app_with_web_and_owner(|_| {});
    let st = post_resend_hook(&app, "{}", true).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ── whatsapp one-tap linking ────────────────────────────────────────────

const WA_APP_SECRET: &str = "wa-app-secret";
const WA_PHONE_NUMBER_ID: &str = "424242424242";

fn with_whatsapp(web: bool) -> Router {
    let mutate = |cfg: &mut uptimepage::config::AppConfig| {
        cfg.whatsapp_app.enabled = true;
        cfg.whatsapp_app.access_token = "EAAG-test".to_string().into();
        cfg.whatsapp_app.phone_number_id = WA_PHONE_NUMBER_ID.into();
        cfg.whatsapp_app.public_number = "15550001111".into();
        cfg.whatsapp_app.app_secret = WA_APP_SECRET.to_string().into();
        cfg.whatsapp_app.verify_token = "0123456789abcdef0123456789abcdef".to_string().into();
        cfg.whatsapp_app.template_name = "uptime_alert".into();
    };
    if web {
        common::build_test_app_with_web_and_owner(mutate)
    } else {
        build_test_app_with_owner(mutate)
    }
}

fn wa_signed_post(body: &str) -> axum::http::Request<Body> {
    use hmac::{KeyInit, Mac};
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(WA_APP_SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    axum::http::Request::builder()
        .method("POST")
        .uri("/hooks/whatsapp")
        .header("x-hub-signature-256", sig)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn wa_inbound(from: &str, text: &str) -> String {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "1", "changes": [{ "field": "messages", "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15550001111",
                          "phone_number_id": WA_PHONE_NUMBER_ID },
            "contacts": [{ "wa_id": from, "profile": { "name": "Jane" } }],
            "messages": [{ "from": from, "id": "wamid.X", "timestamp": "0",
                           "type": "text", "text": { "body": text } }]
        }}]}]
    })
    .to_string()
}

#[tokio::test]
async fn whatsapp_app_config_is_rejected_from_request_bodies() {
    let app = app();
    let wa = json!({ "type": "whatsapp_app", "phone": "15551234567" });
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels",
        json!({ "name": "wa", "config": wa }),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
}

#[tokio::test]
async fn whatsapp_link_is_absent_without_operator_number() {
    let app = app();
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/whatsapp-link",
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "WHATSAPP_LINK_NOT_FOUND");
}

#[tokio::test]
async fn whatsapp_link_mint_and_poll_round_trip() {
    let app = with_whatsapp(false);
    let (st, body) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/whatsapp-link",
        json!({ "name": "Ops WhatsApp" }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let code = body["code"].as_str().unwrap();
    let deep_link = body["deep_link"].as_str().unwrap();
    assert!(deep_link.starts_with("https://wa.me/15550001111?text="));
    assert!(deep_link.ends_with(code));
    let id = body["id"].as_str().unwrap();

    let (st, body) = send_empty(
        &app,
        "GET",
        &format!("/api/v1/notification-channels/whatsapp-link/{id}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["status"], "pending");
}

#[tokio::test]
async fn whatsapp_webhook_links_then_stop_severs() {
    let app = with_whatsapp(true);

    // Subscribe handshake echoes the challenge only for the right token.
    let ok = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/hooks/whatsapp?hub.mode=subscribe&hub.verify_token=0123456789abcdef0123456789abcdef&hub.challenge=echo-me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let challenge = axum::body::to_bytes(ok.into_body(), 1024).await.unwrap();
    assert_eq!(&challenge[..], b"echo-me");
    let bad = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/hooks/whatsapp?hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=echo-me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::FORBIDDEN);

    // Mint a code, deliver it from a phone, and the channel appears.
    let (st, minted) = send(
        &app,
        "POST",
        "/api/v1/notification-channels/whatsapp-link",
        json!({ "name": "Oncall WA" }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let code = minted["code"].as_str().unwrap().to_string();
    let link_id = minted["id"].as_str().unwrap().to_string();

    // Tampered signature first: rejected, nothing created.
    let tampered = wa_signed_post(&wa_inbound("15551234567", "not-the-code"));
    let (parts, _) = tampered.into_parts();
    let forged =
        axum::http::Request::from_parts(parts, Body::from(wa_inbound("15551234567", &code)));
    let st = app.clone().oneshot(forged).await.unwrap().status();
    assert_eq!(st, StatusCode::FORBIDDEN);

    let st = app
        .clone()
        .oneshot(wa_signed_post(&wa_inbound(
            "15551234567",
            &format!("  {code}  "),
        )))
        .await
        .unwrap()
        .status();
    assert_eq!(st, StatusCode::OK);

    // The consume runs off the request task; poll the link until it flips.
    let mut channel_id = None;
    for _ in 0..50 {
        let (_, body) = send_empty(
            &app,
            "GET",
            &format!("/api/v1/notification-channels/whatsapp-link/{link_id}"),
        )
        .await;
        if body["status"] == "consumed" {
            channel_id = Some(body["channel_id"].as_str().unwrap().to_string());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let channel_id = channel_id.expect("link consumed");
    let (st, ch) = send_empty(
        &app,
        "GET",
        &format!("/api/v1/notification-channels/{channel_id}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ch["name"], "Oncall WA");
    assert_eq!(ch["kind"], "whatsapp_app");
    assert_eq!(ch["config"]["phone"], "15551234567");
    assert_eq!(ch["enabled"], true);

    // A second consume of the same code is a no-op (single use).
    let st = app
        .clone()
        .oneshot(wa_signed_post(&wa_inbound("15559999999", &code)))
        .await
        .unwrap()
        .status();
    assert_eq!(st, StatusCode::OK);

    // `stop` from the linked number severs the channel.
    let st = app
        .clone()
        .oneshot(wa_signed_post(&wa_inbound("15551234567", "STOP")))
        .await
        .unwrap()
        .status();
    assert_eq!(st, StatusCode::OK);
    let mut disabled = false;
    for _ in 0..50 {
        let (_, ch) = send_empty(
            &app,
            "GET",
            &format!("/api/v1/notification-channels/{channel_id}"),
        )
        .await;
        if ch["enabled"] == false {
            assert_eq!(ch["disabled_reason"], "the recipient sent stop on WhatsApp");
            disabled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(disabled, "stop must disable the linked channel");
}
