mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::Value;
use uptimepage::domain::{
    ChannelConfig, IncidentOrigin, IncidentSeverity, IncidentUrgency, NotificationReason,
    SlackAppConfig, SlackConfig, TelegramConfig, WebhookConfig,
};
use uptimepage::http_outbound::build_outbound_client;
use uptimepage::notifier::event::IncidentNotice;
use uptimepage::notifier::{AlertControls, build_notifier};
use uuid::Uuid;

#[derive(Default, Clone)]
struct CapturedRequest {
    body: Value,
    /// Verbatim request body bytes — the signature covers these exact bytes.
    raw: String,
    headers: std::collections::HashMap<String, String>,
}

async fn spawn_capture_server() -> (SocketAddr, Arc<Mutex<Vec<CapturedRequest>>>) {
    let store: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/hook", post(capture))
        .with_state(store.clone());
    let addr = crate::common::spawn_router(app).await;
    (addr, store)
}

async fn capture(
    State(store): State<Arc<Mutex<Vec<CapturedRequest>>>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> StatusCode {
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let headers = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    store.lock().push(CapturedRequest {
        body: parsed,
        raw: body,
        headers,
    });
    StatusCode::OK
}

fn make_notice() -> IncidentNotice {
    IncidentNotice {
        incident_id: Uuid::now_v7(),
        reason: NotificationReason::Opened,
        monitor_name: Some("demo".into()),
        title: None,
        severity: IncidentSeverity::Major,
        urgency: IncidentUrgency::High,
        origin: IncidentOrigin::Monitor,
        started_at: Utc::now(),
        ended_at: None,
        error_sample: Some("500".into()),
        regions_down: vec![],
        regions_up: vec![],
        url: None,
        note: None,
    }
}

#[tokio::test]
async fn slack_channel_posts_text_payload() {
    let (addr, store) = spawn_capture_server().await;
    let cfg = ChannelConfig::Slack(SlackConfig {
        webhook_url: format!("http://{addr}/hook"),
        mention: None,
    });
    let notifier = build_notifier(
        &cfg,
        &build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
        None,
        None,
        None,
        None,
        AlertControls::default(),
    )
    .expect("notifier");
    notifier
        .notify_incident(&make_notice())
        .await
        .expect("notify");

    let captured = store.lock().clone();
    assert_eq!(captured.len(), 1);
    let text = captured[0]
        .body
        .get("text")
        .and_then(Value::as_str)
        .unwrap();
    assert!(text.contains("OPEN"));
    assert!(text.contains("demo"));
}

/// The channel gets a laid-out card, not a wall of text: the fallback line
/// stays for push notifications, the blocks carry the layout.
#[tokio::test]
async fn slack_channel_posts_block_kit_layout() {
    let (addr, store) = spawn_capture_server().await;
    let cfg = ChannelConfig::Slack(SlackConfig {
        webhook_url: format!("http://{addr}/hook"),
        mention: None,
    });
    let notifier = build_notifier(
        &cfg,
        &build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
        None,
        None,
        None,
        None,
        AlertControls::default(),
    )
    .expect("notifier");
    let mut notice = make_notice();
    notice.url = Some("https://app.test/i/1".into());
    notifier.notify_incident(&notice).await.expect("notify");

    let captured = store.lock().clone();
    let blocks = captured[0].body["blocks"].as_array().expect("blocks");
    assert_eq!(blocks[0]["type"], "header");
    assert!(
        blocks[0]["text"]["text"].as_str().unwrap().contains("demo"),
        "{blocks:?}"
    );
    let button = blocks
        .iter()
        .find(|b| b["type"] == "actions")
        .expect("link button");
    assert_eq!(button["elements"][0]["url"], "https://app.test/i/1");
}

/// The stored mention is a raw token; only the factory turns it into markup,
/// so a wiring slip would deliver inert text instead of a ping.
#[tokio::test]
async fn slack_mention_reaches_the_wire_as_ping_markup() {
    let (addr, store) = spawn_capture_server().await;
    let webhook_url = format!("http://{addr}/hook");
    let mention = Some("@here, S01ABC234".to_string());
    let pasted = ChannelConfig::Slack(SlackConfig {
        webhook_url: webhook_url.clone(),
        mention: mention.clone(),
    });
    let connected = ChannelConfig::SlackApp(SlackAppConfig {
        webhook_url,
        channel: "#ops".into(),
        channel_id: "C0AB12CD3".into(),
        team_id: None,
        mention,
    });
    for cfg in [pasted, connected] {
        let notifier = build_notifier(
            &cfg,
            &build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
            None,
            None,
            None,
            None,
            AlertControls::default(),
        )
        .expect("notifier");
        notifier
            .notify_incident(&make_notice())
            .await
            .expect("notify");

        let captured = store.lock().pop().expect("one send");
        let text = captured.body["text"].as_str().unwrap().to_string();
        assert!(
            text.starts_with("<!here> <!subteam^S01ABC234> "),
            "{:?} mention missing: {text}",
            cfg.kind()
        );
    }
}

#[tokio::test]
async fn slack_multi_region_includes_breakdown() {
    let (addr, store) = spawn_capture_server().await;
    let cfg = ChannelConfig::Slack(SlackConfig {
        webhook_url: format!("http://{addr}/hook"),
        mention: None,
    });
    let notifier = build_notifier(
        &cfg,
        &build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
        None,
        None,
        None,
        None,
        AlertControls::default(),
    )
    .expect("notifier");
    let mut notice = make_notice();
    notice.regions_down = vec!["eu-west".into(), "us-east".into()];
    notice.regions_up = vec!["ap-south".into()];
    notifier.notify_incident(&notice).await.expect("notify");

    let captured = store.lock().clone();
    let text = captured[0].body["text"].as_str().unwrap();
    assert!(text.contains("eu-west"), "breakdown missing: {text}");
    assert!(text.contains("ap-south"), "breakdown missing: {text}");
}

#[tokio::test]
async fn slack_single_region_omits_breakdown() {
    let (addr, store) = spawn_capture_server().await;
    let cfg = ChannelConfig::Slack(SlackConfig {
        webhook_url: format!("http://{addr}/hook"),
        mention: None,
    });
    let notifier = build_notifier(
        &cfg,
        &build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
        None,
        None,
        None,
        None,
        AlertControls::default(),
    )
    .expect("notifier");
    let mut notice = make_notice();
    notice.regions_down = vec!["eu-west".into()];
    notifier.notify_incident(&notice).await.expect("notify");

    let captured = store.lock().clone();
    let text = captured[0].body["text"].as_str().unwrap();
    assert!(
        !text.contains("down:"),
        "single region leaked breakdown: {text}"
    );
}

#[tokio::test]
async fn webhook_channel_posts_incident_payload_with_custom_header() {
    let (addr, store) = spawn_capture_server().await;
    let cfg = ChannelConfig::Webhook(WebhookConfig {
        url: format!("http://{addr}/hook"),
        headers: std::collections::BTreeMap::from([("X-Test-Token".into(), "secret".into())]),
        secret: None,
    });
    let notifier = build_notifier(
        &cfg,
        &build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
        None,
        None,
        None,
        None,
        AlertControls::default(),
    )
    .expect("notifier");
    let mut notice = make_notice();
    notice.regions_down = vec!["eu-west".into()];
    notice.regions_up = vec!["us-east".into()];
    notifier.notify_incident(&notice).await.expect("notify");

    let captured = store.lock().clone();
    assert_eq!(captured.len(), 1);
    let body = &captured[0].body;
    assert_eq!(body["monitor_name"], "demo");
    assert_eq!(body["regions_down"][0], "eu-west");
    assert_eq!(body["regions_up"][0], "us-east");
    // Unsigned channel: no signature headers.
    assert!(!captured[0].headers.contains_key("x-uptimepage-signature"));
}

#[tokio::test]
async fn webhook_signed_delivery_carries_a_verifiable_signature() {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    let (addr, store) = spawn_capture_server().await;
    let secret = "0123456789abcdef-signing-key";
    let cfg = ChannelConfig::Webhook(WebhookConfig {
        url: format!("http://{addr}/hook"),
        headers: std::collections::BTreeMap::new(),
        secret: Some(secret.into()),
    });
    let notifier = build_notifier(
        &cfg,
        &build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
        None,
        None,
        None,
        None,
        AlertControls::default(),
    )
    .expect("notifier");
    notifier
        .notify_incident(&make_notice())
        .await
        .expect("notify");

    let captured = store.lock().clone();
    let req = &captured[0];
    let ts = req
        .headers
        .get("x-uptimepage-timestamp")
        .expect("timestamp header");
    let sig = req
        .headers
        .get("x-uptimepage-signature")
        .expect("signature header");

    // Recompute over the exact "{timestamp}.{raw_body}" the receiver would.
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(ts.as_bytes());
    mac.update(b".");
    mac.update(req.raw.as_bytes());
    let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    assert_eq!(
        sig, &expected,
        "signature must verify against the sent body"
    );
}

#[tokio::test]
async fn build_notifier_constructs_each_kind() {
    let http = build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests());
    assert!(
        build_notifier(
            &ChannelConfig::Telegram(TelegramConfig {
                bot_token: "123:abc".into(),
                chat_id: "-100".into(),
            }),
            &http,
            None,
            None,
            None,
            None,
            AlertControls::default(),
        )
        .is_ok()
    );
    assert!(
        build_notifier(
            &ChannelConfig::Slack(SlackConfig {
                webhook_url: "https://hooks.slack.com/x".into(),
                mention: None,
            }),
            &http,
            None,
            None,
            None,
            None,
            AlertControls::default(),
        )
        .is_ok()
    );
}

