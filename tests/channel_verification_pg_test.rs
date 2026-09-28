//! Live-Postgres contract for email-channel verification: single-use token
//! consume, the per-channel daily mint cap, `set_verified` kind-gating, and
//! the verified_at reset when a config is replaced.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations are auto-applied by
//! `pg_pool_from_env` on first connect.

mod common;

use uptimepage::domain::{
    ChannelConfig, EmailConfig, NewNotificationChannel, NotificationChannelUpdate, OrgId, UserId,
    WriteSource,
};
use uptimepage::storage::channel_verification::{self, MintOutcome, PER_CHANNEL_DAILY_CAP};
use uptimepage::storage::{
    NotificationChannelStore, PgNotificationChannelStore, create_org_with_owner,
};

use common::{make_user, pg_pool_from_env, unique_slug};

async fn one_org(pool: &sqlx::PgPool, tag: &str) -> (OrgId, UserId) {
    let user = make_user(pool, tag).await;
    let org = create_org_with_owner(pool, user, &unique_slug(tag), "T")
        .await
        .unwrap()
        .expect("org")
        .id;
    (org, user)
}

async fn cleanup(pool: &sqlx::PgPool, org: OrgId, user: UserId) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(pool)
        .await;
}

fn email_channel(name: &str, to: &str) -> NewNotificationChannel {
    NewNotificationChannel {
        name: name.into(),
        config: ChannelConfig::Email(EmailConfig { to: to.into() }),
        enabled: true,
        auto_bind_tags: Vec::new(),
        acknowledge_button: true,
    }
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn token_is_single_use_and_checks_address() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, user) = one_org(&pool, "cv-consume").await;
    // Unique address per run: the per-address mint cap is global and a
    // leftover row from an aborted run would otherwise trip it.
    let addr = format!("{}@example.com", unique_slug("cv-consume"));
    let store = PgNotificationChannelStore::new(pool.clone(), None);
    let ch = store
        .create(org, email_channel("mail", &addr), WriteSource::Ui, 10, None)
        .await
        .unwrap();
    assert!(ch.verified_at.is_none());

    let MintOutcome::Created { token } = channel_verification::mint(&pool, org, ch.id, &addr)
        .await
        .unwrap()
    else {
        panic!("mint capped unexpectedly")
    };
    let consumed = channel_verification::consume(&pool, &token)
        .await
        .unwrap()
        .expect("first consume");
    assert_eq!(consumed.channel_id, ch.id);
    assert_eq!(consumed.org_id, org.0);
    assert_eq!(consumed.email, addr);
    // Second consume of the same token loses.
    assert!(
        channel_verification::consume(&pool, &token)
            .await
            .unwrap()
            .is_none()
    );
    // Garbage token is a miss, not an error.
    assert!(
        channel_verification::consume(&pool, "no-such-token")
            .await
            .unwrap()
            .is_none()
    );

    let upd = store.get(org, ch.id).await.unwrap().unwrap().updated_at;
    assert!(store.set_verified(org, ch.id, upd).await.unwrap());
    // A stale snapshot (the channel changed since) must not verify.
    assert!(!store.set_verified(org, ch.id, upd).await.unwrap());
    let got = store.get(org, ch.id).await.unwrap().unwrap();
    assert!(got.verified_at.is_some());
    assert!(!got.awaiting_verification());

    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn mint_cap_and_config_replace_resets_gate() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, user) = one_org(&pool, "cv-cap").await;
    let addr = format!("{}@example.com", unique_slug("cv-cap"));
    let store = PgNotificationChannelStore::new(pool.clone(), None);
    let ch = store
        .create(org, email_channel("mail", &addr), WriteSource::Ui, 10, None)
        .await
        .unwrap();

    for _ in 0..PER_CHANNEL_DAILY_CAP {
        assert!(matches!(
            channel_verification::mint(&pool, org, ch.id, &addr)
                .await
                .unwrap(),
            MintOutcome::Created { .. }
        ));
    }
    assert!(matches!(
        channel_verification::mint(&pool, org, ch.id, &addr)
            .await
            .unwrap(),
        MintOutcome::LimitReached
    ));

    // Verify, then replace the address: the gate must re-arm.
    let upd = store.get(org, ch.id).await.unwrap().unwrap().updated_at;
    assert!(store.set_verified(org, ch.id, upd).await.unwrap());
    let updated = store
        .update(
            org,
            ch.id,
            NotificationChannelUpdate {
                config: Some(ChannelConfig::Email(EmailConfig {
                    to: "b@example.com".into(),
                })),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(updated.verified_at.is_none(), "config replace resets gate");
    // A name-only patch keeps the stamp.
    let upd = store.get(org, ch.id).await.unwrap().unwrap().updated_at;
    assert!(store.set_verified(org, ch.id, upd).await.unwrap());
    let renamed = store
        .update(
            org,
            ch.id,
            NotificationChannelUpdate {
                name: Some("mail-2".into()),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(renamed.verified_at.is_some());
    // Re-submitting the identical config keeps the stamp too.
    let resubmitted = store
        .update(
            org,
            ch.id,
            NotificationChannelUpdate {
                config: Some(ChannelConfig::Email(EmailConfig {
                    to: "b@example.com".into(),
                })),
                ..Default::default()
            },
            WriteSource::Ui,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        resubmitted.verified_at.is_some(),
        "identical config keeps gate"
    );

    // set_verified is email-only.
    let slack = store
        .create(
            org,
            NewNotificationChannel {
                name: "slk".into(),
                config: ChannelConfig::Slack(uptimepage::domain::SlackConfig {
                    webhook_url: "https://hooks.slack.com/x".into(),
                    mention: None,
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
                acknowledge_button: true,
            },
            WriteSource::Ui,
            10,
            None,
        )
        .await
        .unwrap();
    assert!(
        !store
            .set_verified(org, slack.id, slack.updated_at)
            .await
            .unwrap()
    );

    cleanup(&pool, org, user).await;
}
