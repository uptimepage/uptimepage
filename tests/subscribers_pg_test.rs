//! Live-Postgres contract for public status-page subscribers: subscribe →
//! confirm → unsubscribe round-trip, the stateless unsubscribe HMAC, and the
//! fan-out candidate query (verified-only, lookback, per-pair claim).
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations are auto-applied by
//! `pg_pool_from_env` on first connect.

use crate::common;

use sqlx::PgPool;
use uuid::Uuid;

use uptimepage::domain::{NewSubscriber, SubscriberChannel};
use uptimepage::storage::subscriber_deliveries::{
    self, INCIDENT_UPDATE, MAINTENANCE, MAINTENANCE_UPDATE,
};
use uptimepage::storage::subscriber_maintenance;
use uptimepage::storage::subscribers::{self, ConfirmMint};

use common::{drop_test_db, fresh_test_db, open_test_pool, pg_pool_from_env, unique_slug};

use crate::common::MIGRATOR;

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
    assert_eq!(
        p.page_name, "sub",
        "no display name: the org's, as on the page"
    );
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

    // A send held back by pacing never uses up an attempt: it is claimed
    // again past the cap.
    for _ in 0..=uptimepage::storage::subscribers::FANOUT_MAX_ATTEMPTS {
        subscriber_deliveries::mark(
            &pool,
            p.subscriber_id,
            INCIDENT_UPDATE,
            p.update_id,
            "",
            Some("email send budget unavailable on this instance"),
            false,
        )
        .await
        .unwrap();
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
    }

    // Once marked sent it drops out of the candidate set.
    subscriber_deliveries::mark(
        &pool,
        p.subscriber_id,
        INCIDENT_UPDATE,
        p.update_id,
        "",
        None,
        true,
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

/// An incident with no monitor has no component to route by, so its updates
/// reach a page's subscribers only when it was posted to that page.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_page_incident_reaches_only_the_subscribers_of_its_pages() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let posted_page = seed_page(&pool, org).await;
    let other_page = seed_page(&pool, org).await;
    let told = confirmed_subscriber(&pool, org, posted_page, "told@example.com").await;
    let untold = confirmed_subscriber(&pool, org, other_page, "untold@example.com").await;

    let incident: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, started_at, status_at_start, origin, visibility, public_title)
         VALUES ($1, now(), 'down', 'manual', 'public', 'Network outage in Frankfurt')
         RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO incident_status_pages (org_id, incident_id, status_page_id) VALUES ($1, $2, $3)",
    )
    .bind(org)
    .bind(incident)
    .bind(posted_page)
    .execute(&pool)
    .await
    .unwrap();
    let update = add_update(&pool, org, incident, 0).await;

    let pending = subscribers::list_pending(&pool, 1000).await.unwrap();
    let mine: Vec<_> = pending.iter().filter(|p| p.update_id == update).collect();
    assert_eq!(mine.len(), 1, "one delivery, to the posted page only");
    assert_eq!(mine[0].subscriber_id, told);
    assert_eq!(mine[0].incident_title(), "Network outage in Frankfurt");
    assert!(pending.iter().all(|p| p.subscriber_id != untold));

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
async fn a_cancelled_maintenance_window_is_not_fanned_out() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub_id = confirmed_subscriber(&pool, org, page, "cancelled@example.com").await;
    let window = seed_maintenance(&pool, org, 24, 25).await;
    add_maintenance_component(&pool, org, window, target).await;

    let pending_for = |pending: Vec<subscriber_maintenance::PendingMaintenance>| {
        pending
            .into_iter()
            .filter(|m| m.subscriber_id == sub_id)
            .count()
    };
    assert_eq!(
        pending_for(
            subscriber_maintenance::list_pending(&pool, 100)
                .await
                .unwrap()
        ),
        1,
        "the announced window is pending"
    );

    sqlx::query("UPDATE maintenance_windows SET deleted_at = now() WHERE id = $1")
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        pending_for(
            subscriber_maintenance::list_pending(&pool, 100)
                .await
                .unwrap()
        ),
        0,
        "a cancelled window is no longer announced"
    );
    cleanup(&pool, org).await;
}

async fn pending_counts(pool: &PgPool, sub: Uuid) -> (usize, usize) {
    let updates = subscribers::list_pending(pool, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.subscriber_id == sub)
        .count();
    let windows = subscriber_maintenance::list_pending(pool, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.subscriber_id == sub)
        .count();
    (updates, windows)
}

