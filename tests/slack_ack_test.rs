//! The Acknowledge and Resolve buttons on an alert sent through our Slack app,
//! end to end on the in-memory app: only a request Slack signed is read, a
//! press lands in the org its button was minted for, and it names a member
//! only through a Slack account they linked from the offer the press brought.
//! Acknowledging takes anyone; resolving takes a member who linked theirs.

mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::domain::{
    ActorType, AlertAction, ChannelConfig, IncidentAcknowledgement, IncidentState, Linked,
    LinkedApp, NewManualIncident, NewNotificationChannel, NotificationChannelUpdate,
    NotificationReason, OrgId, SlackAppConfig, UserId, WriteSource,
};
use uptimepage::notifier::slack::{ACKNOWLEDGE_ACTION, RESOLVE_ACTION};
use uptimepage::security::incident_ack::button_data;
use uptimepage::security::mac::hmac_sha256_hex;
use uptimepage::storage::Actor;
use uuid::Uuid;

const ACK_SECRET: &str = "slack-button-test-ack-secret";
const LINK_SECRET: &str = "app-link-test-secret";
const SIGNING_SECRET: &str = "slack-signing-secret-for-tests";
const SLACK_CHANNEL: &str = "C0OPS0001";
const OLENA: &str = "U0OLENA01";
const TARAS: &str = "U0TARAS01";
/// Fails the reply's host check before anything leaves the test.
const RESPONSE_URL: &str = "https://replies.invalid/actions/1";

struct Rig {
    app: Router,
    state: AppState,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
}

async fn rig() -> Rig {
    rig_with(|state| state).await
}

