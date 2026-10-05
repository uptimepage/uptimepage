//! The public one-click stop link: the signed link stops exactly the channel it
//! names and drops its verification, a GET only offers the stop so a mailbox
//! prefetch can't silence alerts, and a forged or unsigned link stops nothing.
//! The in-memory app covers the link; one live-Postgres case runs the stop
//! through the production store.

use crate::common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::domain::{ChannelConfig, EmailConfig, NewNotificationChannel, OrgId, WriteSource};
use uptimepage::storage::NotificationChannelStore;
use uptimepage::storage::notification_channels::channel_stop_token;
use uuid::Uuid;

use common::{build_test_app_with_pg_store_anon_tweaked, pg_pool_from_env};

const SECRET: &str = "alert-channel-stop-test-secret";

struct Rig {
    app: Router,
    channels: Arc<dyn NotificationChannelStore>,
    org: OrgId,
}

fn rig(secret: &str) -> Rig {
    let state =
        common::build_test_app_state(|_| {}).with_alert_channel_stop_secret(secret.to_string());
    let channels = state.notification_channel_store.clone();
    Rig {
        app: uptimepage::build_app_router(state, CancellationToken::new()),
        channels,
        org: common::test_org_id(),
    }
}

async fn pg_rig(pool: PgPool) -> Rig {
    let mut channels = None;
    let (app, org) = build_test_app_with_pg_store_anon_tweaked(
        pool,
        |_| {},
        |state| {
            channels = Some(state.notification_channel_store.clone());
            state.with_alert_channel_stop_secret(SECRET.to_string())
        },
    )
    .await;
    Rig {
        app,
        channels: channels.expect("tweak ran"),
        org,
    }
}

impl Rig {
    async fn channel(&self, to: &str) -> Uuid {
        self.channels
            .create(
                self.org,
                NewNotificationChannel {
                    name: to.into(),
                    config: ChannelConfig::Email(EmailConfig { to: to.into() }),
                    enabled: true,
                    auto_bind_tags: Vec::new(),
                    acknowledge_button: true,
                    resolve_button: false,
                },
                WriteSource::Ui,
                10,
                None,
            )
            .await
            .expect("email channel")
            .id
    }

    async fn verified_channel(&self, to: &str) -> Uuid {
        let id = self.channel(to).await;
        let ch = self.channels.get(self.org, id).await.unwrap().unwrap();
        assert!(
            self.channels
                .set_verified(self.org, id, ch.updated_at)
                .await
                .unwrap()
        );
        id
    }

    async fn enabled(&self, id: Uuid) -> bool {
        self.channels
            .get(self.org, id)
            .await
            .unwrap()
            .expect("channel exists")
            .enabled
    }

    async fn send(&self, method: &str, c: &str, t: &str) -> (StatusCode, String) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/alert-channel/stop?c={c}&t={t}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        (resp.status(), common::body_text(resp).await)
    }

    /// Stops `named` through its signed link and checks that it, and only it,
    /// is off and unverified.
    async fn assert_stops_only(&self, named: Uuid, other: Uuid) {
        let (status, html) = self
            .send(
                "POST",
                &named.to_string(),
                &channel_stop_token(SECRET, named),
            )
            .await;

        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Delivery stopped"));
        let stopped = self.channels.get(self.org, named).await.unwrap().unwrap();
        assert!(!stopped.enabled);
        assert!(stopped.verified_at.is_none(), "re-enabling must re-verify");
        assert_eq!(
            stopped.disabled_reason.as_deref(),
            Some("recipient stopped delivery")
        );
        let untouched = self.channels.get(self.org, other).await.unwrap().unwrap();
        assert!(untouched.enabled);
        assert!(untouched.verified_at.is_some());
    }
}

#[tokio::test]
async fn a_get_only_offers_the_stop() {
    let rig = rig(SECRET);
    let id = rig.channel("oncall@example.com").await;
    let mac = channel_stop_token(SECRET, id);

    let (status, html) = rig.send("GET", &id.to_string(), &mac).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(&format!(
            "action=\"/alert-channel/stop?c={id}&amp;t={mac}\""
        )),
        "the confirmation posts the same signed link back"
    );
    assert!(rig.enabled(id).await, "a prefetch must not stop delivery");
}

#[tokio::test]
async fn a_post_stops_the_named_channel_and_no_other() {
    let rig = rig(SECRET);
    let named = rig.verified_channel("oncall@example.com").await;
    let other = rig.verified_channel("backup@example.com").await;

    rig.assert_stops_only(named, other).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_post_stops_the_named_channel_in_postgres() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let rig = pg_rig(pool.clone()).await;
    let named = rig.verified_channel("oncall@example.com").await;
    let other = rig.verified_channel("backup@example.com").await;

    rig.assert_stops_only(named, other).await;

    common::delete_org_and_account(&pool, rig.org.0).await;
}

#[tokio::test]
async fn a_forged_link_stops_nothing() {
    let rig = rig(SECRET);
    let id = rig.channel("oncall@example.com").await;
    let other = rig.channel("backup@example.com").await;
    let id_str = id.to_string();
    let forged = [
        (id_str.as_str(), "0".repeat(64)),
        (id_str.as_str(), channel_stop_token(SECRET, other)),
        (id_str.as_str(), channel_stop_token("another-secret", id)),
        (id_str.as_str(), String::new()),
        ("not-a-uuid", channel_stop_token(SECRET, id)),
        ("", String::new()),
    ];

    for (c, t) in &forged {
        for method in ["GET", "POST"] {
            let (status, html) = rig.send(method, c, t).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} c={c} t={t}");
            assert!(html.contains("Link invalid"), "{method} c={c} t={t}");
        }
    }
    assert!(rig.enabled(id).await);
    assert!(rig.enabled(other).await);
}

#[tokio::test]
async fn without_a_secret_no_link_is_valid() {
    let rig = rig("");
    let id = rig.channel("oncall@example.com").await;
    let mac = channel_stop_token("", id);

    for method in ["GET", "POST"] {
        let (status, _) = rig.send(method, &id.to_string(), &mac).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
    }
    assert!(rig.enabled(id).await);
}
