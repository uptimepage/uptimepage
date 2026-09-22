//! Live-Postgres contract for public status-page subscribers: subscribe →
//! confirm → unsubscribe round-trip, the stateless unsubscribe HMAC, and the
//! fan-out candidate query (verified-only, lookback, per-pair claim).
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations are auto-applied by
//! `pg_pool_from_env` on first connect.

mod common;

use sqlx::PgPool;
use uuid::Uuid;

use uptimepage::domain::{NewSubscriber, SubscriberChannel};
use uptimepage::storage::subscriber_deliveries::{self, INCIDENT_UPDATE, MAINTENANCE};
use uptimepage::storage::subscriber_maintenance;
use uptimepage::storage::subscribers::{self, ConfirmMint};

use common::{drop_test_db, fresh_test_db, open_test_pool, pg_pool_from_env, unique_slug};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

async fn seed_org(pool: &PgPool) -> Uuid {
    let slug = unique_slug("sub-org");
    let row: (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'sub', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn seed_target(pool: &PgPool, org: Uuid) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        r#"INSERT INTO targets (org_id, name, check_spec, interval_secs)
           VALUES ($1, 'mon', '{"type":"http","url":"https://example.com/"}'::jsonb, 60)
           RETURNING id"#,
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn seed_page(pool: &PgPool, org: Uuid) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled) VALUES ($1, $2, 'Acme', true) RETURNING id",
    )
    .bind(org)
    .bind(unique_slug("subpage"))
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn add_component(pool: &PgPool, org: Uuid, page: Uuid, target: Uuid) {
    sqlx::query(
        "INSERT INTO status_page_components (org_id, status_page_id, target_id) VALUES ($1, $2, $3)",
    )
    .bind(org)
    .bind(page)
    .bind(target)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_public_incident(pool: &PgPool, org: Uuid, target: Uuid) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility, public_title)
         VALUES ($1, $2, now(), 'down', 'public', 'Elevated errors')
         RETURNING id",
    )
    .bind(org)
    .bind(target)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn add_update(pool: &PgPool, org: Uuid, incident: Uuid, age_hours: i64) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, posted_at)
         VALUES ($1, $2, 'investigating', 'looking into it', now() - make_interval(hours => $3))
         RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .bind(age_hours as i32)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn seed_internal_incident(pool: &PgPool, org: Uuid, target: Uuid) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility, public_title)
         VALUES ($1, $2, now(), 'down', 'internal', 'Elevated errors')
         RETURNING id",
    )
    .bind(org)
    .bind(target)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn mark_published(pool: &PgPool, org: Uuid, incident: Uuid) {
    sqlx::query(
        "INSERT INTO incident_events (org_id, incident_id, kind, actor_type)
         VALUES ($1, $2, 'published', 'system')",
    )
    .bind(org)
    .bind(incident)
    .execute(pool)
    .await
    .unwrap();
}

