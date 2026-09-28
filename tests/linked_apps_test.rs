//! Acknowledging from an app that reports who pressed, end to end on the
//! in-memory app: the central bot's Acknowledge button, the one-time code that
//! links a Telegram account, and the offer that links a Pushover account. A
//! press names a member only through an account they linked, and a link never
//! takes an account from whoever holds it.

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
    ActorType, ChannelConfig, ChannelKind, ExternalId, IncidentAcknowledgement, IncidentState,
    LinkedApp, NewManualIncident, NewNotificationChannel, NotificationChannelUpdate, OrgId,
    TelegramAppConfig, UserId, WriteSource,
};
use uptimepage::security::app_link::{PUSHOVER_OFFER_COOLDOWN, telegram_start_code};
use uptimepage::security::incident_ack::button_data;
use uptimepage::security::sha256_hex;
use uptimepage::storage::Actor;
use uptimepage::storage::linked_apps::{Claimant, LinkOutcome, Linked};
use uuid::Uuid;

const ACK_SECRET: &str = "telegram-button-test-ack-secret";
const LINK_SECRET: &str = "app-link-test-secret";
const WEBHOOK_SECRET: &str = "telegram-webhook-secret-for-linked-app-tests";
const CHAT: i64 = -1_001_234;
const OLENA: i64 = 77;
const TARAS: i64 = 78;

fn account(raw: &str) -> ExternalId {
    uptimepage::security::app_link::external_id(LINK_SECRET, raw)
}

struct Rig {
    app: Router,
    state: AppState,
    org: OrgId,
    incident_id: Uuid,
    channel_id: Uuid,
}