#[tokio::test]
async fn build_notifier_rejects_unparseable_url() {
    let http = build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests());
    let err = build_notifier(
        &ChannelConfig::Slack(SlackConfig {
            webhook_url: "not a url".into(),
            mention: None,
        }),
        &http,
        None,
        None,
        None,
        None,
        AlertControls::default(),
    );
    assert!(err.is_err());
}

#[tokio::test]
async fn build_notifier_telegram_app_needs_central_token() {
    use uptimepage::domain::TelegramAppConfig;
    use uptimepage::notifier::CentralTelegram;
    use uptimepage::telegram::TelegramSendBudget;

    let http = build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests());
    let cfg = ChannelConfig::TelegramApp(TelegramAppConfig {
        chat_id: "-100123".into(),
        chat_title: None,
    });
    let budget = std::sync::Arc::new(TelegramSendBudget::new());
    let central = |tok: &'static str| CentralTelegram {
        bot_token: tok,
        budget: &budget,
    };
    assert!(
        build_notifier(
            &cfg,
            &http,
            Some(central("123:abc")),
            None,
            None,
            None,
            AlertControls::default()
        )
        .is_ok()
    );
    // No operator bot → clear error, not a broken send.
    let err = match build_notifier(
        &cfg,
        &http,
        None,
        None,
        None,
        None,
        AlertControls::default(),
    ) {
        Err(e) => e,
        Ok(_) => panic!("token-less telegram_app build must fail"),
    };
    assert!(err.to_string().contains("central bot"));
    // Blank token (misconfig) is treated as absent.
    assert!(
        build_notifier(
            &cfg,
            &http,
            Some(central("  ")),
            None,
            None,
            None,
            AlertControls::default()
        )
        .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn telegram_app_deferred_send_carries_retry_hint() {
    use uptimepage::notifier::transport::Notifier;
    use uptimepage::telegram::TelegramSendBudget;

    let budget = std::sync::Arc::new(TelegramSendBudget::new());
    // Book the group chat solid: four concurrent acquires reserve slots at
    // 0/3/6/9 s before any of them finishes sleeping.
    let mut held = Vec::new();
    for _ in 0..4 {
        let b = budget.clone();
        held.push(tokio::spawn(async move { b.acquire(-5).await }));
    }
    tokio::task::yield_now().await;

    let http = build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests());
    let notifier =
        uptimepage::notifier::telegram::TelegramNotifier::new(http, "123:abc", "-5".into())
            .unwrap()
            .with_budget(budget);
    let notice = make_notice();
    // The next slot is 12 s out — past the wait ceiling, so the send is
    // deferred before any network I/O, with the vendor-hint fragment the
    // escalation engine schedules from.
    let err = notifier
        .notify_incident(&notice)
        .await
        .expect_err("must defer");
    let msg = err.to_string();
    assert!(msg.contains(r#""retry_after":"#), "{msg}");
    assert!(msg.contains("send deferred"), "{msg}");
}
