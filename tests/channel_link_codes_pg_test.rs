//! Live-Postgres contract for `channel_link_codes`: the single-use consume
//! race, expiry, org scoping of the status poll, the outstanding-codes cap,
//! and the consume-path channel create (name-collision suffix + the
//! `telegram_app` kind passing the CHECK constraint).
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations are auto-applied by
//! `pg_pool_from_env` on first connect — point it at a throwaway DB to also
//! validate the migrations themselves.

mod common;

use chrono::{Duration, Utc};
use uptimepage::domain::{
    ChannelConfig, NewNotificationChannel, OrgId, TelegramAppConfig, UserId, WriteSource,
};
use uptimepage::error::AppError;
use uptimepage::error::codes;
use uptimepage::storage::{
    ChannelLinkCodeStore, LinkCodeStatus, LinkPurpose, MintOutcome, NotificationChannelStore,
    PgChannelLinkCodeStore, PgNotificationChannelStore, create_org_with_owner,
};
use uptimepage::web::views::notification_channels::{QuotaBlockLog, create_channel_deduped};

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

async fn cleanup(pool: &sqlx::PgPool, orgs: &[OrgId], users: &[UserId]) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(orgs.iter().map(|o| o.0).collect::<Vec<_>>())
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(users.iter().map(|u| u.0).collect::<Vec<_>>())
        .execute(pool)
        .await;
}

async fn mint(
    store: &PgChannelLinkCodeStore,
    org: OrgId,
    user: UserId,
    hash: &str,
    name: Option<&str>,
) -> uptimepage::storage::LinkCode {
    match store
        .mint(
            org,
            LinkPurpose::Telegram,
            Some(user),
            hash,
            name,
            None,
            Utc::now() + Duration::minutes(15),
            5,
        )
        .await
        .unwrap()
    {
        MintOutcome::Created(c) => c,
        MintOutcome::LimitReached => panic!("unexpected limit"),
    }
}

