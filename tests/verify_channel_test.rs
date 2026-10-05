//! The public `/verify-channel` link through the router on live Postgres: the
//! mailed token verifies the address it was sent to, once; a token for an
//! address the channel no longer holds verifies nothing; and every miss is the
//! same invalid page, byte for byte, so the surface answers no "does this
//! token exist".
//!
//! Live-PG ignored: needs `DATABASE_URL`.

use crate::common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use uptimepage::domain::{
    ChannelConfig, EmailConfig, NewNotificationChannel, NotificationChannelUpdate, OrgId,
    WriteSource,
};
use uptimepage::storage::NotificationChannelStore;
use uptimepage::storage::channel_verification::{self, MintOutcome};
use uuid::Uuid;

use common::{build_test_app_with_pg_store_anon_tweaked, pg_pool_from_env, unique_slug};

struct Rig {
    app: Router,
    pool: sqlx::PgPool,
    store: Arc<dyn NotificationChannelStore>,
    org: OrgId,
}

async fn rig() -> Option<Rig> {
    let pool = pg_pool_from_env().await?;
    let mut store = None;
    let (app, org) = build_test_app_with_pg_store_anon_tweaked(
        pool.clone(),
        |_| {},
        |state| {
            store = Some(state.notification_channel_store.clone());
            state
        },
    )
    .await;
    Some(Rig {
        app,
        pool,
        store: store.expect("tweak ran"),
        org,
    })
}

/// Unique per run: the mint cap is per address and global.
fn address(tag: &str) -> String {
    format!("{}@example.com", unique_slug(tag))
}

impl Rig {
    async fn channel(&self, to: &str) -> Uuid {
        self.store
            .create(
                self.org,
                NewNotificationChannel {
                    name: "mail".into(),
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
            .unwrap()
            .id
    }

    async fn token(&self, channel: Uuid, to: &str) -> String {
        match channel_verification::mint(&self.pool, self.org, channel, to)
            .await
            .unwrap()
        {
            MintOutcome::Created { token } => token,
            _ => panic!("mint capped unexpectedly"),
        }
    }

    async fn verified(&self, channel: Uuid) -> bool {
        self.store
            .get(self.org, channel)
            .await
            .unwrap()
            .expect("channel exists")
            .verified_at
            .is_some()
    }

    async fn visit(&self, query: &str) -> (StatusCode, String) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::get(format!("/verify-channel{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        (resp.status(), common::body_text(resp).await)
    }

    /// The page an unknown token gets; every other miss must match it.
    async fn invalid_page(&self) -> String {
        let (status, html) = self
            .visit(&format!("?token={}", Uuid::new_v4().simple()))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        html
    }

    async fn cleanup(&self) {
        common::delete_org_and_account(&self.pool, self.org.0).await;
    }
}

fn encode(token: &str) -> String {
    url::form_urlencoded::byte_serialize(token.as_bytes()).collect()
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_mailed_token_verifies_its_address_once() {
    let Some(rig) = rig().await else {
        return;
    };
    let to = address("vc-once");
    let channel = rig.channel(&to).await;
    let token = rig.token(channel, &to).await;

    let (status, html) = rig.visit(&format!("?token={}", encode(&token))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Address verified"));
    assert!(html.contains(&to));
    assert!(rig.verified(channel).await);

    let (again, html) = rig.visit(&format!("?token={}", encode(&token))).await;
    assert_eq!(again, StatusCode::NOT_FOUND, "a token is single-use");
    assert_eq!(html, rig.invalid_page().await);

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_token_for_a_replaced_address_verifies_nothing() {
    let Some(rig) = rig().await else {
        return;
    };
    let mailed = address("vc-old");
    let channel = rig.channel(&mailed).await;
    let token = rig.token(channel, &mailed).await;
    rig.store
        .update(
            rig.org,
            channel,
            NotificationChannelUpdate {
                config: Some(ChannelConfig::Email(EmailConfig {
                    to: address("vc-new"),
                })),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap();

    let (status, html) = rig.visit(&format!("?token={}", encode(&token))).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(html, rig.invalid_page().await);
    assert!(
        !rig.verified(channel).await,
        "the token proves the old inbox, not the new one"
    );

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn every_miss_is_the_same_invalid_page() {
    let Some(rig) = rig().await else {
        return;
    };
    let invalid = rig.invalid_page().await;

    for query in ["", "?token=", "?token=%20%20"] {
        let (status, html) = rig.visit(query).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{query:?}");
        assert_eq!(
            html, invalid,
            "{query:?} must not differ from an unknown token"
        );
    }

    rig.cleanup().await;
}