async fn rig() -> Rig {
    let state = common::build_test_app_state(|cfg| {
        cfg.telegram.bot_token = SecretString::from("123:linked-app-test-token");
        cfg.telegram.bot_username = "uptimepagebot".into();
        cfg.telegram.webhook_secret = SecretString::from(WEBHOOK_SECRET);
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
                config: ChannelConfig::TelegramApp(TelegramAppConfig {
                    chat_id: CHAT.to_string(),
                    chat_title: Some("Ops".into()),
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
        .expect("create linked telegram channel");
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

impl Rig {
    fn button(&self, org: OrgId, channel_id: Uuid) -> String {
        button_data(ACK_SECRET, org, self.incident_id, channel_id, 0).expect("episode 0 fits")
    }

    async fn hook(&self, update: Value) -> StatusCode {
        self.app
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
            .unwrap()
            .status()
    }

    async fn press(&self, from: i64, data: &str) {
        let status = self
            .hook(json!({
                "callback_query": {
                    "id": Uuid::now_v7().to_string(),
                    "from": { "id": from, "first_name": "Olena", "username": "olena_k" },
                    "message": { "message_id": 9, "chat": { "id": CHAT, "type": "supergroup" } },
                    "data": data,
                }
            }))
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    async fn start(&self, from: i64, payload: &str) {
        let status = self
            .hook(json!({
                "message": {
                    "message_id": 1,
                    "text": format!("/start {payload}"),
                    "chat": { "id": from, "type": "private" },
                    "from": { "id": from, "first_name": "Olena", "username": "olena_k" },
                }
            }))
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

    /// The webhook answers before it acts, so wait for the list to reach `n`.
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

    /// Long enough for a spawned update to have landed had it been taken.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    async fn resolve(&self, app: LinkedApp, raw: &str) -> Linked {
        self.state
            .linked_app_store
            .resolve(self.org, app, account(raw))
            .await
            .unwrap()
    }

    /// Link a Telegram account the way the bot does, without the webhook.
    async fn link_telegram(&self, user: UserId, from: i64) {
        self.state
            .linked_app_store
            .mint_telegram(user, "seed", Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        let outcome = self
            .state
            .linked_app_store
            .claim(
                LinkedApp::Telegram,
                "seed",
                Claimant::Account {
                    id: account(&from.to_string()),
                    label: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome, LinkOutcome::Linked(user));
    }

    /// Ask for a Telegram link as `user`, returning the `/start` payload.
    async fn telegram_payload(&self, user: UserId) -> String {
        let resp = self
            .send(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/me/linked-apps/telegram")
                    .body(Body::empty())
                    .unwrap(),
                Some(user),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let url = common::body_json(resp).await["url"]
            .as_str()
            .unwrap()
            .to_string();
        let payload = url
            .strip_prefix("https://t.me/uptimepagebot?start=")
            .expect("a bot start link")
            .to_string();
        assert!(telegram_start_code(&payload).is_some(), "{payload}");
        payload
    }

    async fn offer_pushover(&self, key: &str, code: &str, at: chrono::DateTime<Utc>) -> bool {
        self.state
            .linked_app_store
            .offer(
                LinkedApp::Pushover,
                account(key),
                Some("iphone"),
                &sha256_hex(code),
                at,
            )
            .await
            .unwrap()
    }

    async fn send(&self, req: Request<Body>, user: Option<UserId>) -> axum::http::Response<Body> {
        let app = match user {
            Some(u) => common::with_session(self.app.clone(), u, Some(self.org), None),
            None => self.app.clone(),
        };
        app.oneshot(req).await.unwrap()
    }
}

async fn html(resp: axum::http::Response<Body>) -> String {
    String::from_utf8(
        axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn an_unlinked_press_acknowledges_without_a_name() {
    let rig = rig().await;
    rig.press(OLENA, &rig.button(rig.org, rig.channel_id)).await;

    let acks = rig.acks_reaching(1).await;
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].actor_type, ActorType::Telegram);
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
}

#[tokio::test]
async fn each_person_is_listed_once_named_only_through_a_linked_account() {
    let rig = rig().await;
    let olena = UserId(Uuid::now_v7());
    rig.link_telegram(olena, OLENA).await;
    let button = rig.button(rig.org, rig.channel_id);

    rig.press(OLENA, &button).await;
    let acks = rig.acks_reaching(1).await;
    assert_eq!(acks[0].actor_type, ActorType::Telegram);
    assert_eq!(acks[0].actor_id, Some(olena));
    assert!(!acks[0].anonymous);

    rig.press(OLENA, &button).await;
    rig.press(TARAS, &button).await;
    let acks = rig.acks_reaching(2).await;
    assert_eq!(acks[1].actor_id, None, "taras linked nothing");

    rig.press(TARAS + 1, &button).await;
    let acks = rig.acks_reaching(3).await;
    assert_eq!(
        acks.iter().filter(|a| a.anonymous).count(),
        2,
        "two people nobody linked are two acknowledgements"
    );
    rig.press(TARAS, &button).await;
    rig.settle().await;
    assert_eq!(rig.acks().await.len(), 3, "a repeat press adds nobody");
}

#[tokio::test]
async fn a_button_minted_for_another_chat_or_org_takes_nothing() {
    let rig = rig().await;
    rig.press(OLENA, &rig.button(rig.org, Uuid::now_v7())).await;
    rig.press(OLENA, &rig.button(OrgId(Uuid::now_v7()), rig.channel_id))
        .await;
    rig.press(OLENA, "a-forged").await;
    rig.settle().await;
    assert!(rig.acks().await.is_empty());
}

#[tokio::test]
async fn a_chat_its_org_cut_loose_acknowledges_nothing() {
    let rig = rig().await;
    rig.state
        .notification_channel_store
        .disable_by_external_ref(ChannelKind::TelegramApp, &CHAT.to_string(), "stopped")
        .await
        .unwrap();
    rig.press(OLENA, &rig.button(rig.org, rig.channel_id)).await;
    rig.settle().await;
    assert!(rig.acks().await.is_empty());
}

/// Switching the button off withdraws the ones already in the chat, so a room
/// shared with outsiders stops taking incidents at once.
#[tokio::test]
async fn a_chat_with_the_button_switched_off_acknowledges_nothing() {
    let rig = rig().await;
    rig.state
        .notification_channel_store
        .update(
            rig.org,
            rig.channel_id,
            NotificationChannelUpdate {
                acknowledge_button: Some(false),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .expect("channel");
    rig.press(OLENA, &rig.button(rig.org, rig.channel_id)).await;
    rig.settle().await;
    assert!(rig.acks().await.is_empty());
}

#[tokio::test]
async fn unlink_in_the_bot_frees_the_senders_own_account() {
    let rig = rig().await;
    let olena = UserId(Uuid::now_v7());
    rig.link_telegram(olena, OLENA).await;
    let unlink = |from: i64| {
        json!({
            "message": {
                "message_id": 2,
                "text": "/unlink",
                "chat": { "id": from, "type": "private" },
                "from": { "id": from, "first_name": "Taras" },
            }
        })
    };

    assert_eq!(rig.hook(unlink(TARAS)).await, StatusCode::OK);
    rig.settle().await;
    assert_eq!(
        rig.resolve(LinkedApp::Telegram, &OLENA.to_string()).await,
        Linked::Member(olena),
        "someone else's /unlink frees only their own account"
    );

    assert_eq!(rig.hook(unlink(OLENA)).await, StatusCode::OK);
    for _ in 0..100 {
        if rig.resolve(LinkedApp::Telegram, &OLENA.to_string()).await == Linked::Unlinked {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the account was never freed");
}

#[tokio::test]
async fn a_telegram_link_is_spent_by_the_first_account_to_press_start() {
    let rig = rig().await;
    let (olena, taras) = (UserId(Uuid::now_v7()), UserId(Uuid::now_v7()));

    let payload = rig.telegram_payload(olena).await;
    rig.start(OLENA, &payload).await;
    for _ in 0..100 {
        if rig.resolve(LinkedApp::Telegram, &OLENA.to_string()).await != Linked::Unlinked {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        rig.resolve(LinkedApp::Telegram, &OLENA.to_string()).await,
        Linked::Member(olena)
    );
    let linked = rig.state.linked_app_store.for_user(olena).await.unwrap();
    assert_eq!(linked[0].label.as_deref(), Some("@olena_k"));

    rig.start(TARAS, &payload).await;
    rig.settle().await;
    assert_eq!(
        rig.resolve(LinkedApp::Telegram, &TARAS.to_string()).await,
        Linked::Unlinked,
        "a used link links nobody else"
    );

    let theirs = rig.telegram_payload(taras).await;
    rig.start(OLENA, &theirs).await;
    rig.settle().await;
    assert_eq!(
        rig.resolve(LinkedApp::Telegram, &OLENA.to_string()).await,
        Linked::Member(olena),
        "a held account is refused, never moved"
    );
    assert!(
        rig.state
            .linked_app_store
            .for_user(taras)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_pushover_offer_links_once_and_never_moves_a_held_account() {
    let rig = rig().await;
    let (olena, taras) = (UserId(Uuid::now_v7()), UserId(Uuid::now_v7()));
    let now = Utc::now();
    assert!(rig.offer_pushover("ukey", "offer-1", now).await);
    assert!(
        !rig.offer_pushover("ukey", "offer-2", now).await,
        "one offer per cooldown"
    );
    let spend = |code: &str| {
        common::json_request(
            "POST",
            "/api/v1/me/linked-apps/pushover",
            json!({ "code": code }),
        )
    };

    let anonymous = rig.send(spend("offer-1"), None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let linked = rig.send(spend("offer-1"), Some(olena)).await;
    assert_eq!(linked.status(), StatusCode::NO_CONTENT);
    let accounts = rig.state.linked_app_store.for_user(olena).await.unwrap();
    assert_eq!(accounts[0].app, LinkedApp::Pushover);
    assert_eq!(accounts[0].label.as_deref(), Some("iphone"));

    let reused = rig.send(spend("offer-1"), Some(taras)).await;
    assert_eq!(reused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        common::body_json(reused).await["error"]["code"],
        "APP_LINK_INVALID"
    );

    let later = now + PUSHOVER_OFFER_COOLDOWN + chrono::Duration::seconds(1);
    assert!(rig.offer_pushover("ukey", "offer-3", later).await);
    let taken = rig.send(spend("offer-3"), Some(taras)).await;
    assert_eq!(taken.status(), StatusCode::CONFLICT);
    assert_eq!(
        common::body_json(taken).await["error"]["code"],
        "APP_ACCOUNT_TAKEN"
    );
    assert_eq!(
        rig.resolve(LinkedApp::Pushover, "ukey").await,
        Linked::Member(olena)
    );
}

#[tokio::test]
async fn the_offer_page_asks_for_a_sign_in_before_it_offers_anything() {
    let rig = rig().await;
    assert!(rig.offer_pushover("ukey", "page-offer", Utc::now()).await);
    let get = |path: &str| Request::builder().uri(path).body(Body::empty()).unwrap();

    let out = rig.send(get("/link/pushover?c=page-offer"), None).await;
    assert!(out.status().is_redirection());
    let to = out.headers()["location"].to_str().unwrap();
    assert!(to.starts_with("/login?redirect_after="), "{to}");

    let olena = UserId(Uuid::now_v7());
    let page = rig
        .send(get("/link/pushover?c=page-offer"), Some(olena))
        .await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = html(page).await;
    assert!(page.contains("link it"), "{page}");
    assert!(page.contains("iphone"));
    assert!(page.contains(&common::session_email(olena)));

    let unknown = html(
        rig.send(get("/link/pushover?c=never-offered"), Some(olena))
            .await,
    )
    .await;
    assert!(unknown.contains("link invalid"), "{unknown}");
    assert!(!unknown.contains("link it"));
}

#[tokio::test]
async fn unlinking_takes_only_the_callers_own_account() {
    let rig = rig().await;
    let (olena, taras) = (UserId(Uuid::now_v7()), UserId(Uuid::now_v7()));
    rig.link_telegram(olena, OLENA).await;
    let id = rig.state.linked_app_store.for_user(olena).await.unwrap()[0].id;
    let delete = || {
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/me/linked-apps/{id}"))
            .body(Body::empty())
            .unwrap()
    };

    let foreign = rig.send(delete(), Some(taras)).await;
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        rig.state
            .linked_app_store
            .for_user(olena)
            .await
            .unwrap()
            .len(),
        1
    );

    let own = rig.send(delete(), Some(olena)).await;
    assert_eq!(own.status(), StatusCode::NO_CONTENT);
    assert!(
        rig.state
            .linked_app_store
            .for_user(olena)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The Postgres store: a code is spent in the transaction that writes the link,
/// a held account is refused, the offer cooldown holds, and membership is
/// checked where a press lands rather than when the account is linked.
#[tokio::test]
#[ignore]
async fn linking_and_naming_on_postgres_pg() {
    use uptimepage::storage::{LinkedAppStore, PgLinkedAppStore};

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let owner = common::make_user(&pool, "linkedapps").await;
    let org = uptimepage::storage::create_org_with_owner(
        &pool,
        owner,
        &common::unique_slug("linkedapps"),
        "n",
    )
    .await
    .expect("create org")
    .expect("org created")
    .id;
    let outsider = common::make_user(&pool, "linkedappsout").await;
    let store = PgLinkedAppStore::new(pool.clone());
    let telegram = account(&Uuid::now_v7().to_string());
    let press = |id| Claimant::Account {
        id,
        label: Some("@owner"),
    };
    let later = Utc::now() + chrono::Duration::hours(1);
    let code = |c: &str| sha256_hex(&format!("{c}-{}", Uuid::now_v7()));

    let first = code("owner");
    store.mint_telegram(owner, &first, later).await.unwrap();
    assert_eq!(
        store
            .claim(LinkedApp::Telegram, &first, press(telegram))
            .await
            .unwrap(),
        LinkOutcome::Linked(owner)
    );
    assert_eq!(
        store
            .claim(
                LinkedApp::Telegram,
                &first,
                press(account(&Uuid::now_v7().to_string()))
            )
            .await
            .unwrap(),
        LinkOutcome::Invalid,
        "spent"
    );
    assert_eq!(
        store
            .resolve(org, LinkedApp::Telegram, telegram)
            .await
            .unwrap(),
        Linked::Member(owner)
    );

    let theirs = code("outsider");
    store.mint_telegram(outsider, &theirs, later).await.unwrap();
    assert_eq!(
        store
            .claim(LinkedApp::Telegram, &theirs, press(telegram))
            .await
            .unwrap(),
        LinkOutcome::Taken
    );
    let fresh = account(&Uuid::now_v7().to_string());
    assert_eq!(
        store
            .claim(LinkedApp::Telegram, &theirs, press(fresh))
            .await
            .unwrap(),
        LinkOutcome::Linked(outsider),
        "a refusal leaves the code live"
    );
    assert_eq!(
        store
            .resolve(org, LinkedApp::Telegram, fresh)
            .await
            .unwrap(),
        Linked::Outsider,
        "linked, but not by a member of this org"
    );

    let key = account(&Uuid::now_v7().to_string());
    let now = Utc::now();
    let offer = code("offer");
    assert!(
        store
            .offer(LinkedApp::Pushover, key, Some("iphone"), &offer, now)
            .await
            .unwrap()
    );
    assert!(
        !store
            .offer(LinkedApp::Pushover, key, None, &code("again"), now)
            .await
            .unwrap()
    );
    assert_eq!(
        store.offered(LinkedApp::Pushover, &offer).await.unwrap(),
        Some(Some("iphone".to_string()))
    );
    assert_eq!(
        store
            .claim(LinkedApp::Pushover, &offer, Claimant::Person(owner))
            .await
            .unwrap(),
        LinkOutcome::Linked(owner)
    );

    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(owner.0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        store.resolve(org, LinkedApp::Pushover, key).await.unwrap(),
        Linked::Outsider,
        "a closing account is no longer a member"
    );
}
