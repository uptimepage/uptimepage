//! The Acknowledge button on an alert sent through our Discord app, end to
//! end on the in-memory app: only a request Discord signed is read, a press
//! lands in the org its button was minted for, it names a member only through
//! a Discord account they linked from the offer the press brought, and an
//! edit to a connected channel changes only its ping.

mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::domain::{
    ActorType, ChannelConfig, DiscordAppConfig, IncidentAcknowledgement, IncidentState, Linked,
    LinkedApp, NewManualIncident, NewNotificationChannel, NotificationChannelUpdate, OrgId, UserId,
    WriteSource,
};
use uptimepage::security::incident_ack::button_data;
use uptimepage::security::sha256_hex;
use uptimepage::storage::Actor;
use uuid::Uuid;

const ACK_SECRET: &str = "discord-button-test-ack-secret";
const LINK_SECRET: &str = "app-link-test-secret";
const WEBHOOK: &str = "1112223334445556667";
const OLENA: &str = "1000000000000000001";
const TARAS: &str = "1000000000000000002";
/// Not an application id, so the reply address check fails before anything
/// leaves the test.
const APPLICATION: &str = "not-sent";

fn signer() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

struct Rig {
    app: Router,
    state: AppState,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
}

async fn rig() -> Rig {
    rig_with_key(Some(signer().verifying_key())).await
}