async fn rig_with(wire: impl FnOnce(AppState) -> AppState) -> Rig {
    let state = wire(
        common::build_test_app_state(|cfg| {
            cfg.slack_interactivity.signing_secret = SecretString::from(SIGNING_SECRET);
        })
        .with_incident_ack_secret(ACK_SECRET.to_string())
        .with_app_link_secret(LINK_SECRET.to_string()),
    );
    let org = common::test_org_id();
    let channel = state
        .notification_channel_store
        .create(
            org,
            NewNotificationChannel {
                name: "Ops".into(),
                config: ChannelConfig::SlackApp(SlackAppConfig {
                    webhook_url: "https://hooks.slack.com/services/T/B/x".into(),
                    channel: "#ops".into(),
                    channel_id: SLACK_CHANNEL.into(),
                    team_id: Some("T0INSTALL1".into()),
                    mention: None,
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button: true,
                resolve_button: false,
            },
            WriteSource::Ui,
            100,
            None,
        )
        .await
        .expect("create slack app channel");
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

fn form(action_id: &str, value: Option<&str>, user: &str, channel: &str) -> String {
    let mut action = json!({ "action_id": action_id, "type": "button" });
    if let Some(v) = value {
        action["value"] = v.into();
    }
    let payload = json!({
        "type": "block_actions",
        "team": { "id": "T0INSTALL1", "domain": "acme" },
        "user": { "id": user, "username": "olena", "team_id": "T0HOME001" },
        "channel": { "id": channel, "name": "ops" },
        "container": { "type": "message", "message_ts": "1700000000.000100", "channel_id": channel },
        "response_url": RESPONSE_URL,
        "actions": [action],
    });
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("payload", &payload.to_string())
        .finish()
}

fn signature(secret: &str, timestamp: i64, body: &str) -> String {
    let ts = timestamp.to_string();
    format!(
        "v0={}",
        hmac_sha256_hex(
            secret.as_bytes(),
            &[b"v0:", ts.as_bytes(), b":", body.as_bytes()]
        )
    )
}

impl Rig {
    fn button(&self, org: OrgId, channel_id: Uuid) -> String {
        self.minted(AlertAction::Acknowledge, org, channel_id)
    }

    fn resolve_button(&self) -> String {
        self.minted(AlertAction::Resolve, self.org, self.channel_id)
    }

    fn minted(&self, action: AlertAction, org: OrgId, channel_id: Uuid) -> String {
        button_data(ACK_SECRET, action, org, self.incident_id, channel_id, 0)
            .expect("episode 0 fits")
    }

    async fn switch_resolve(&self, on: bool) {
        common::switch_resolve(&self.state, self.org, self.channel_id, on).await;
    }

    async fn state_of_incident(&self) -> IncidentState {
        common::ops_incident(&self.state, self.org, self.incident_id)
            .await
            .state
    }

    async fn incident_reaching(&self, state: IncidentState) {
        common::wait_for_incident_state(&self.state, self.org, self.incident_id, state).await;
    }

    async fn link_olena(&self) -> UserId {
        common::link_offered_account(
            &self.app,
            &self.state,
            self.org,
            LinkedApp::Slack,
            self.account(OLENA),
            "slack-offer",
        )
        .await
    }

    async fn post(&self, body: String, timestamp: i64, signature: String) -> StatusCode {
        self.app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/hooks/slack/interactions")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("x-slack-request-timestamp", timestamp.to_string())
                    .header("x-slack-signature", signature)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    async fn signed(&self, body: String) -> StatusCode {
        let now = Utc::now().timestamp();
        let sig = signature(SIGNING_SECRET, now, &body);
        self.post(body, now, sig).await
    }

    async fn press(&self, user: &str, value: &str) {
        let status = self
            .signed(form(ACKNOWLEDGE_ACTION, Some(value), user, SLACK_CHANNEL))
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    async fn press_resolve(&self, user: &str) {
        let status = self
            .signed(form(
                RESOLVE_ACTION,
                Some(&self.resolve_button()),
                user,
                SLACK_CHANNEL,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
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
        uptimepage::security::app_link::external_id(LINK_SECRET, &format!("T0HOME001:{user}"))
    }

    async fn as_owner(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        common::owner_json(self.app.clone(), self.org, method, path, body).await
    }
}

#[tokio::test]
async fn only_a_request_slack_signed_just_now_is_read() {
    let rig = rig().await;
    let body = || {
        form(
            ACKNOWLEDGE_ACTION,
            Some(&rig.button(rig.org, rig.channel_id)),
            OLENA,
            SLACK_CHANNEL,
        )
    };
    let now = Utc::now().timestamp();

    let forged = signature("not-our-secret", now, &body());
    assert_eq!(
        rig.post(body(), now, forged).await,
        StatusCode::UNAUTHORIZED
    );
    let stale = now - 6 * 60;
    let replayed = signature(SIGNING_SECRET, stale, &body());
    assert_eq!(
        rig.post(body(), stale, replayed).await,
        StatusCode::UNAUTHORIZED
    );
    let tampered = signature(SIGNING_SECRET, now, &body());
    assert_eq!(
        rig.post(format!("{}&x=1", body()), now, tampered).await,
        StatusCode::UNAUTHORIZED
    );
    rig.settle().await;
    assert!(rig.acks().await.is_empty());
}

#[tokio::test]
async fn an_unlinked_press_acknowledges_without_a_name_once_per_person() {
    let rig = rig().await;
    let button = rig.button(rig.org, rig.channel_id);
    rig.press(OLENA, &button).await;

    let acks = rig.acks_reaching(1).await;
    assert_eq!(acks[0].actor_type, ActorType::Slack);
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
async fn a_press_names_a_member_through_the_slack_account_they_linked() {
    let rig = rig().await;
    let olena = rig.link_olena().await;
    assert_eq!(
        rig.state
            .linked_app_store
            .resolve(rig.org, LinkedApp::Slack, rig.account(OLENA))
            .await
            .unwrap(),
        Linked::Member(olena)
    );

    rig.press(OLENA, &rig.button(rig.org, rig.channel_id)).await;
    let acks = rig.acks_reaching(1).await;
    assert_eq!(acks[0].actor_type, ActorType::Slack);
    assert_eq!(acks[0].actor_id, Some(olena));
    assert!(!acks[0].anonymous);
}

#[tokio::test]
async fn a_linked_member_resolves_from_the_resolve_button_in_their_own_name() {
    let rig = rig().await;
    rig.switch_resolve(true).await;
    let olena = rig.link_olena().await;

    let status = rig
        .signed(form(
            RESOLVE_ACTION,
            Some(&rig.resolve_button()),
            OLENA,
            SLACK_CHANNEL,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    rig.incident_reaching(IncidentState::Resolved).await;
    let incident = rig
        .state
        .incident_ops_store
        .get(rig.org, rig.incident_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incident.resolved_by, Some(olena));
}

/// The all-clear rides on the engine's resolved signal, so a press that closes
/// the incident sends it, and one that closes nothing does not.
#[tokio::test]
async fn a_resolve_press_tells_the_engine_only_when_it_closed_the_incident() {
    let (tx, mut signals) = tokio::sync::mpsc::channel(8);
    let rig = rig_with(|state| state.with_incident_signals(tx)).await;
    rig.switch_resolve(true).await;

    rig.press_resolve(OLENA).await;
    rig.settle().await;
    assert!(signals.try_recv().is_err(), "nobody linked, nothing closed");

    rig.link_olena().await;
    rig.press_resolve(OLENA).await;
    rig.incident_reaching(IncidentState::Resolved).await;
    let signal = signals
        .recv()
        .await
        .expect("the engine hears of the resolve");
    assert_eq!(
        (signal.org, signal.incident_id, signal.reason),
        (rig.org, rig.incident_id, NotificationReason::Resolved)
    );

    rig.press_resolve(OLENA).await;
    rig.settle().await;
    assert!(
        signals.try_recv().is_err(),
        "a second press adds no all-clear"
    );
}

/// Closing an incident for everyone takes a name on it: an account nobody
/// linked is told how to link it and resolves nothing.
#[tokio::test]
async fn a_press_from_an_account_nobody_linked_cannot_resolve() {
    let rig = rig().await;
    rig.switch_resolve(true).await;
    let status = rig
        .signed(form(
            RESOLVE_ACTION,
            Some(&rig.resolve_button()),
            OLENA,
            SLACK_CHANNEL,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    rig.settle().await;
    assert_eq!(rig.state_of_incident().await, IncidentState::Triggered);
}

/// Resolve has a switch of its own, off until someone turns it on, so a
/// button minted for it does nothing while it is off.
#[tokio::test]
async fn a_resolve_button_does_nothing_while_the_channel_has_it_off() {
    let rig = rig().await;
    rig.link_olena().await;
    rig.press_resolve(OLENA).await;
    rig.settle().await;
    assert_eq!(rig.state_of_incident().await, IncidentState::Triggered);

    rig.switch_resolve(true).await;
    rig.press_resolve(OLENA).await;
    rig.incident_reaching(IncidentState::Resolved).await;
}

/// The signed value says what a press does, not the action id around it: an
/// Acknowledge button carried under Resolve's id still only acknowledges.
#[tokio::test]
async fn an_acknowledge_value_never_resolves_whichever_action_carries_it() {
    let rig = rig().await;
    rig.switch_resolve(true).await;
    rig.link_olena().await;
    let status = rig
        .signed(form(
            RESOLVE_ACTION,
            Some(&rig.button(rig.org, rig.channel_id)),
            OLENA,
            SLACK_CHANNEL,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    rig.incident_reaching(IncidentState::Acknowledged).await;
    rig.settle().await;
    assert_eq!(rig.state_of_incident().await, IncidentState::Acknowledged);
}

#[tokio::test]
async fn a_button_minted_elsewhere_or_a_link_click_takes_nothing() {
    let rig = rig().await;
    rig.press(OLENA, &rig.button(rig.org, Uuid::now_v7())).await;
    rig.press(OLENA, &rig.button(OrgId(Uuid::now_v7()), rig.channel_id))
        .await;
    rig.press(OLENA, "a-forged").await;
    // The right button, pressed in a channel nobody connected.
    let elsewhere = form(
        ACKNOWLEDGE_ACTION,
        Some(&rig.button(rig.org, rig.channel_id)),
        OLENA,
        "C0ELSEWHERE",
    );
    assert_eq!(rig.signed(elsewhere).await, StatusCode::OK);
    // Slack reports clicks on link buttons too; they only need an answer.
    for action in ["view_incident", "acknowledge_page", "resolve_page"] {
        assert_eq!(
            rig.signed(form(action, None, OLENA, SLACK_CHANNEL)).await,
            StatusCode::OK
        );
    }
    rig.settle().await;
    assert!(rig.acks().await.is_empty());
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

/// A caller-supplied channel id would point our Slack app's presses at a
/// channel the webhook does not post to, so an edit changes only the ping and
/// one that names another connection is refused.
#[tokio::test]
async fn an_edit_to_a_connected_channel_changes_only_its_ping() {
    let rig = rig().await;
    let path = format!("/api/v1/notification-channels/{}", rig.channel_id);
    let (_, read) = rig.as_owner("GET", &path, json!({})).await;
    let mut config = read["config"].clone();
    config["mention"] = "@here S01ABC234".into();
    let (status, body) = rig
        .as_owner("PATCH", &path, json!({ "config": config }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["config"]["webhook_url"], "***");
    assert_eq!(body["config"]["channel_id"], SLACK_CHANNEL);
    assert_eq!(body["config"]["mention"], "@here S01ABC234");

    for moved in [
        json!({ "webhook_url": "https://hooks.slack.com/services/T/B/evil" }),
        json!({ "channel": "#elsewhere" }),
        json!({ "channel_id": "C0EVIL0001" }),
        json!({ "team_id": "T0EVIL0001" }),
    ] {
        let mut config = json!({ "type": "slack_app", "mention": "@here" });
        config
            .as_object_mut()
            .unwrap()
            .extend(moved.as_object().unwrap().clone());
        let (status, body) = rig
            .as_owner("PATCH", &path, json!({ "config": config }))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{moved}: {body}");
        assert_eq!(body["error"]["code"], "CHANNEL_KIND_MANAGED");
    }
    let stored = rig
        .state
        .notification_channel_store
        .get(rig.org, rig.channel_id)
        .await
        .unwrap()
        .unwrap();
    let ChannelConfig::SlackApp(cfg) = stored.config else {
        panic!("still a slack_app channel");
    };
    assert_eq!(cfg.webhook_url, "https://hooks.slack.com/services/T/B/x");
    assert_eq!(cfg.channel_id, SLACK_CHANNEL);
    assert_eq!(cfg.mention.as_deref(), Some("@here S01ABC234"));

    // The ping alone, as the form sends it; a bad one is refused.
    let (status, body) = rig
        .as_owner(
            "PATCH",
            &path,
            json!({ "config": { "type": "slack_app", "mention": "@sre" } }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = rig
        .as_owner(
            "PATCH",
            &path,
            json!({ "config": { "type": "slack_app", "mention": "" } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["config"].get("mention").is_none(), "{body}");
    assert_eq!(body["config"]["channel"], "#ops");
    assert_eq!(body["config"]["team_id"], "T0INSTALL1");

    let (status, body) = rig
        .as_owner(
            "PATCH",
            &format!("/api/v1/notification-channels/{}", Uuid::now_v7()),
            json!({ "config": { "type": "slack_app", "mention": "@here" } }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// A test send may carry the ping still being typed, checked like a save and
/// sent on the stored connection only.
#[tokio::test]
async fn a_test_send_takes_an_unsaved_ping_and_nothing_else() {
    let rig = rig().await;
    let test = format!("/api/v1/notification-channels/{}/test", rig.channel_id);
    let refused = [
        (
            json!({ "type": "slack_app", "mention": "@sre" }),
            StatusCode::BAD_REQUEST,
            "INVALID_CHANNEL_CONFIG",
        ),
        (
            json!({ "type": "slack_app", "channel_id": "C0EVIL0001", "mention": "@here" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "CHANNEL_KIND_MANAGED",
        ),
        (
            json!({ "type": "slack", "webhook_url": "https://hooks.slack.com/services/T/B/y" }),
            StatusCode::BAD_REQUEST,
            "INVALID_CHANNEL_CONFIG",
        ),
    ];
    for (config, status_expected, code) in refused {
        let (status, body) = rig
            .as_owner("POST", &test, json!({ "config": config }))
            .await;
        assert_eq!(status, status_expected, "{config}: {body}");
        assert_eq!(body["error"]["code"], code, "{config}: {body}");
    }
    let (status, body) = rig
        .as_owner(
            "POST",
            &test,
            json!({ "config": { "type": "slack_app", "mention": "@here" }, "extra": 1 }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "INVALID_JSON");
}