fn app_config(chat_id: &str) -> ChannelConfig {
    ChannelConfig::TelegramApp(TelegramAppConfig {
        chat_id: chat_id.into(),
        chat_title: Some("Ops".into()),
    })
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn consume_is_single_use_under_race() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, user) = one_org(&pool, "tg-race").await;
    let store = std::sync::Arc::new(PgChannelLinkCodeStore::new(pool.clone()));
    let hash = unique_slug("tg-race-hash");
    mint(&store, org, user, &hash, None).await;

    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let hash = hash.clone();
            tokio::spawn(async move { store.consume(LinkPurpose::Telegram, &hash).await.unwrap() })
        })
        .collect();
    let mut winners = 0;
    for t in tasks {
        if let Some(link) = t.await.unwrap() {
            assert_eq!(link.org_id, org);
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "exactly one concurrent consume must win");

    cleanup(&pool, &[org], &[user]).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn expired_code_neither_consumes_nor_polls_pending() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, user) = one_org(&pool, "tg-exp").await;
    let store = PgChannelLinkCodeStore::new(pool.clone());
    let hash = unique_slug("tg-exp-hash");
    let code = match store
        .mint(
            org,
            LinkPurpose::Telegram,
            Some(user),
            &hash,
            None,
            None,
            Utc::now() - Duration::minutes(1),
            5,
        )
        .await
        .unwrap()
    {
        MintOutcome::Created(c) => c,
        MintOutcome::LimitReached => panic!("unexpected limit"),
    };

    assert!(
        store
            .consume(LinkPurpose::Telegram, &hash)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.status(org, code.id).await.unwrap(),
        Some(LinkCodeStatus::Expired)
    );

    cleanup(&pool, &[org], &[user]).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn status_poll_is_org_scoped_and_transitions() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, user_a) = one_org(&pool, "tg-scope-a").await;
    let (org_b, user_b) = one_org(&pool, "tg-scope-b").await;
    let store = PgChannelLinkCodeStore::new(pool.clone());
    let channels = PgNotificationChannelStore::new(pool.clone(), None);
    let hash = unique_slug("tg-scope-hash");
    let code = mint(&store, org_a, user_a, &hash, Some("Ops Telegram")).await;

    assert_eq!(
        store.status(org_a, code.id).await.unwrap(),
        Some(LinkCodeStatus::Pending)
    );
    // The other org cannot observe the code at all.
    assert_eq!(store.status(org_b, code.id).await.unwrap(), None);

    let link = store
        .consume(LinkPurpose::Telegram, &hash)
        .await
        .unwrap()
        .expect("claim");
    assert_eq!(link.org_id, org_a);
    assert_eq!(link.channel_name.as_deref(), Some("Ops Telegram"));
    // Claimed but no channel attached yet → dead from the poll's view.
    assert_eq!(
        store.status(org_a, code.id).await.unwrap(),
        Some(LinkCodeStatus::Expired)
    );

    let channel = channels
        .create(
            org_a,
            NewNotificationChannel {
                name: "Ops Telegram".into(),
                config: app_config("-100123"),
                enabled: true,
                auto_bind_tags: Vec::new(),
            },
            WriteSource::Ui,
            10,
            None,
        )
        .await
        .unwrap();
    store.attach_channel(link.id, channel.id).await.unwrap();
    assert_eq!(
        store.status(org_a, code.id).await.unwrap(),
        Some(LinkCodeStatus::Consumed {
            channel_id: channel.id
        })
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn mint_caps_outstanding_codes_per_org() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, user) = one_org(&pool, "tg-cap").await;
    let store = PgChannelLinkCodeStore::new(pool.clone());
    let hashes: Vec<String> = (0..5)
        .map(|i| unique_slug(&format!("tg-cap-{i}")))
        .collect();
    for h in &hashes {
        mint(&store, org, user, h, None).await;
    }
    assert!(matches!(
        store
            .mint(
                org,
                LinkPurpose::Telegram,
                Some(user),
                &unique_slug("tg-cap-over"),
                None,
                None,
                Utc::now() + Duration::minutes(15),
                5,
            )
            .await
            .unwrap(),
        MintOutcome::LimitReached
    ));
    // Consuming one frees a slot.
    store
        .consume(LinkPurpose::Telegram, &hashes[0])
        .await
        .unwrap()
        .unwrap();
    mint(&store, org, user, &unique_slug("tg-cap-freed"), None).await;

    cleanup(&pool, &[org], &[user]).await;
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn consume_path_channel_create_suffixes_taken_names() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, user) = one_org(&pool, "tg-name").await;
    let channels = PgNotificationChannelStore::new(pool.clone(), None);

    let first = create_channel_deduped(&channels, org, "Ops", app_config("-1"), 10, no_block_log())
        .await
        .unwrap();
    assert_eq!(first.name, "Ops");
    assert_eq!(first.config, app_config("-1"));

    let second =
        create_channel_deduped(&channels, org, "Ops", app_config("-2"), 10, no_block_log())
            .await
            .unwrap();
    assert_eq!(second.name, "Ops 2");

    // The quota error is not swallowed by the suffix loop, and the breach
    // lands in the quota_events sample stream.
    let err = create_channel_deduped(
        &channels,
        org,
        "Other",
        app_config("-3"),
        2,
        QuotaBlockLog {
            db: Some(pool.clone()),
            user: None,
            flow: "telegram_link",
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, AppError::Unprocessable { code, .. } if code == codes::CHANNEL_QUOTA_EXCEEDED)
    );
    let mut event_rows = 0i64;
    for _ in 0..50 {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM quota_events \
             WHERE org_id = $1 AND event = 'quota_exceeded' \
               AND quota_name = 'max_notification_channels' \
               AND details->>'flow' = 'telegram_link'",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        event_rows = n;
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(event_rows, 1, "the blocked create writes one sample row");

    cleanup(&pool, &[org], &[user]).await;
}

fn no_block_log() -> QuotaBlockLog {
    QuotaBlockLog {
        db: None,
        user: None,
        flow: "test",
    }
}