async fn rig_with_key(public_key: Option<VerifyingKey>) -> Rig {
    let state = common::build_test_app_state(|cfg| {
        cfg.discord_interactions.public_key = public_key;
    })
    .with_incident_ack_secret(ACK_SECRET.to_string())
    .with_app_link_secret(LINK_SECRET.to_string());
    let org = common::test_org_id();
    let channel = state
        .notification_channel_store
        .create(
            org,
            NewNotificationChannel {
                name: "Ops".into(),
                config: ChannelConfig::DiscordApp(DiscordAppConfig {
                    webhook_url: format!("https://discord.com/api/webhooks/{WEBHOOK}/tok"),
                    webhook_id: WEBHOOK.into(),
                    mention: None,
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button: true,
            },
            WriteSource::Ui,
            100,
            None,
        )
        .await
        .expect("create discord app channel");
    let incident = state
        .incident_ops_store
        .declare(
            org,
            NewManualIncident {
                title: Some("db unreachable".into()),
                ..Default::default()
            },
            Actor::System,
        )
        .await
        .expect("declare incident");
    Rig {
        app: uptimepage::build_app_router(state.clone(), CancellationToken::new()),
        state,
        org,
        incident_id: incident.id,
        channel_id: channel.id,
    }
}

fn press(custom_id: &str, user: &str, webhook_id: &str) -> String {
    json!({
        "type": 3,
        "id": "1300000000000000001",
        "application_id": APPLICATION,
        "token": "aW50ZXJhY3Rpb24",
        "guild_id": "1100000000000000001",
        "channel_id": "1100000000000000002",
        "member": {
            "nick": null,
            "user": { "id": user, "username": "olena", "global_name": "Olena" }
        },
        "message": { "id": "1300000000000000002", "webhook_id": webhook_id },
        "data": { "component_type": 2, "custom_id": custom_id }
    })
    .to_string()
}

fn signature(key: &SigningKey, timestamp: i64, body: &str) -> String {
    let message = [timestamp.to_string().as_bytes(), body.as_bytes()].concat();
    hex::encode(key.sign(&message).to_bytes())
}

impl Rig {
    fn button(&self, org: OrgId, channel_id: Uuid) -> String {
        button_data(ACK_SECRET, org, self.incident_id, channel_id, 0).expect("episode 0 fits")
    }

    async fn post(&self, body: String, timestamp: i64, signature: String) -> (StatusCode, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hooks/discord/interactions")
                    .header("content-type", "application/json")
                    .header("x-signature-timestamp", timestamp.to_string())
                    .header("x-signature-ed25519", signature)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn signed(&self, body: String) -> (StatusCode, Value) {
        let now = Utc::now().timestamp();
        let sig = signature(&signer(), now, &body);
        self.post(body, now, sig).await
    }

    async fn press(&self, user: &str, custom_id: &str) {
        let (status, answer) = self.signed(press(custom_id, user, WEBHOOK)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer, json!({ "type": 5, "data": { "flags": 64 } }));
    }

    async fn acks(&self) -> Vec<IncidentAcknowledgement> {
        self.state
            .incident_ops_store
            .acknowledgements(self.org, &[self.incident_id])
            .await
            .unwrap()
            .remove(&self.incident_id)
            .unwrap_or_default()
    }

    /// The receiver answers before it acts, so wait for the list to reach `n`.
    async fn acks_reaching(&self, n: usize) -> Vec<IncidentAcknowledgement> {
        for _ in 0..100 {
            let acks = self.acks().await;
            if acks.len() >= n {
                return acks;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("acknowledgements never reached {n}");
    }

    /// Long enough for a spawned press to have landed had it been taken.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    fn account(&self, user: &str) -> uptimepage::domain::ExternalId {
        uptimepage::security::app_link::external_id(LINK_SECRET, user)
    }

    async fn send(&self, req: Request<Body>, user: UserId) -> axum::http::Response<Body> {
        common::with_session(self.app.clone(), user, Some(self.org), None)
            .oneshot(req)
            .await
            .unwrap()
    }

    async fn as_owner(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        common::owner_json(self.app.clone(), self.org, method, path, body).await
    }
}

#[tokio::test]
async fn only_a_request_discord_signed_just_now_is_read() {
    let rig = rig().await;
    let body = || press(&rig.button(rig.org, rig.channel_id), OLENA, WEBHOOK);
    let now = Utc::now().timestamp();

    let forged = signature(&SigningKey::from_bytes(&[9; 32]), now, &body());
    assert_eq!(
        rig.post(body(), now, forged).await.0,
        StatusCode::UNAUTHORIZED
    );
    let stale = now - 6 * 60;
    let replayed = signature(&signer(), stale, &body());
    assert_eq!(
        rig.post(body(), stale, replayed).await.0,
        StatusCode::UNAUTHORIZED
    );
    let tampered = signature(&signer(), now, &body());
    assert_eq!(
        rig.post(format!("{} ", body()), now, tampered).await.0,
        StatusCode::UNAUTHORIZED
    );
    // Discord's own probe: a PING with a bad signature must be refused too.
    assert_eq!(
        rig.post(r#"{"type":1}"#.into(), now, "00".repeat(64))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    rig.settle().await;
    assert!(rig.acks().await.is_empty());
}

#[tokio::test]
async fn a_signed_ping_gets_a_pong() {
    let rig = rig().await;
    assert_eq!(
        rig.signed(r#"{"type":1}"#.into()).await,
        (StatusCode::OK, json!({ "type": 1 }))
    );
}

#[tokio::test]
async fn an_unlinked_press_acknowledges_without_a_name_once_per_person() {
    let rig = rig().await;
    let button = rig.button(rig.org, rig.channel_id);
    rig.press(OLENA, &button).await;

    let acks = rig.acks_reaching(1).await;
    assert_eq!(acks[0].actor_type, ActorType::Discord);
    assert_eq!(acks[0].actor_id, None);
    assert!(acks[0].anonymous);
    let incident = rig
        .state
        .incident_ops_store
        .get(rig.org, rig.incident_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incident.state, IncidentState::Acknowledged);

    rig.press(OLENA, &button).await;
    rig.press(TARAS, &button).await;
    rig.acks_reaching(2).await;
    rig.settle().await;
    assert_eq!(rig.acks().await.len(), 2, "a repeat press adds nobody");
}

#[tokio::test]
async fn a_press_names_a_member_through_the_discord_account_they_linked() {
    let rig = rig().await;
    let olena = UserId(Uuid::now_v7());
    // What an unlinked press offers: a code only its presser sees.
    assert!(
        rig.state
            .linked_app_store
            .offer(
                LinkedApp::Discord,
                rig.account(OLENA),
                Some("olena"),
                &sha256_hex("discord-offer"),
                Utc::now(),
            )
            .await
            .unwrap()
    );
    let page = rig
        .send(
            Request::builder()
                .uri("/link/discord?c=discord-offer")
                .body(Body::empty())
                .unwrap(),
            olena,
        )
        .await;
    assert_eq!(page.status(), StatusCode::OK);
    let linked = rig
        .send(
            common::json_request(
                "POST",
                "/api/v1/me/linked-apps/discord",
                json!({ "code": "discord-offer" }),
            ),
            olena,
        )
        .await;
    assert_eq!(linked.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        rig.state
            .linked_app_store
            .resolve(rig.org, LinkedApp::Discord, rig.account(OLENA))
            .await
            .unwrap(),
        Linked::Member(olena)
    );

    rig.press(OLENA, &rig.button(rig.org, rig.channel_id)).await;
    let acks = rig.acks_reaching(1).await;
    assert_eq!(acks[0].actor_type, ActorType::Discord);
    assert_eq!(acks[0].actor_id, Some(olena));
    assert!(!acks[0].anonymous);
}

#[tokio::test]
async fn a_button_minted_elsewhere_or_under_another_webhook_takes_nothing() {
    let rig = rig().await;
    rig.press(OLENA, &rig.button(rig.org, Uuid::now_v7())).await;
    rig.press(OLENA, &rig.button(OrgId(Uuid::now_v7()), rig.channel_id))
        .await;
    rig.press(OLENA, "a-forged").await;
    // The right button, on a message some other webhook posted.
    let elsewhere = press(
        &rig.button(rig.org, rig.channel_id),
        OLENA,
        "9998887776665554443",
    );
    assert_eq!(rig.signed(elsewhere).await.0, StatusCode::OK);
    rig.settle().await;
    assert!(rig.acks().await.is_empty());
}

#[tokio::test]
async fn every_other_interaction_gets_a_valid_answer_that_does_nothing() {
    let rig = rig().await;
    let (status, answer) = rig.signed(r#"{"type":2,"token":"t"}"#.into()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["type"], 4);
    assert_eq!(answer["data"]["flags"], 64);
    assert_eq!(
        rig.signed(r#"{"type":4}"#.into()).await,
        (
            StatusCode::OK,
            json!({ "type": 8, "data": { "choices": [] } })
        )
    );
    // A component on a message no webhook posted.
    let mut not_ours: Value = serde_json::from_str(&press("x", OLENA, WEBHOOK)).unwrap();
    not_ours["message"] = json!({ "id": "1" });
    let (status, answer) = rig.signed(not_ours.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["type"], 4);
    assert_eq!(
        rig.signed("not json".into()).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_channel_with_the_button_switched_off_or_disabled_takes_nothing() {
    let rig = rig().await;
    let button = rig.button(rig.org, rig.channel_id);
    for update in [
        NotificationChannelUpdate {
            acknowledge_button: Some(false),
            ..Default::default()
        },
        NotificationChannelUpdate {
            acknowledge_button: Some(true),
            enabled: Some(false),
            ..Default::default()
        },
    ] {
        rig.state
            .notification_channel_store
            .update(rig.org, rig.channel_id, update, WriteSource::Ui, None)
            .await
            .unwrap()
            .expect("channel exists");
        rig.press(OLENA, &button).await;
        rig.settle().await;
        assert!(rig.acks().await.is_empty());
    }
}

#[tokio::test]
async fn without_the_public_key_nothing_answers_at_the_hook() {
    let rig = rig_with_key(None).await;
    let (status, _) = rig.signed(r#"{"type":1}"#.into()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A caller-supplied webhook id would point our Discord app's presses at a
/// webhook the alerts never go through, so an edit keeps the connection and
/// changes only the ping, and nothing else may write the kind.
#[tokio::test]
async fn an_edit_to_a_connected_channel_changes_only_its_ping() {
    let rig = rig().await;
    let path = format!("/api/v1/notification-channels/{}", rig.channel_id);
    let (status, body) = rig
        .as_owner(
            "PATCH",
            &path,
            json!({ "config": {
                "type": "discord_app",
                "webhook_url": "https://discord.com/api/webhooks/9998887776665554443/evil",
                "webhook_id": "9998887776665554443",
                "mention": "&123456789012345678",
            } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["webhook_url"], "***");
    assert_eq!(body["config"]["webhook_id"], WEBHOOK);
    assert_eq!(body["config"]["mention"], "&123456789012345678");
    let stored = rig
        .state
        .notification_channel_store
        .get(rig.org, rig.channel_id)
        .await
        .unwrap()
        .unwrap();
    let ChannelConfig::DiscordApp(cfg) = stored.config else {
        panic!("still a discord_app channel");
    };
    assert_eq!(
        cfg.webhook_url,
        format!("https://discord.com/api/webhooks/{WEBHOOK}/tok")
    );

    // The ping alone, as the form sends it; a bad one is refused.
    let (status, body) = rig
        .as_owner(
            "PATCH",
            &path,
            json!({ "config": { "type": "discord_app", "mention": "@sre" } }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let discord_app = json!({
        "type": "discord_app",
        "webhook_url": format!("https://discord.com/api/webhooks/{WEBHOOK}/tok"),
        "webhook_id": WEBHOOK,
    });
    let (status, body) = rig
        .as_owner(
            "POST",
            "/api/v1/notification-channels",
            json!({ "name": "discord", "config": discord_app }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Add to Discord"),
        "{body}"
    );

    let (_, pasted) = rig
        .as_owner(
            "POST",
            "/api/v1/notification-channels",
            json!({ "name": "pasted", "config": {
                "type": "discord",
                "webhook_url": "https://discord.com/api/webhooks/1/tok",
            } }),
        )
        .await;
    let (status, body) = rig
        .as_owner(
            "PATCH",
            &format!(
                "/api/v1/notification-channels/{}",
                pasted["id"].as_str().unwrap()
            ),
            json!({ "config": discord_app }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
}