async fn add_resolved_update(pool: &PgPool, org: Uuid, incident: Uuid) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message)
         VALUES ($1, $2, 'resolved', 'all clear')
         RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn confirmed_subscriber(pool: &PgPool, org: Uuid, page: Uuid, email: &str) -> Uuid {
    let sub = subscribers::subscribe(
        pool,
        &NewSubscriber {
            status_page_id: page,
            org_id: org,
            channel: SubscriberChannel::Email,
            target: email.into(),
            config: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    let ConfirmMint::Created { token } =
        subscribers::mint_confirm_token(pool, sub.id, org, page, &sub.target)
            .await
            .unwrap()
    else {
        panic!("confirm mint capped");
    };
    subscribers::confirm(pool, &token)
        .await
        .unwrap()
        .expect("confirm");
    sub.id
}

async fn cleanup(pool: &PgPool, org: Uuid) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn subscribe_confirm_unsubscribe_roundtrip() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let page = seed_page(&pool, org).await;
    let email = format!("{}@example.com", unique_slug("rt"));

    let sub = subscribers::subscribe(
        &pool,
        &NewSubscriber {
            status_page_id: page,
            org_id: org,
            channel: SubscriberChannel::Email,
            target: email.clone(),
            config: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    assert!(sub.verified_at.is_none(), "starts pending");

    // Re-subscribe is idempotent: same row, still pending.
    let again = subscribers::subscribe(
        &pool,
        &NewSubscriber {
            status_page_id: page,
            org_id: org,
            channel: SubscriberChannel::Email,
            target: email.clone(),
            config: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    assert_eq!(again.id, sub.id, "re-subscribe folds onto one row");

    let ConfirmMint::Created { token } =
        subscribers::mint_confirm_token(&pool, sub.id, org, page, &sub.target)
            .await
            .unwrap()
    else {
        panic!("mint capped");
    };
    let confirmed = subscribers::confirm(&pool, &token)
        .await
        .unwrap()
        .expect("confirm");
    assert!(confirmed.is_verified());
    // Single-use: a second confirm of the same token loses.
    assert!(subscribers::confirm(&pool, &token).await.unwrap().is_none());

    // Unsubscribe HMAC: right token verifies, a wrong one does not.
    let secret = "test-salt";
    let mac = subscribers::unsubscribe_token(secret, sub.id);
    assert!(subscribers::verify_unsubscribe(secret, sub.id, &mac));
    assert!(!subscribers::verify_unsubscribe(secret, sub.id, "nope"));

    assert!(subscribers::unsubscribe(&pool, sub.id).await.unwrap());
    // Idempotent: a second unsubscribe is a no-op, not an error.
    assert!(!subscribers::unsubscribe(&pool, sub.id).await.unwrap());

    cleanup(&pool, org).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn fanout_lists_verified_recent_public_updates_only() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;

    // Verified subscriber, then a fresh public update — verified_at precedes
    // posted_at so the update is eligible.
    let sub_id = confirmed_subscriber(&pool, org, page, "v@example.com").await;
    // An unverified subscriber on the same page must never be a candidate.
    subscribers::subscribe(
        &pool,
        &NewSubscriber {
            status_page_id: page,
            org_id: org,
            channel: SubscriberChannel::Email,
            target: "pending@example.com".into(),
            config: serde_json::json!({}),
        },
    )
    .await
    .unwrap();

    let incident = seed_public_incident(&pool, org, target).await;
    let fresh = add_update(&pool, org, incident, 0).await;
    let stale = add_update(&pool, org, incident, 48).await; // outside 24h lookback

    let pending = subscribers::list_pending(&pool, 100).await.unwrap();
    let mine: Vec<_> = pending
        .iter()
        .filter(|p| p.subscriber_id == sub_id)
        .collect();
    assert_eq!(
        mine.len(),
        1,
        "only the fresh update, only the verified sub"
    );
    let p = mine[0];
    assert_eq!(p.update_id, fresh);
    assert_ne!(p.update_id, stale);
    assert_eq!(p.incident_title(), "Elevated errors");
    assert_eq!(p.page_name, "Acme");
    assert_eq!(p.phase, "investigating");

    // Claiming is exclusive: the first wins, an immediate re-claim loses.
    assert!(
        subscriber_deliveries::claim(
            &pool,
            p.subscriber_id,
            p.org_id,
            INCIDENT_UPDATE,
            p.update_id,
            ""
        )
        .await
        .unwrap()
    );
    assert!(
        !subscriber_deliveries::claim(
            &pool,
            p.subscriber_id,
            p.org_id,
            INCIDENT_UPDATE,
            p.update_id,
            ""
        )
        .await
        .unwrap()
    );

    // Once marked sent it drops out of the candidate set.
    subscriber_deliveries::mark(
        &pool,
        p.subscriber_id,
        INCIDENT_UPDATE,
        p.update_id,
        "",
        None,
    )
    .await
    .unwrap();
    let after = subscribers::list_pending(&pool, 100).await.unwrap();
    assert!(after.iter().all(|q| q.subscriber_id != sub_id));

    cleanup(&pool, org).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_activated_custom_domain_is_published_only_while_the_plan_sells_one() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub_id = confirmed_subscriber(&pool, org, page, "cd@example.com").await;
    let incident = seed_public_incident(&pool, org, target).await;
    add_update(&pool, org, incident, 0).await;
    sqlx::query(
        "UPDATE status_pages SET custom_domain = $2, custom_domain_verified_at = now() \
         WHERE id = $1",
    )
    .bind(page)
    .bind(format!("{}.example.com", unique_slug("st")))
    .execute(&pool)
    .await
    .unwrap();

    let mine = |pending: Vec<subscribers::PendingUpdate>| {
        pending
            .into_iter()
            .find(|p| p.subscriber_id == sub_id)
            .expect("the update is pending")
    };
    let on_free = mine(subscribers::list_pending(&pool, 100).await.unwrap());
    assert!(
        !on_free.custom_domain_published,
        "a plan without custom domains links the page's own subdomain"
    );

    sqlx::query(
        "UPDATE accounts SET plan_id = 'team' \
         WHERE id = (SELECT account_id FROM organizations WHERE id = $1)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let verified_only = mine(subscribers::list_pending(&pool, 100).await.unwrap());
    assert!(
        !verified_only.custom_domain_published,
        "verification permits a certificate, it does not move a mailed link: \
         the host has completed no handshake yet and mail cannot be recalled"
    );

    sqlx::query("UPDATE status_pages SET custom_domain_activated_at = now() WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();
    let activated = mine(subscribers::list_pending(&pool, 100).await.unwrap());
    assert!(activated.custom_domain_published);

    cleanup(&pool, org).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn unpublished_incident_fans_out_only_the_resolved_closer() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub_id = confirmed_subscriber(&pool, org, page, "u@example.com").await;

    // Published then unpublished: investigating update is suppressed while
    // internal, the resolved closer still reaches subscribers who were notified.
    let incident = seed_internal_incident(&pool, org, target).await;
    mark_published(&pool, org, incident).await;
    let investigating = add_update(&pool, org, incident, 0).await;
    let resolved = add_resolved_update(&pool, org, incident).await;

    // Never published, resolved while internal: subscribers never heard of it,
    // so even the resolved update must not fan out. Its own target — the open
    // incident index allows only one open incident per monitor.
    let target2 = seed_target(&pool, org).await;
    add_component(&pool, org, page, target2).await;
    let silent = seed_internal_incident(&pool, org, target2).await;
    let silent_resolved = add_resolved_update(&pool, org, silent).await;

    let pending = subscribers::list_pending(&pool, 100).await.unwrap();
    let mine: Vec<_> = pending
        .iter()
        .filter(|p| p.subscriber_id == sub_id)
        .map(|p| p.update_id)
        .collect();
    assert!(
        mine.contains(&resolved),
        "resolved closer reaches subscriber"
    );
    assert!(
        !mine.contains(&investigating),
        "internal investigating update stays suppressed"
    );
    assert!(
        !mine.contains(&silent_resolved),
        "never-published incident stays fully silent"
    );

    cleanup(&pool, org).await;
}

async fn seed_maintenance(pool: &PgPool, org: Uuid, start_h: i64, end_h: i64) -> Uuid {
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO maintenance_windows (org_id, title, description, starts_at, ends_at)
         VALUES ($1, 'DB upgrade', 'brief blip', now() + make_interval(hours => $2),
                 now() + make_interval(hours => $3))
         RETURNING id",
    )
    .bind(org)
    .bind(start_h as i32)
    .bind(end_h as i32)
    .fetch_one(pool)
    .await
    .unwrap();
    row.0
}

async fn add_maintenance_component(pool: &PgPool, org: Uuid, mid: Uuid, target: Uuid) {
    sqlx::query(
        "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
         VALUES ($1, $2, $3)",
    )
    .bind(org)
    .bind(mid)
    .bind(target)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn maintenance_fanout_scheduled_and_completed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;

    let sub_id = confirmed_subscriber(&pool, org, page, "m@example.com").await;
    // Backdate the subscription so a window that already ended still counts as
    // "after they subscribed".
    sqlx::query(
        "UPDATE status_page_subscribers SET verified_at = now() - interval '3 hours' WHERE id = $1",
    )
    .bind(sub_id)
    .execute(&pool)
    .await
    .unwrap();

    let future = seed_maintenance(&pool, org, 24, 25).await;
    add_maintenance_component(&pool, org, future, target).await;
    let past = seed_maintenance(&pool, org, -2, -1).await;
    add_maintenance_component(&pool, org, past, target).await;

    let pending = subscriber_maintenance::list_pending(&pool, 100)
        .await
        .unwrap();
    let mine: Vec<_> = pending
        .iter()
        .filter(|m| m.subscriber_id == sub_id)
        .collect();
    assert_eq!(mine.len(), 2, "one scheduled + one completed");
    let scheduled = mine.iter().find(|m| m.maintenance_id == future).unwrap();
    assert_eq!(scheduled.phase, "scheduled");
    let completed = mine.iter().find(|m| m.maintenance_id == past).unwrap();
    assert_eq!(completed.phase, "completed");

    // Claim is exclusive per (subscriber, window, phase).
    assert!(
        subscriber_deliveries::claim(&pool, sub_id, org, MAINTENANCE, future, "scheduled")
            .await
            .unwrap()
    );
    assert!(
        !subscriber_deliveries::claim(&pool, sub_id, org, MAINTENANCE, future, "scheduled")
            .await
            .unwrap()
    );

    subscriber_deliveries::mark(&pool, sub_id, MAINTENANCE, future, "scheduled", None)
        .await
        .unwrap();
    let after = subscriber_maintenance::list_pending(&pool, 100)
        .await
        .unwrap();
    assert!(
        after
            .iter()
            .all(|m| !(m.subscriber_id == sub_id && m.maintenance_id == future)),
        "sent scheduled drops out"
    );
}

fn dispatcher(
    pool: PgPool,
    email: std::sync::Arc<dyn uptimepage::email::EmailSender>,
) -> uptimepage::public_status::subscriber_dispatch::SubscriberDispatcher {
    use uptimepage::public_status::subscriber_dispatch::{
        SubscriberDispatchConfig, SubscriberDispatcher,
    };
    let (http, _) = common::build_test_outbound_and_email();
    SubscriberDispatcher::new(
        pool,
        email,
        http,
        SubscriberDispatchConfig {
            tick_interval: std::time::Duration::from_secs(60),
            batch_limit: 100,
            base_domain: "example.com".into(),
            public_base_url: "https://app.example.com".into(),
            subdomain_routes: true,
            unsubscribe_secret: "dispatch-secret".into(),
            from_address: "status@example.com".into(),
            from_name: "Status".into(),
        },
    )
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn dispatcher_sends_incident_email_end_to_end() {
    // Isolated DB: run_once sweeps globally, so a shared DB would let it claim
    // other parallel tests' pending rows (and vice versa).
    let Some((db_url, db_name)) = fresh_test_db("subdisp").await else {
        return;
    };
    let pool = open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    confirmed_subscriber(&pool, org, page, "e2e@example.com").await;
    let incident = seed_public_incident(&pool, org, target).await;
    add_update(&pool, org, incident, 0).await;

    let mem = std::sync::Arc::new(uptimepage::email::InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.run_once().await.unwrap();

    let sent = mem.sent();
    let mine: Vec<_> = sent
        .iter()
        .filter(|e| e.to.address == "e2e@example.com")
        .collect();
    assert_eq!(mine.len(), 1, "one incident email sent");
    assert!(
        mine[0].template.list_unsubscribe_url().is_some(),
        "carries a one-click unsubscribe url"
    );

    // Idempotent: a second sweep sends nothing new (delivery logged 'sent').
    disp.run_once().await.unwrap();
    let after = mem.sent();
    assert_eq!(
        after
            .iter()
            .filter(|e| e.to.address == "e2e@example.com")
            .count(),
        1,
        "no duplicate send"
    );

    drop(pool);
    drop_test_db(&db_name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn webhook_subscriber_appears_in_pending() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;

    // Webhook subscriber, verified directly (what the confirmation ping does).
    let sub = subscribers::subscribe(
        &pool,
        &NewSubscriber {
            status_page_id: page,
            org_id: org,
            channel: SubscriberChannel::Webhook,
            target: "https://hook.example.com/x".into(),
            config: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    assert!(sub.verified_at.is_none(), "starts pending");
    subscribers::mark_verified(&pool, sub.id).await.unwrap();

    let incident = seed_public_incident(&pool, org, target).await;
    add_update(&pool, org, incident, 0).await;

    let pending = subscribers::list_pending(&pool, 100).await.unwrap();
    let mine = pending
        .iter()
        .find(|p| p.subscriber_id == sub.id)
        .expect("webhook subscriber pending");
    assert_eq!(mine.channel, "webhook");
    assert_eq!(mine.target, "https://hook.example.com/x");

    cleanup(&pool, org).await;
}