/// A page that 404s (Off, held, or its org deleted) mails its subscribers
/// nothing about incidents or maintenance until it is served again.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_page_that_is_not_served_mails_its_subscribers_nothing() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "quiet@example.com").await;
    let incident = seed_public_incident(&pool, org, target).await;
    add_update(&pool, org, incident, 0).await;
    let window = seed_maintenance(&pool, org, 24, 25).await;
    add_maintenance_component(&pool, org, window, target).await;

    assert_eq!(
        pending_counts(&pool, sub).await,
        (1, 1),
        "a served page mails"
    );

    for (state, sql, id) in [
        (
            "Off",
            "UPDATE status_pages SET enabled = false WHERE id = $1",
            page,
        ),
        (
            "held",
            "UPDATE status_pages SET plan_hold_at = now() WHERE id = $1",
            page,
        ),
        (
            "deleted org",
            "UPDATE organizations SET deleted_at = now() WHERE id = $1",
            org,
        ),
    ] {
        sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
        assert_eq!(
            pending_counts(&pool, sub).await,
            (0, 0),
            "{state}: mails nobody"
        );
        sqlx::query("UPDATE status_pages SET enabled = true, plan_hold_at = NULL WHERE id = $1")
            .bind(page)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE organizations SET deleted_at = NULL WHERE id = $1")
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            pending_counts(&pool, sub).await,
            (1, 1),
            "{state}: served again"
        );
    }

    cleanup(&pool, org).await;
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

    subscriber_deliveries::mark(&pool, sub_id, MAINTENANCE, future, "scheduled", None, true)
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

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn maintenance_fanout_reaches_back_only_the_lookback_for_completed_windows() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;

    let sub_id = confirmed_subscriber(&pool, org, page, "lookback@example.com").await;
    sqlx::query(
        "UPDATE status_page_subscribers SET verified_at = now() - interval '5 days' WHERE id = $1",
    )
    .bind(sub_id)
    .execute(&pool)
    .await
    .unwrap();

    let inside = seed_maintenance(&pool, org, -24, -23).await;
    add_maintenance_component(&pool, org, inside, target).await;
    let outside = seed_maintenance(&pool, org, -26, -25).await;
    add_maintenance_component(&pool, org, outside, target).await;

    let pending = subscriber_maintenance::list_pending(&pool, 100)
        .await
        .unwrap();
    let reached: Vec<Uuid> = pending
        .iter()
        .filter(|m| m.subscriber_id == sub_id)
        .map(|m| m.maintenance_id)
        .collect();
    assert_eq!(reached, [inside]);
    cleanup(&pool, org).await;
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
async fn a_german_page_mails_its_subscribers_in_german() {
    let Some((db_url, db_name)) = fresh_test_db("subde").await else {
        return;
    };
    let pool = open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    sqlx::query("UPDATE status_pages SET public_locale = 'de' WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "de@example.com").await;
    assert_eq!(
        uptimepage::storage::subscribers::page_locale(&pool, sub)
            .await
            .unwrap(),
        Some(uptimepage::domain::Locale::De),
        "unsubscribe pages follow the subscriber's page"
    );
    let incident = seed_public_incident(&pool, org, target).await;
    add_update(&pool, org, incident, 0).await;

    let mem = std::sync::Arc::new(uptimepage::email::InMemoryEmailSender::new());
    dispatcher(pool.clone(), mem.clone())
        .run_once()
        .await
        .unwrap();

    let sent = mem.sent();
    let mail = sent
        .iter()
        .find(|e| e.to.address == "de@example.com")
        .expect("German subscriber mailed");
    let rendered = mail.template.render("Uptimepage");
    assert!(
        rendered.subject.ends_with("Untersuchung läuft"),
        "{}",
        rendered.subject
    );
    assert!(rendered.html_body.contains(r#"<html lang="de">"#));

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

async fn post_maintenance_update(pool: &PgPool, org: Uuid, window: Uuid, message: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO maintenance_updates (org_id, maintenance_id, phase, message)
         VALUES ($1, $2, 'in_progress', $3) RETURNING id",
    )
    .bind(org)
    .bind(window)
    .bind(message)
    .fetch_one(pool)
    .await
    .expect("insert maintenance update")
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_late_update_reaches_a_page_whose_monitor_left_the_window_before_the_page_had_it() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    let sub_id = confirmed_subscriber(&pool, org, page, "left@example.com").await;
    sqlx::query(
        "UPDATE status_page_subscribers SET verified_at = now() - interval '3 hours' WHERE id = $1",
    )
    .bind(sub_id)
    .execute(&pool)
    .await
    .unwrap();

    // The window dropped the monitor before the page had it, and has ended.
    let window = seed_maintenance(&pool, org, -2, -1).await;
    sqlx::query(
        "INSERT INTO maintenance_window_departures (org_id, maintenance_id, target_id, left_at)
         VALUES ($1, $2, $3, now() - interval '90 minutes')",
    )
    .bind(org)
    .bind(window)
    .bind(target)
    .execute(&pool)
    .await
    .unwrap();
    add_component(&pool, org, page, target).await;
    let late = post_maintenance_update(&pool, org, window, "How it went.").await;

    let owed: Vec<Uuid> = subscriber_maintenance::list_pending_updates(&pool, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|u| u.subscriber_id == sub_id)
        .map(|u| u.update_id)
        .collect();
    assert_eq!(
        owed,
        [late],
        "the page shows the window, so it hears the update"
    );
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_maintenance_update_reaches_each_subscriber_once_and_keeps_reaching_a_page_that_kept_it()
{
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub_id = confirmed_subscriber(&pool, org, page, "upd@example.com").await;
    sqlx::query(
        "UPDATE status_page_subscribers SET verified_at = now() - interval '3 hours' WHERE id = $1",
    )
    .bind(sub_id)
    .execute(&pool)
    .await
    .unwrap();

    let window = seed_maintenance(&pool, org, -1, 1).await;
    add_maintenance_component(&pool, org, window, target).await;
    let update = post_maintenance_update(&pool, org, window, "Running late.").await;
    let cancelled = seed_maintenance(&pool, org, -1, 1).await;
    add_maintenance_component(&pool, org, cancelled, target).await;
    let silent = post_maintenance_update(&pool, org, cancelled, "never sent").await;
    sqlx::query("UPDATE maintenance_windows SET deleted_at = now() WHERE id = $1")
        .bind(cancelled)
        .execute(&pool)
        .await
        .unwrap();

    let owed = |pending: Vec<subscriber_maintenance::PendingMaintenanceUpdate>| -> Vec<Uuid> {
        pending
            .into_iter()
            .filter(|u| u.subscriber_id == sub_id)
            .map(|u| u.update_id)
            .collect()
    };
    let mine = owed(
        subscriber_maintenance::list_pending_updates(&pool, 100)
            .await
            .unwrap(),
    );
    assert_eq!(
        mine,
        [update],
        "a cancelled window's update stays silent: {silent}"
    );

    // The monitor leaves the page; the page kept the window, so a later
    // update still reaches its subscribers.
    sqlx::query("DELETE FROM status_page_components WHERE status_page_id = $1 AND target_id = $2")
        .bind(page)
        .bind(target)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        owed(
            subscriber_maintenance::list_pending_updates(&pool, 100)
                .await
                .unwrap()
        )
        .is_empty(),
        "a page that never kept the window hears nothing"
    );
    sqlx::query(
        "INSERT INTO maintenance_status_pages
             (org_id, maintenance_id, status_page_id, target_ref, component_name)
         VALUES ($1, $2, $3, $4, 'API')",
    )
    .bind(org)
    .bind(window)
    .bind(page)
    .bind(target)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        owed(
            subscriber_maintenance::list_pending_updates(&pool, 100)
                .await
                .unwrap()
        )
        .is_empty(),
        "a kept window is history: nothing while it runs"
    );
    sqlx::query("UPDATE maintenance_windows SET ends_at = clock_timestamp() WHERE id = $1")
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();
    let closing = post_maintenance_update(&pool, org, window, "Finished.").await;
    assert_eq!(
        owed(
            subscriber_maintenance::list_pending_updates(&pool, 100)
                .await
                .unwrap()
        ),
        [closing],
        "only what was said once it ended: {update} stays unsent"
    );

    let ended = seed_maintenance(&pool, org, -2, -1).await;
    add_maintenance_component(&pool, org, ended, target).await;
    sqlx::query(
        "INSERT INTO maintenance_status_pages
             (org_id, maintenance_id, status_page_id, target_ref, component_name)
         VALUES ($1, $2, $3, $4, 'API')",
    )
    .bind(org)
    .bind(ended)
    .bind(page)
    .bind(target)
    .execute(&pool)
    .await
    .unwrap();
    let notices = subscriber_maintenance::list_pending(&pool, 100)
        .await
        .unwrap();
    assert!(
        notices.iter().any(|m| m.subscriber_id == sub_id
            && m.maintenance_id == ended
            && m.phase == "completed"),
        "a page that kept the window is told it completed"
    );

    assert!(
        subscriber_deliveries::claim(&pool, sub_id, org, MAINTENANCE_UPDATE, closing, "")
            .await
            .unwrap()
    );
    subscriber_deliveries::mark(&pool, sub_id, MAINTENANCE_UPDATE, closing, "", None, true)
        .await
        .unwrap();
    assert!(
        owed(
            subscriber_maintenance::list_pending_updates(&pool, 100)
                .await
                .unwrap()
        )
        .is_empty(),
        "sent once"
    );
    cleanup(&pool, org).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_page_that_kept_an_incident_hears_how_it_ended() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    let sub_id = confirmed_subscriber(&pool, org, page, "kept@example.com").await;
    let incident = seed_public_incident(&pool, org, target).await;
    sqlx::query(
        "INSERT INTO incident_status_pages (org_id, incident_id, status_page_id, component_name)
         VALUES ($1, $2, $3, 'API')",
    )
    .bind(org)
    .bind(incident)
    .bind(page)
    .execute(&pool)
    .await
    .unwrap();
    let meanwhile = add_update(&pool, org, incident, 0).await;
    let advisory: Uuid = sqlx::query_scalar(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message)
         VALUES ($1, $2, 'resolved', 'Looks fixed, still watching') RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .fetch_one(&pool)
    .await
    .unwrap();

    let owed = |pending: Vec<subscribers::PendingUpdate>| -> Vec<Uuid> {
        pending
            .into_iter()
            .filter(|p| p.subscriber_id == sub_id)
            .map(|p| p.update_id)
            .collect()
    };
    assert!(
        owed(subscribers::list_pending(&pool, 100).await.unwrap()).is_empty(),
        "the monitor left the page, so the open incident is not its news, not even {advisory}"
    );

    sqlx::query("UPDATE incidents SET ended_at = now(), resolved_at = now(), state = 'resolved' WHERE id = $1")
        .bind(incident)
        .execute(&pool)
        .await
        .unwrap();
    let resolved = add_resolved_update(&pool, org, incident).await;
    let postmortem: Uuid = sqlx::query_scalar(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, posted_at)
         VALUES ($1, $2, 'postmortem', 'What happened', now() + interval '1 second')
         RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        owed(subscribers::list_pending(&pool, 100).await.unwrap()),
        [resolved, postmortem],
        "how it ended reaches the page that kept it, {meanwhile} and {advisory} do not"
    );
    cleanup(&pool, org).await;
}

/// The worker can run after several updates have piled up. A page that kept
/// the incident must still get the closing update and what followed it, even
/// when a later update carries the resolved label too.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_later_resolved_update_does_not_hide_the_closing_one_from_a_kept_page() {
    use uptimepage::storage::{Actor, IncidentOpsStore, PgIncidentOpsStore};
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let page = seed_page(&pool, org).await;
    let sub_id = confirmed_subscriber(&pool, org, page, "late@example.com").await;
    let incident = seed_public_incident(&pool, org, target).await;
    sqlx::query(
        "INSERT INTO incident_status_pages (org_id, incident_id, status_page_id, component_name)
         VALUES ($1, $2, $3, 'API')",
    )
    .bind(org)
    .bind(incident)
    .bind(page)
    .execute(&pool)
    .await
    .unwrap();

    PgIncidentOpsStore::new(pool.clone())
        .resolve(
            uptimepage::domain::OrgId(org),
            incident,
            Actor::System,
            Some("Fixed.".into()),
        )
        .await
        .unwrap();
    let closing: Uuid = sqlx::query_scalar(
        "SELECT id FROM incident_updates WHERE incident_id = $1 AND phase = 'resolved'",
    )
    .bind(incident)
    .fetch_one(&pool)
    .await
    .unwrap();
    let later = |phase: &'static str, secs: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO incident_updates (org_id, incident_id, phase, message, posted_at)
                 VALUES ($1, $2, $3, 'more', now() + make_interval(secs => $4))
                 RETURNING id",
            )
            .bind(org)
            .bind(incident)
            .bind(phase)
            .bind(secs)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let postmortem = later("postmortem", 1).await;
    let restated = later("resolved", 2).await;

    let mine: Vec<Uuid> = subscribers::list_pending(&pool, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.subscriber_id == sub_id)
        .map(|p| p.update_id)
        .collect();
    assert_eq!(mine, [closing, postmortem, restated]);
    cleanup(&pool, org).await;
}
