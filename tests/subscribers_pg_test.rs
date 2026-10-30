//! Live-Postgres contract for public status-page subscribers: subscribe →
//! confirm → unsubscribe, the stateless unsubscribe HMAC, what the planner
//! decides each page's subscribers hear, outages told once at each end, and
//! the senders' leases.
//!
//! Live-PG ignored: needs `DATABASE_URL`. The planner reads every page in its
//! database, so each planner test runs in a database of its own.

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use uptimepage::domain::{NewSubscriber, SubscriberChannel};
use uptimepage::email::{
    BatchFailure, BatchResult, EmailError, EmailResult, EmailSender, EmailTemplate,
    InMemoryEmailSender, MessageId, PreparedBatch, SendClass, TransactionalEmail,
};
use uptimepage::public_status::subscriber_dispatch::{
    SubscriberDispatchConfig, SubscriberDispatcher,
};
use uptimepage::public_status::subscriber_notice::PageLinks;
use uptimepage::storage::subscriber_deliveries::{self, Outcome};
use uptimepage::storage::subscribers::{self, ConfirmMint};

use common::{drop_test_db, fresh_test_db, open_test_pool, pg_pool_from_env, unique_slug};

async fn isolated(tag: &str) -> Option<(PgPool, String)> {
    let (url, name) = fresh_test_db(tag).await?;
    Some((open_test_pool(&url).await, name))
}

async fn done(pool: PgPool, name: &str) {
    pool.close().await;
    drop_test_db(name).await;
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let slug = unique_slug("sub-org");
    sqlx::query_scalar(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'sub', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed_target(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        r#"INSERT INTO targets (org_id, name, check_spec, interval_secs)
           VALUES ($1, $2, '{"type":"http","url":"https://example.com/"}'::jsonb, 60)
           RETURNING id"#,
    )
    .bind(org)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed_page(pool: &PgPool, org: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO status_pages (org_id, slug, name, enabled) VALUES ($1, $2, 'Acme', true) RETURNING id",
    )
    .bind(org)
    .bind(unique_slug("subpage"))
    .fetch_one(pool)
    .await
    .unwrap()
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

async fn seed_incident(pool: &PgPool, org: Uuid, target: Uuid, visibility: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility, public_title)
         VALUES ($1, $2, now(), 'down', $3, 'Elevated errors')
         RETURNING id",
    )
    .bind(org)
    .bind(target)
    .bind(visibility)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A person's update, `age_hours` old.
async fn add_update(pool: &PgPool, org: Uuid, incident: Uuid, age_hours: i64) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, posted_at)
         VALUES ($1, $2, 'investigating', 'looking into it', now() - make_interval(hours => $3))
         RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .bind(age_hours as i32)
    .fetch_one(pool)
    .await
    .unwrap()
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
    sqlx::query_scalar(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message)
         VALUES ($1, $2, 'resolved', 'all clear')
         RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .fetch_one(pool)
    .await
    .unwrap()
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

fn config() -> SubscriberDispatchConfig {
    let mut cfg = SubscriberDispatchConfig::new(
        PageLinks {
            base_domain: "example.com".into(),
            public_base_url: "https://app.example.com".into(),
            subdomain_routes: true,
            enforce_plan_features: true,
            default_show_powered_by: true,
        },
        "dispatch-secret".into(),
        "status@example.com".into(),
        "Status".into(),
    );
    cfg.tick_interval = Duration::from_secs(60);
    cfg.resend_waits = vec![Duration::ZERO; 2];
    cfg
}

fn dispatcher(pool: PgPool, email: Arc<dyn EmailSender>) -> SubscriberDispatcher {
    let (http, _) = common::build_test_outbound_and_email();
    SubscriberDispatcher::new(pool, email, http, config())
}

/// What a subscriber has waiting: (notice kind, source, phase), oldest first.
async fn owed(pool: &PgPool, sub: Uuid) -> Vec<(String, Uuid, String)> {
    sqlx::query_as(
        "SELECT n.kind, n.source_ref, n.phase FROM status_page_subscriber_deliveries d
         JOIN status_page_notices n ON n.id = d.notice_id
         WHERE d.subscriber_id = $1 AND d.status = 'queued'
         ORDER BY n.created_at, n.id",
    )
    .bind(sub)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn kinds(owed: &[(String, Uuid, String)]) -> Vec<&str> {
    owed.iter().map(|(k, _, _)| k.as_str()).collect()
}

fn sources(owed: &[(String, Uuid, String)], kind: &str) -> Vec<Uuid> {
    owed.iter()
        .filter(|(k, _, _)| k == kind)
        .map(|(_, s, _)| *s)
        .collect()
}

fn mails_to(mem: &InMemoryEmailSender, to: &str) -> Vec<TransactionalEmail> {
    mem.sent()
        .into_iter()
        .filter(|e| e.to.address == to)
        .collect()
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
    let new = || NewSubscriber {
        status_page_id: page,
        org_id: org,
        channel: SubscriberChannel::Email,
        target: email.clone(),
        config: serde_json::json!({}),
    };

    let sub = subscribers::subscribe(&pool, &new()).await.unwrap();
    assert!(sub.verified_at.is_none(), "starts pending");
    let again = subscribers::subscribe(&pool, &new()).await.unwrap();
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
    assert!(subscribers::confirm(&pool, &token).await.unwrap().is_none());

    let secret = "test-salt";
    let mac = subscribers::unsubscribe_token(secret, sub.id);
    assert!(subscribers::verify_unsubscribe(secret, sub.id, &mac));
    assert!(!subscribers::verify_unsubscribe(secret, sub.id, "nope"));

    assert!(subscribers::unsubscribe(&pool, sub.id).await.unwrap());
    assert!(!subscribers::unsubscribe(&pool, sub.id).await.unwrap());

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_update_reaches_verified_subscribers_inside_the_lookback_once() {
    let Some((pool, name)) = isolated("subfresh").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "v@example.com").await;
    let pending = subscribers::subscribe(
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

    let incident = seed_incident(&pool, org, target, "public").await;
    let fresh = add_update(&pool, org, incident, 0).await;
    let _stale = add_update(&pool, org, incident, 48).await;

    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.plan_once().await.unwrap();
    let mine = owed(&pool, sub).await;
    assert_eq!(
        sources(&mine, "incident_update"),
        [fresh],
        "only the fresh update"
    );
    assert!(
        owed(&pool, pending.id).await.is_empty(),
        "never the unverified"
    );

    disp.run_once().await.unwrap();
    let mails = mails_to(&mem, "v@example.com");
    assert_eq!(mails.len(), 1);
    let rendered = mails[0].template.render("Uptimepage");
    assert!(
        rendered.subject.contains("Elevated errors"),
        "{}",
        rendered.subject
    );
    assert!(
        rendered.subject.starts_with("[sub]"),
        "the org's name, as on the page"
    );
    assert!(mails[0].template.list_unsubscribe_url().is_some());

    disp.run_once().await.unwrap();
    assert_eq!(mails_to(&mem, "v@example.com").len(), 1, "no duplicate");
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_mail_links_the_custom_domain_only_once_it_is_activated_on_a_plan_that_sells_one() {
    let Some((pool, name)) = isolated("subcd").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    confirmed_subscriber(&pool, org, page, "cd@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 0).await;
    let domain = format!("{}.example.net", unique_slug("st"));
    sqlx::query(
        "UPDATE status_pages SET custom_domain = $2, custom_domain_verified_at = now() WHERE id = $1",
    )
    .bind(page)
    .bind(&domain)
    .execute(&pool)
    .await
    .unwrap();

    let disp = dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()));
    let origin = async || -> String {
        sqlx::query("DELETE FROM status_page_notices")
            .execute(&pool)
            .await
            .unwrap();
        disp.plan_once().await.unwrap();
        sqlx::query_scalar(
            "SELECT payload -> 'page' ->> 'origin' FROM status_page_notices
             WHERE kind = 'incident_update'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(
        !origin().await.contains(&domain),
        "a free plan links the subdomain"
    );

    sqlx::query(
        "UPDATE accounts SET plan_id = 'team' \
         WHERE id = (SELECT account_id FROM organizations WHERE id = $1)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !origin().await.contains(&domain),
        "verification permits a certificate, it does not move a mailed link"
    );

    sqlx::query("UPDATE status_pages SET custom_domain_activated_at = now() WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(origin().await, format!("https://{domain}"));
    done(pool, &name).await;
}

/// An incident with no monitor has no component to route by, so its updates
/// reach a page's subscribers only when it was posted to that page.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_page_incident_reaches_only_the_subscribers_of_its_pages() {
    let Some((pool, name)) = isolated("subposted").await else {
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

    let mem = Arc::new(InMemoryEmailSender::new());
    dispatcher(pool.clone(), mem.clone())
        .plan_once()
        .await
        .unwrap();
    assert_eq!(
        sources(&owed(&pool, told).await, "incident_update"),
        [update]
    );
    assert!(owed(&pool, untold).await.is_empty());
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_unpublished_incident_tells_only_its_resolved_closer() {
    let Some((pool, name)) = isolated("subunpub").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "u@example.com").await;

    let incident = seed_incident(&pool, org, target, "internal").await;
    mark_published(&pool, org, incident).await;
    let investigating = add_update(&pool, org, incident, 0).await;
    let resolved = add_resolved_update(&pool, org, incident).await;

    let target2 = seed_target(&pool, org, "mon2").await;
    add_component(&pool, org, page, target2).await;
    let silent = seed_incident(&pool, org, target2, "internal").await;
    let silent_resolved = add_resolved_update(&pool, org, silent).await;

    dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()))
        .plan_once()
        .await
        .unwrap();
    let told = sources(&owed(&pool, sub).await, "incident_update");
    assert!(told.contains(&resolved), "the closer reaches subscribers");
    assert!(!told.contains(&investigating), "the rest stays suppressed");
    assert!(
        !told.contains(&silent_resolved),
        "never published, never told"
    );
    done(pool, &name).await;
}

async fn seed_maintenance(pool: &PgPool, org: Uuid, start_h: i64, end_h: i64) -> Uuid {
    sqlx::query_scalar(
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
    .unwrap()
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
async fn maintenance_tells_scheduled_and_completed_inside_the_lookback_and_never_a_cancelled_window()
 {
    let Some((pool, name)) = isolated("submaint").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "m@example.com").await;

    let future = seed_maintenance(&pool, org, 24, 25).await;
    add_maintenance_component(&pool, org, future, target).await;
    let past = seed_maintenance(&pool, org, -2, -1).await;
    add_maintenance_component(&pool, org, past, target).await;
    let old = seed_maintenance(&pool, org, -26, -25).await;
    add_maintenance_component(&pool, org, old, target).await;
    let cancelled = seed_maintenance(&pool, org, 24, 25).await;
    add_maintenance_component(&pool, org, cancelled, target).await;
    sqlx::query("UPDATE maintenance_windows SET deleted_at = now() WHERE id = $1")
        .bind(cancelled)
        .execute(&pool)
        .await
        .unwrap();

    dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()))
        .plan_once()
        .await
        .unwrap();
    let mut mine: Vec<(Uuid, String)> = owed(&pool, sub)
        .await
        .into_iter()
        .filter(|(k, _, _)| k == "maintenance")
        .map(|(_, s, p)| (s, p))
        .collect();
    mine.sort();
    let mut want = vec![
        (future, "scheduled".to_string()),
        (past, "completed".to_string()),
    ];
    want.sort();
    assert_eq!(
        mine, want,
        "the old window is past the lookback, the cancelled one silent"
    );
    done(pool, &name).await;
}

/// A page that 404s (Off, held, or its org deleted) mails its subscribers
/// nothing until it is served again, and what was already planned for it is
/// skipped rather than sent.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_page_that_is_not_served_mails_its_subscribers_nothing() {
    let Some((pool, name)) = isolated("subquiet").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "quiet@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 0).await;
    let window = seed_maintenance(&pool, org, 24, 25).await;
    add_maintenance_component(&pool, org, window, target).await;

    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    let restore = async || {
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
    };
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
        disp.plan_once().await.unwrap();
        assert!(owed(&pool, sub).await.is_empty(), "{state}: plans nothing");
        restore().await;
    }

    disp.plan_once().await.unwrap();
    assert_eq!(
        kinds(&owed(&pool, sub).await).len(),
        2,
        "served: update and window"
    );
    sqlx::query("UPDATE status_pages SET enabled = false WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();
    disp.run_once().await.unwrap();
    assert!(mem.is_empty(), "turned off before sending: skipped");
    let skipped: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM status_page_subscriber_deliveries WHERE status = 'skipped'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(skipped, 2);
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_german_page_mails_its_subscribers_in_german() {
    let Some((pool, name)) = isolated("subde").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    sqlx::query("UPDATE status_pages SET public_locale = 'de' WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "de@example.com").await;
    assert_eq!(
        subscribers::page_locale(&pool, sub).await.unwrap(),
        Some(uptimepage::domain::Locale::De),
        "unsubscribe pages follow the subscriber's page"
    );
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 0).await;

    let mem = Arc::new(InMemoryEmailSender::new());
    dispatcher(pool.clone(), mem.clone())
        .run_once()
        .await
        .unwrap();
    let mail = mails_to(&mem, "de@example.com").pop().expect("mailed");
    let rendered = mail.template.render("Uptimepage");
    assert!(
        rendered.subject.ends_with("Untersuchung läuft"),
        "{}",
        rendered.subject
    );
    assert!(rendered.html_body.contains(r#"<html lang="de">"#));
    done(pool, &name).await;
}

/// A page whose plan cannot drop the footer badge carries the credit in its
/// mail too; a white-label page that turned the badge off carries neither.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_mail_carries_the_credit_only_where_the_plan_keeps_the_badge() {
    let Some((pool, name)) = isolated("subcr").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    sqlx::query("UPDATE status_pages SET public_show_powered_by = false WHERE id = $1")
        .bind(page)
        .execute(&pool)
        .await
        .unwrap();
    add_component(&pool, org, page, target).await;
    confirmed_subscriber(&pool, org, page, "cr@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;

    let mailed = async || -> String {
        add_update(&pool, org, incident, 0).await;
        let mem = Arc::new(InMemoryEmailSender::new());
        dispatcher(pool.clone(), mem.clone())
            .run_once()
            .await
            .unwrap();
        let mail = mails_to(&mem, "cr@example.com").pop().expect("mailed");
        mail.template.render("Uptimepage").text_body
    };
    let free = mailed().await;
    assert!(
        free.contains("Status page by uptimepage — free for your own project"),
        "{free}"
    );
    assert!(free.contains("utm_source=status-email"), "{free}");

    sqlx::query(
        "UPDATE accounts SET plan_id = 'team' \
         WHERE id = (SELECT account_id FROM organizations WHERE id = $1)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let team = mailed().await;
    assert!(!team.contains("Status page by uptimepage"), "{team}");
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_webhook_subscriber_gets_every_update_including_the_platforms_own() {
    let Some((pool, name)) = isolated("subhook").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let hook = subscribers::subscribe(
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
    subscribers::mark_verified(&pool, hook.id).await.unwrap();
    let mail = confirmed_subscriber(&pool, org, page, "mail@example.com").await;

    let incident = seed_incident(&pool, org, target, "public").await;
    let generated: Uuid = sqlx::query_scalar(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated)
         VALUES ($1, $2, 'investigating', 'Automatically detected', 'system', true) RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .fetch_one(&pool)
    .await
    .unwrap();
    let written = add_update(&pool, org, incident, 0).await;

    dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()))
        .plan_once()
        .await
        .unwrap();
    let mut hooked = sources(&owed(&pool, hook.id).await, "incident_update");
    hooked.sort();
    let mut both = vec![generated, written];
    both.sort();
    assert_eq!(hooked, both, "a program hears each update");
    assert_eq!(
        sources(&owed(&pool, mail).await, "incident_update"),
        [written],
        "an inbox hears what a person wrote; the outage mail covers the rest"
    );
    done(pool, &name).await;
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
    let Some((pool, name)) = isolated("subleft").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    let sub = confirmed_subscriber(&pool, org, page, "left@example.com").await;
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

    dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()))
        .plan_once()
        .await
        .unwrap();
    assert_eq!(
        sources(&owed(&pool, sub).await, "maintenance_update"),
        [late],
        "the page shows the window, so it hears the update"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_maintenance_update_reaches_each_subscriber_once_and_keeps_reaching_a_page_that_kept_it()
{
    let Some((pool, name)) = isolated("subkeptmw").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "upd@example.com").await;
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    let updates = async || {
        disp.plan_once().await.unwrap();
        sources(&owed(&pool, sub).await, "maintenance_update")
    };

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
    assert_eq!(
        updates().await,
        [update],
        "a cancelled window stays silent: {silent}"
    );
    disp.run_once().await.unwrap();

    // The monitor leaves the page: a page that never kept the window hears
    // nothing more of it.
    sqlx::query("DELETE FROM status_page_components WHERE status_page_id = $1 AND target_id = $2")
        .bind(page)
        .bind(target)
        .execute(&pool)
        .await
        .unwrap();
    let unheard = post_maintenance_update(&pool, org, window, "Still going.").await;
    assert!(
        updates().await.is_empty(),
        "{unheard} is not this page's news"
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
    let running = post_maintenance_update(&pool, org, window, "Nearly there.").await;
    assert!(
        updates().await.is_empty(),
        "a kept window is history: {running} while it runs"
    );

    sqlx::query("UPDATE maintenance_windows SET ends_at = clock_timestamp() WHERE id = $1")
        .bind(window)
        .execute(&pool)
        .await
        .unwrap();
    let closing = post_maintenance_update(&pool, org, window, "Finished.").await;
    assert_eq!(updates().await, [closing], "what was said once it ended");

    disp.run_once().await.unwrap();
    assert!(updates().await.is_empty(), "sent once");
    assert_eq!(
        mails_to(&mem, "upd@example.com").len(),
        4,
        "scheduled, update, completed, closing"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_page_that_kept_an_incident_hears_how_it_ended() {
    let Some((pool, name)) = isolated("subkept").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    let sub = confirmed_subscriber(&pool, org, page, "kept@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;
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

    let disp = dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()));
    disp.plan_once().await.unwrap();
    assert!(
        owed(&pool, sub).await.is_empty(),
        "the monitor left the page, so the open incident is not its news: {meanwhile}, {advisory}"
    );

    sqlx::query(
        "UPDATE incidents SET ended_at = now(), resolved_at = now(), state = 'resolved' WHERE id = $1",
    )
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
    disp.plan_once().await.unwrap();
    let mut told = sources(&owed(&pool, sub).await, "incident_update");
    told.sort();
    let mut want = vec![resolved, postmortem];
    want.sort();
    assert_eq!(told, want, "how it ended reaches the page that kept it");
    done(pool, &name).await;
}

/// The planner can run after several updates have piled up. A page that kept
/// the incident must still get the closing update and what followed it, even
/// when a later update carries the resolved label too.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_later_resolved_update_does_not_hide_the_closing_one_from_a_kept_page() {
    use uptimepage::storage::{Actor, IncidentOpsStore, PgIncidentOpsStore};
    let Some((pool, name)) = isolated("subrestate").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    let sub = confirmed_subscriber(&pool, org, page, "late@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;
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
    let later = async |phase: &str, secs: i32| -> Uuid {
        sqlx::query_scalar(
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
    };
    let postmortem = later("postmortem", 1).await;
    let restated = later("resolved", 2).await;

    dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()))
        .plan_once()
        .await
        .unwrap();
    let mut told = sources(&owed(&pool, sub).await, "incident_update");
    told.sort();
    let mut want = vec![closing, postmortem, restated];
    want.sort();
    assert_eq!(told, want);
    done(pool, &name).await;
}

/// Five monitors on a page, as the incident writer opens and closes them:
/// generated openers and closers, written a little in the past so the gather
/// and settle times have run.
struct Outage {
    org: Uuid,
    page: Uuid,
    incidents: Vec<Uuid>,
}

async fn five_down(pool: &PgPool, opened_mins_ago: i32) -> Outage {
    let org = seed_org(pool).await;
    let page = seed_page(pool, org).await;
    let mut incidents = Vec::new();
    for name in ["Website", "API", "Dashboard", "Checkout", "Webhooks"] {
        let target = seed_target(pool, org, name).await;
        add_component(pool, org, page, target).await;
        let incident: Uuid = sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, visibility)
             VALUES ($1, $2, now() - make_interval(mins => $3 + 1), 'down', 'public')
             RETURNING id",
        )
        .bind(org)
        .bind(target)
        .bind(opened_mins_ago)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated,
                                           posted_at)
             VALUES ($1, $2, 'investigating', 'Automatically detected', 'system', true,
                     now() - make_interval(mins => $3))",
        )
        .bind(org)
        .bind(incident)
        .bind(opened_mins_ago)
        .execute(pool)
        .await
        .unwrap();
        incidents.push(incident);
    }
    Outage {
        org,
        page,
        incidents,
    }
}

async fn close_all(pool: &PgPool, outage: &Outage, ended_mins_ago: i32, posted_mins_ago: i32) {
    for incident in &outage.incidents {
        sqlx::query(
            "UPDATE incidents SET ended_at = now() - make_interval(mins => $3),
                                  resolved_at = now() - make_interval(mins => $4),
                                  state = 'resolved'
             WHERE id = $1 AND org_id = $2",
        )
        .bind(incident)
        .bind(outage.org)
        .bind(ended_mins_ago)
        .bind(posted_mins_ago)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated,
                                           posted_at)
             VALUES ($1, $2, 'resolved', 'Automatically resolved', 'system', true,
                     now() - make_interval(mins => $3))",
        )
        .bind(outage.org)
        .bind(incident)
        .bind(posted_mins_ago)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn outage_mail(mail: &TransactionalEmail) -> (bool, usize) {
    match &mail.template {
        EmailTemplate::SubscriberOutage(o) => (o.resolved, o.components.len()),
        other => panic!("expected an outage mail, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn five_monitors_down_together_send_one_mail_when_it_starts_and_one_when_it_ends() {
    let Some((pool, name)) = isolated("subfive").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    confirmed_subscriber(&pool, outage.org, outage.page, "five@example.com").await;
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());

    disp.run_once().await.unwrap();
    let mails = mails_to(&mem, "five@example.com");
    assert_eq!(mails.len(), 1, "one started mail, no mail per monitor");
    assert_eq!(outage_mail(&mails[0]), (false, 5));

    disp.run_once().await.unwrap();
    assert_eq!(
        mails_to(&mem, "five@example.com").len(),
        1,
        "planning twice adds nothing"
    );
    let notices: i64 =
        sqlx::query_scalar("SELECT count(*) FROM status_page_notices WHERE kind LIKE 'outage%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(notices, 1);

    close_all(&pool, &outage, 7, 6).await;
    disp.run_once().await.unwrap();
    let mails = mails_to(&mem, "five@example.com");
    assert_eq!(mails.len(), 2, "one resolved mail");
    assert_eq!(outage_mail(&mails[1]), (true, 5));
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_outage_that_opened_and_closed_between_two_passes_sends_one_summary() {
    let Some((pool, name)) = isolated("subblip").await else {
        return;
    };
    let outage = five_down(&pool, 20).await;
    close_all(&pool, &outage, 18, 15).await;
    confirmed_subscriber(&pool, outage.org, outage.page, "blip@example.com").await;
    let mem = Arc::new(InMemoryEmailSender::new());
    dispatcher(pool.clone(), mem.clone())
        .run_once()
        .await
        .unwrap();
    let mails = mails_to(&mem, "blip@example.com");
    assert_eq!(mails.len(), 1, "no started mail for what was already over");
    assert_eq!(outage_mail(&mails[0]), (true, 5));
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_started_notice_held_back_until_its_outage_ended_is_never_sent() {
    use uptimepage::public_status::subscriber_planner;
    let Some((pool, name)) = isolated("subheld").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    let sub = confirmed_subscriber(&pool, outage.org, outage.page, "held@example.com").await;
    let links = config().links;
    subscriber_planner::plan(&pool, &links).await.unwrap();
    close_all(&pool, &outage, 7, 6).await;
    subscriber_planner::plan(&pool, &links).await.unwrap();

    let mem = Arc::new(InMemoryEmailSender::new());
    dispatcher(pool.clone(), mem.clone())
        .run_once()
        .await
        .unwrap();
    let mails = mails_to(&mem, "held@example.com");
    assert_eq!(mails.len(), 1);
    assert_eq!(outage_mail(&mails[0]), (true, 5), "only the resolved mail");
    assert!(owed(&pool, sub).await.is_empty());
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_started_delivery_still_queued_when_the_outage_ends_is_skipped() {
    use uptimepage::public_status::subscriber_planner;
    let Some((pool, name)) = isolated("subskip").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    confirmed_subscriber(&pool, outage.org, outage.page, "skip@example.com").await;
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.plan_once().await.unwrap();
    close_all(&pool, &outage, 7, 6).await;
    subscriber_planner::plan(&pool, &config().links)
        .await
        .unwrap();
    let skipped: Vec<String> = sqlx::query_scalar(
        "SELECT d.status FROM status_page_subscriber_deliveries d
         JOIN status_page_notices n ON n.id = d.notice_id WHERE n.kind = 'outage_started'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(skipped, ["queued"], "planning leaves it; the claim decides");
    disp.run_once().await.unwrap();
    let skipped: Vec<String> = sqlx::query_scalar(
        "SELECT d.status FROM status_page_subscriber_deliveries d
         JOIN status_page_notices n ON n.id = d.notice_id WHERE n.kind = 'outage_started'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(skipped, ["skipped"]);
    let mails = mails_to(&mem, "skip@example.com");
    assert_eq!(mails.len(), 1);
    assert_eq!(outage_mail(&mails[0]), (true, 5));
    done(pool, &name).await;
}

async fn subscribers_for(pool: &PgPool, org: Uuid, page: Uuid, n: usize) -> Vec<String> {
    let mut emails = Vec::new();
    for i in 0..n {
        let email = format!("s{i}@example.com");
        confirmed_subscriber(pool, org, page, &email).await;
        emails.push(email);
    }
    emails
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn two_instances_fan_out_one_notice_once() {
    let Some((pool, name)) = isolated("subfan").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    subscribers_for(&pool, org, page, 30).await;
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 0).await;

    let mem: Arc<dyn EmailSender> = Arc::new(InMemoryEmailSender::new());
    let (blue, green) = (
        dispatcher(pool.clone(), mem.clone()),
        dispatcher(pool.clone(), mem),
    );
    let (a, b) = tokio::join!(blue.plan_once(), green.plan_once());
    a.unwrap();
    b.unwrap();
    let deliveries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM status_page_subscriber_deliveries")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deliveries, 30);
    done(pool, &name).await;
}

/// Sends slowly, so a unit outlives a short lease unless it is renewed.
struct Slow {
    inner: InMemoryEmailSender,
    wait: Duration,
    batches: AtomicUsize,
}

#[async_trait]
impl EmailSender for Slow {
    async fn send(&self, email: TransactionalEmail, class: SendClass) -> EmailResult<MessageId> {
        self.inner.send(email, class).await
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        self.inner.prepare_batch(emails, key)
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        self.batches.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.wait).await;
        self.inner.send_prepared(batch).await
    }
}

fn slow_dispatcher(pool: PgPool, email: Arc<dyn EmailSender>) -> SubscriberDispatcher {
    let (http, _) = common::build_test_outbound_and_email();
    let mut cfg = config();
    cfg.lease = Duration::from_millis(800);
    cfg.renew_every = Duration::from_millis(200);
    SubscriberDispatcher::new(pool, email, http, cfg)
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn two_instances_with_a_sender_slower_than_the_lease_send_each_delivery_once() {
    let Some((pool, name)) = isolated("sublease").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let page = seed_page(&pool, org).await;
    let emails = subscribers_for(&pool, org, page, 3).await;
    for i in 0..4 {
        let target = seed_target(&pool, org, &format!("mon{i}")).await;
        add_component(&pool, org, page, target).await;
        let incident = seed_incident(&pool, org, target, "public").await;
        add_update(&pool, org, incident, 0).await;
    }
    let slow = Arc::new(Slow {
        inner: InMemoryEmailSender::new(),
        wait: Duration::from_millis(2000),
        batches: AtomicUsize::new(0),
    });
    let (blue, green) = (
        Arc::new(slow_dispatcher(pool.clone(), slow.clone())),
        Arc::new(slow_dispatcher(pool.clone(), slow.clone())),
    );
    blue.plan_once().await.unwrap();
    let drain = |d: Arc<SubscriberDispatcher>, pool: PgPool| async move {
        loop {
            subscriber_deliveries::release_expired(&pool).await.unwrap();
            if !d.send_unit("email").await.unwrap() {
                break;
            }
        }
    };
    tokio::join!(drain(blue, pool.clone()), drain(green, pool.clone()));
    for email in &emails {
        assert_eq!(
            mails_to(&slow.inner, email).len(),
            4,
            "{email}: each update once"
        );
    }
    assert_eq!(
        slow.batches.load(Ordering::SeqCst),
        4,
        "one request per notice"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_stopped_workers_rows_go_to_another_and_its_late_write_changes_nothing() {
    let Some((pool, name)) = isolated("substopped").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "stopped@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 0).await;
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.plan_once().await.unwrap();

    // A worker claims the unit and dies before it sends or renews.
    let gone = Uuid::now_v7();
    let claimed =
        subscriber_deliveries::claim(&pool, "email", 100, gone, Duration::from_millis(300))
            .await
            .unwrap()
            .expect("claimed")
            .deliveries;
    tokio::time::sleep(Duration::from_millis(500)).await;
    disp.run_once().await.unwrap();
    assert_eq!(mails_to(&mem, "stopped@example.com").len(), 1);

    let late = subscriber_deliveries::record(
        &pool,
        gone,
        &[Outcome {
            id: claimed[0].id,
            status: "failed",
            error: Some("late".into()),
            attempts: 9,
            retry_in: Duration::ZERO,
            batch_key: None,
        }],
        None,
    )
    .await
    .unwrap();
    assert!(late.is_empty());
    let status: String = sqlx::query_scalar(
        "SELECT status FROM status_page_subscriber_deliveries WHERE subscriber_id = $1",
    )
    .bind(sub)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "sent");
    done(pool, &name).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn concurrent_claims_never_split_one_notice() {
    let Some((pool, name)) = isolated("subsplit").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    subscribers_for(&pool, org, page, 20).await;
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 0).await;
    dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()))
        .plan_once()
        .await
        .unwrap();
    for _ in 0..50 {
        sqlx::query(
            "UPDATE status_page_subscriber_deliveries
             SET status = 'queued', claim_token = NULL, lease_until = NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        let claims: Vec<_> = (0..4)
            .map(|_| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    subscriber_deliveries::claim(
                        &pool,
                        "email",
                        100,
                        Uuid::now_v7(),
                        Duration::from_secs(60),
                    )
                    .await
                    .unwrap()
                })
            })
            .collect();
        let mut sizes = Vec::new();
        for claim in claims {
            if let Some(unit) = claim.await.unwrap() {
                sizes.push(unit.deliveries.len());
            }
        }
        assert_eq!(sizes, [20], "one sender takes the whole notice");
    }
    done(pool, &name).await;
}

/// Holds every batch back as pacing would.
struct Paced(InMemoryEmailSender);

#[async_trait]
impl EmailSender for Paced {
    async fn send(&self, email: TransactionalEmail, class: SendClass) -> EmailResult<MessageId> {
        self.0.send(email, class).await
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        self.0.prepare_batch(emails, key)
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        Err(BatchFailure {
            error: EmailError::GateUnavailable,
            batch,
        })
    }
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_send_held_back_by_pacing_uses_no_attempt() {
    let Some((pool, name)) = isolated("subpaced").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let sub = confirmed_subscriber(&pool, org, page, "paced@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 0).await;
    dispatcher(pool.clone(), Arc::new(Paced(InMemoryEmailSender::new())))
        .run_once()
        .await
        .unwrap();
    let (status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempts FROM status_page_subscriber_deliveries WHERE subscriber_id = $1",
    )
    .bind(sub)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), attempts), ("queued", 0));
    done(pool, &name).await;
}

/// One delivery as it stands: (status, attempts, batch key).
async fn delivery_state(pool: &PgPool, id: Uuid) -> Option<(String, i32, Option<String>)> {
    sqlx::query_as(
        "SELECT status, attempts, batch_key FROM status_page_subscriber_deliveries WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn make_due(pool: &PgPool) {
    sqlx::query(
        "UPDATE status_page_subscriber_deliveries SET next_attempt_at = now()
         WHERE status = 'queued'",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn stored_batches(pool: &PgPool) -> Vec<(String, Vec<u8>, Vec<Uuid>)> {
    sqlx::query_as("SELECT key, request, delivery_ids FROM status_page_delivery_batches")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Answers batch requests with `errors` in order, then sends. Remembers
/// every request, bounces the mail to `bounce` by its position, and answers
/// with at most `answers` results. With `steal` set, another sender takes
/// the unit while an error is being answered.
struct Flaky {
    inner: InMemoryEmailSender,
    errors: std::sync::Mutex<std::collections::VecDeque<EmailError>>,
    requests: std::sync::Mutex<Vec<(String, Vec<u8>)>>,
    order: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
    bounce: std::sync::Mutex<Option<String>>,
    answers: std::sync::Mutex<Option<usize>>,
    steal: std::sync::Mutex<Option<PgPool>>,
    stolen_by: std::sync::Mutex<Option<Uuid>>,
}

impl Flaky {
    fn new(errors: impl IntoIterator<Item = EmailError>) -> Self {
        Self {
            inner: InMemoryEmailSender::new(),
            errors: std::sync::Mutex::new(errors.into_iter().collect()),
            requests: Default::default(),
            order: Default::default(),
            bounce: Default::default(),
            answers: Default::default(),
            steal: Default::default(),
            stolen_by: Default::default(),
        }
    }

    fn requests(&self) -> Vec<(String, Vec<u8>)> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl EmailSender for Flaky {
    async fn send(&self, email: TransactionalEmail, class: SendClass) -> EmailResult<MessageId> {
        self.inner.send(email, class).await
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        let to = emails.iter().map(|e| e.to.address.clone()).collect();
        self.order.lock().unwrap().insert(key.clone(), to);
        self.inner.prepare_batch(emails, key)
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        self.requests
            .lock()
            .unwrap()
            .push((batch.key().to_owned(), batch.body().to_vec()));
        let error = self.errors.lock().unwrap().pop_front();
        if let Some(error) = error {
            let thief = self.steal.lock().unwrap().clone();
            if let Some(pool) = thief {
                *self.stolen_by.lock().unwrap() = Some(steal_held(&pool).await);
            }
            return Err(BatchFailure { error, batch });
        }
        let order = self.order.lock().unwrap().get(batch.key()).cloned();
        let bounce = self.bounce.lock().unwrap().clone();
        let answers = self.answers.lock().unwrap().unwrap_or(usize::MAX);
        let results = self.inner.send_prepared(batch).await?;
        Ok(results
            .into_iter()
            .zip(order.unwrap_or_default())
            .map(|(result, to)| {
                if bounce.as_deref() == Some(to.as_str()) {
                    Err(EmailError::Rejected("bounced".into()))
                } else {
                    result
                }
            })
            .take(answers)
            .collect())
    }
}

async fn one_update_to(pool: &PgPool, subscribers: usize) -> (Uuid, Uuid) {
    let org = seed_org(pool).await;
    let target = seed_target(pool, org, "mon").await;
    let page = seed_page(pool, org).await;
    add_component(pool, org, page, target).await;
    subscribers_for(pool, org, page, subscribers).await;
    let incident = seed_incident(pool, org, target, "public").await;
    add_update(pool, org, incident, 0).await;
    (org, page)
}

/// A batch the provider may have taken is sent again in process, then
/// stored and sent again as it was, its results read by stored position.
async fn a_batch_with_no_clear_answer_goes_out_again_as_it_was(
    tag: &str,
    error: fn() -> EmailError,
    attempts: i32,
) {
    let Some((pool, name)) = isolated(tag).await else {
        return;
    };
    one_update_to(&pool, 4).await;
    let flaky = Arc::new(Flaky::new((0..3).map(|_| error())));
    let disp = dispatcher(pool.clone(), flaky.clone());
    disp.run_once().await.unwrap();

    let first = flaky.requests();
    assert_eq!(first.len(), 3, "sent, then twice more in process");
    assert!(first.iter().all(|r| *r == first[0]), "same key and bytes");
    let [(key, request, ids)] = stored_batches(&pool).await.try_into().unwrap();
    assert_eq!((&key, &request), (&first[0].0, &first[0].1));
    assert_eq!(ids.len(), 4);
    for id in &ids {
        assert_eq!(
            delivery_state(&pool, *id).await,
            Some(("queued".into(), attempts, Some(key.clone())))
        );
    }

    // The subscriber at position 0 leaves; the mail at position 2 bounces.
    let at = |i: usize| {
        let pool = pool.clone();
        let id = ids[i];
        async move {
            sqlx::query_as::<_, (Uuid, String)>(
                "SELECT s.id, s.target FROM status_page_subscriber_deliveries d
                 JOIN status_page_subscribers s ON s.id = d.subscriber_id WHERE d.id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (gone, _) = at(0).await;
    let (_, bounced) = at(2).await;
    subscribers::unsubscribe(&pool, gone).await.unwrap();
    *flaky.bounce.lock().unwrap() = Some(bounced);
    make_due(&pool).await;
    disp.run_once().await.unwrap();

    let all = flaky.requests();
    assert_eq!(all.len(), 4);
    assert_eq!(all[3], first[0], "the stored request, not a new render");
    assert_eq!(delivery_state(&pool, ids[0]).await, None);
    let status = |i: usize| {
        let pool = pool.clone();
        let id = ids[i];
        async move { delivery_state(&pool, id).await.unwrap().0 }
    };
    assert_eq!(status(1).await, "sent");
    assert_eq!(status(2).await, "failed", "its own bounce, by position");
    assert_eq!(status(3).await, "sent");
    assert!(stored_batches(&pool).await.is_empty(), "settled");
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_timed_out_batch_goes_out_again_as_it_was() {
    a_batch_with_no_clear_answer_goes_out_again_as_it_was(
        "subtimeout",
        || EmailError::Transport("no response within 30s".into()),
        1,
    )
    .await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_batch_still_in_progress_goes_out_again_as_it_was_without_an_attempt() {
    a_batch_with_no_clear_answer_goes_out_again_as_it_was(
        "subinprogress",
        || EmailError::InProgress("concurrent_idempotent_requests".into()),
        0,
    )
    .await;
}

/// Another sender takes every held row, as after a lease ran out.
async fn steal_held(pool: &PgPool) -> Uuid {
    let thief = Uuid::now_v7();
    sqlx::query(
        "UPDATE status_page_subscriber_deliveries SET claim_token = $1 WHERE status = 'sending'",
    )
    .bind(thief)
    .execute(pool)
    .await
    .unwrap();
    thief
}

async fn held_by(pool: &PgPool) -> Vec<(String, Option<Uuid>)> {
    sqlx::query_as("SELECT status, claim_token FROM status_page_subscriber_deliveries")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_sender_that_lost_its_unit_sends_it_no_more() {
    let Some((pool, name)) = isolated("sublostresend").await else {
        return;
    };
    one_update_to(&pool, 2).await;
    let flaky = Arc::new(Flaky::new(
        (0..3).map(|_| EmailError::Transport("no response within 30s".into())),
    ));
    *flaky.steal.lock().unwrap() = Some(pool.clone());
    let disp = dispatcher(pool.clone(), flaky.clone());
    disp.plan_once().await.unwrap();
    assert!(disp.send_unit("email").await.unwrap());
    assert_eq!(flaky.requests().len(), 1, "no resend once the unit is gone");
    let thief = *flaky.stolen_by.lock().unwrap();
    assert!(thief.is_some());
    for (status, token) in held_by(&pool).await {
        assert_eq!(
            (status.as_str(), token),
            ("sending", thief),
            "left to its new sender"
        );
    }
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_batch_that_timed_out_before_a_rate_limit_is_still_kept() {
    let Some((pool, name)) = isolated("subtimedrate").await else {
        return;
    };
    one_update_to(&pool, 2).await;
    let rate_limited = || EmailError::RateLimited {
        retry_after: Duration::from_secs(5),
        detail: "rate_limit_exceeded".into(),
    };
    let flaky = Arc::new(Flaky::new([
        EmailError::Transport("no response within 30s".into()),
        rate_limited(),
    ]));
    dispatcher(pool.clone(), flaky.clone())
        .run_once()
        .await
        .unwrap();
    assert_eq!(
        flaky.requests().len(),
        2,
        "a rate limit is not resent in process"
    );
    let [(key, _, ids)] = stored_batches(&pool).await.try_into().unwrap();
    for id in &ids {
        assert_eq!(
            delivery_state(&pool, *id).await,
            Some(("queued".into(), 0, Some(key.clone()))),
            "the first try may have gone through"
        );
    }
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn mails_the_answer_leaves_out_are_built_anew() {
    let Some((pool, name)) = isolated("subshort").await else {
        return;
    };
    one_update_to(&pool, 2).await;
    let flaky = Arc::new(Flaky::new([]));
    *flaky.answers.lock().unwrap() = Some(0);
    let disp = dispatcher(pool.clone(), flaky.clone());
    disp.run_once().await.unwrap();
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM status_page_subscriber_deliveries ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    for id in &ids {
        assert_eq!(
            delivery_state(&pool, *id).await,
            Some(("queued".into(), 1, None))
        );
    }
    assert!(
        stored_batches(&pool).await.is_empty(),
        "the batch was answered"
    );
    *flaky.answers.lock().unwrap() = None;
    make_due(&pool).await;
    disp.run_once().await.unwrap();
    let requests = flaky.requests();
    assert_eq!(requests.len(), 2);
    assert_ne!(
        requests[1].0, requests[0].0,
        "the same deliveries, but not a replay of the cached answer"
    );
    for id in &ids {
        assert_eq!(delivery_state(&pool, *id).await.unwrap().0, "sent");
    }
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_stored_batch_refused_as_a_whole_is_let_go_and_built_again() {
    let Some((pool, name)) = isolated("substoredrefused").await else {
        return;
    };
    one_update_to(&pool, 2).await;
    let timeout = || EmailError::Transport("no response within 30s".into());
    let flaky = Arc::new(Flaky::new([
        timeout(),
        timeout(),
        timeout(),
        EmailError::Rejected("422 validation_error".into()),
    ]));
    let disp = dispatcher(pool.clone(), flaky.clone());
    disp.run_once().await.unwrap();
    let [(_, _, ids)] = stored_batches(&pool).await.try_into().unwrap();
    make_due(&pool).await;
    disp.run_once().await.unwrap();
    assert!(
        stored_batches(&pool).await.is_empty(),
        "never taken as it was"
    );
    for id in &ids {
        assert_eq!(
            delivery_state(&pool, *id).await,
            Some(("queued".into(), 1, None))
        );
    }
    make_due(&pool).await;
    disp.run_once().await.unwrap();
    let requests = flaky.requests();
    assert_eq!(requests.len(), 5);
    assert_ne!(requests[4].0, requests[0].0);
    for id in &ids {
        assert_eq!(delivery_state(&pool, *id).await.unwrap().0, "sent");
    }
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_provider_outage_never_gives_up_on_a_mail() {
    let Some((pool, name)) = isolated("suboutage").await else {
        return;
    };
    one_update_to(&pool, 1).await;
    let flaky = Arc::new(Flaky::new(
        (0..20).map(|_| EmailError::Transport("502 bad gateway".into())),
    ));
    let disp = dispatcher(pool.clone(), flaky);
    disp.run_once().await.unwrap();
    let [(key, _, ids)] = stored_batches(&pool).await.try_into().unwrap();
    sqlx::query("UPDATE status_page_subscriber_deliveries SET attempts = 30")
        .execute(&pool)
        .await
        .unwrap();
    make_due(&pool).await;
    disp.run_once().await.unwrap();
    assert_eq!(
        delivery_state(&pool, ids[0]).await,
        Some(("queued".into(), 31, Some(key))),
        "it waits for the provider until the notice expires"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_batch_refused_as_a_whole_waits_without_an_attempt() {
    let Some((pool, name)) = isolated("subwhole").await else {
        return;
    };
    one_update_to(&pool, 2).await;
    let flaky = Arc::new(Flaky::new([EmailError::Rejected(
        "422 validation_error: invalid from".into(),
    )]));
    dispatcher(pool.clone(), flaky.clone())
        .run_once()
        .await
        .unwrap();
    assert_eq!(
        flaky.requests().len(),
        1,
        "a refusal is not resent in process"
    );
    let rows: Vec<(String, i32, f64)> = sqlx::query_as(
        "SELECT status, attempts, EXTRACT(EPOCH FROM next_attempt_at - now())::float8
         FROM status_page_subscriber_deliveries",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for (status, attempts, due_in) in rows {
        assert_eq!((status.as_str(), attempts), ("queued", 0));
        assert!((890.0..=901.0).contains(&due_in), "{due_in}");
    }
    assert!(stored_batches(&pool).await.is_empty());
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_delivery_still_unsent_a_day_after_its_notice_expires() {
    let Some((pool, name)) = isolated("subexpiry").await else {
        return;
    };
    one_update_to(&pool, 1).await;
    dispatcher(pool.clone(), Arc::new(Paced(InMemoryEmailSender::new())))
        .run_once()
        .await
        .unwrap();
    sqlx::query("UPDATE status_page_notices SET created_at = now() - INTERVAL '25 hours'")
        .execute(&pool)
        .await
        .unwrap();
    make_due(&pool).await;
    let mem = Arc::new(InMemoryEmailSender::new());
    dispatcher(pool.clone(), mem.clone())
        .run_once()
        .await
        .unwrap();
    assert!(mem.sent().is_empty());
    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM status_page_subscriber_deliveries")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(statuses, ["expired"]);
    done(pool, &name).await;
}

/// A webhook receiver answering each POST with the next of `answers`.
async fn receiver(answers: Vec<(u16, Option<&'static str>)>) -> String {
    use axum::http::{HeaderValue, StatusCode, header::RETRY_AFTER};
    use axum::response::IntoResponse;
    let answers = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
        answers,
    )));
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move || {
            let answers = answers.clone();
            async move {
                let (status, wait) = answers.lock().unwrap().pop_front().unwrap_or((204, None));
                let mut res = StatusCode::from_u16(status).unwrap().into_response();
                if let Some(wait) = wait {
                    res.headers_mut()
                        .insert(RETRY_AFTER, HeaderValue::from_static(wait));
                }
                res
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://127.0.0.1:{port}/hook")
}

/// A page whose one subscriber is a webhook at a receiver answering with
/// `answers`, with one update to tell it.
async fn update_to_receiver(pool: &PgPool, answers: Vec<(u16, Option<&'static str>)>) -> Uuid {
    let org = seed_org(pool).await;
    let target = seed_target(pool, org, "mon").await;
    let page = seed_page(pool, org).await;
    add_component(pool, org, page, target).await;
    let hook = subscribers::subscribe(
        pool,
        &NewSubscriber {
            status_page_id: page,
            org_id: org,
            channel: SubscriberChannel::Webhook,
            target: receiver(answers).await,
            config: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
    subscribers::mark_verified(pool, hook.id).await.unwrap();
    let incident = seed_incident(pool, org, target, "public").await;
    add_update(pool, org, incident, 0).await;
    hook.id
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_webhook_out_of_attempts_is_given_up() {
    let Some((pool, name)) = isolated("subhookcap").await else {
        return;
    };
    let hook = update_to_receiver(&pool, vec![(500, None)]).await;
    let disp = dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()));
    disp.plan_once().await.unwrap();
    sqlx::query("UPDATE status_page_subscriber_deliveries SET attempts = 7")
        .execute(&pool)
        .await
        .unwrap();
    disp.run_once().await.unwrap();
    let (status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempts FROM status_page_subscriber_deliveries WHERE subscriber_id = $1",
    )
    .bind(hook)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), attempts), ("failed", 8));
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_receiver_that_asks_to_wait_is_waited_for_without_an_attempt() {
    let Some((pool, name)) = isolated("subhookwait").await else {
        return;
    };
    let hook = update_to_receiver(&pool, vec![(429, Some("120")), (500, None)]).await;
    let disp = dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()));
    let state = || {
        sqlx::query_as::<_, (String, i32, f64)>(
            "SELECT status, attempts, EXTRACT(EPOCH FROM next_attempt_at - now())::float8
             FROM status_page_subscriber_deliveries WHERE subscriber_id = $1",
        )
        .bind(hook)
        .fetch_one(&pool)
    };

    disp.run_once().await.unwrap();
    let (status, attempts, due_in) = state().await.unwrap();
    assert_eq!(
        (status.as_str(), attempts),
        ("queued", 0),
        "429 asked to wait"
    );
    assert!(
        (110.0..=121.0).contains(&due_in),
        "at its Retry-After: {due_in}"
    );

    make_due(&pool).await;
    disp.run_once().await.unwrap();
    let (status, attempts, due_in) = state().await.unwrap();
    assert_eq!((status.as_str(), attempts), ("queued", 1), "a 500 counts");
    assert!((25.0..=46.0).contains(&due_in), "backoff: {due_in}");

    make_due(&pool).await;
    disp.run_once().await.unwrap();
    assert_eq!(state().await.unwrap().0, "sent");
    done(pool, &name).await;
}

/// The migration's seed, run on a database that already holds what
/// subscribers were told: nothing told before it goes out again, and an
/// outage still open sends exactly its resolved mail when it closes.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_cutover_sends_nothing_already_told_and_still_ends_what_is_open() {
    let Some((pool, name)) = isolated("subcutover").await else {
        return;
    };
    let closed = five_down(&pool, 30).await;
    close_all(&pool, &closed, 25, 22).await;
    confirmed_subscriber(&pool, closed.org, closed.page, "closed@example.com").await;
    let open = five_down(&pool, 3).await;
    confirmed_subscriber(&pool, open.org, open.page, "open@example.com").await;
    let manual = seed_org(&pool).await;
    let manual_page = seed_page(&pool, manual).await;
    confirmed_subscriber(&pool, manual, manual_page, "manual@example.com").await;
    let declared: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, started_at, status_at_start, origin, visibility, public_title)
         VALUES ($1, now(), 'down', 'manual', 'public', 'Declared') RETURNING id",
    )
    .bind(manual)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO incident_status_pages (org_id, incident_id, status_page_id) VALUES ($1, $2, $3)",
    )
    .bind(manual)
    .bind(declared)
    .bind(manual_page)
    .execute(&pool)
    .await
    .unwrap();
    let told_update = add_update(&pool, manual, declared, 0).await;
    // Posted moments before the deploy: the old log never got to it.
    let untold_update = add_update(&pool, manual, declared, 0).await;

    // What the old log had sent in the last moments: the declared incident's
    // first update. The monitors' updates are minutes old, so the old
    // dispatcher had settled them whether its log shows a delivery or not.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "CREATE TEMP TABLE told (status_page_id uuid, org_id uuid, event_kind text,
                                 event_ref uuid, phase text) ON COMMIT DROP",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO told VALUES ($1, $2, 'incident_update', $3, '')")
        .bind(manual_page)
        .bind(manual)
        .bind(told_update)
        .execute(&mut *tx)
        .await
        .unwrap();
    let migration = include_str!("../migrations/postgres/105_subscriber_notices.up.sql");
    let seed = &migration[migration
        .find("-- What subscribers were already told")
        .expect("the seed section")..];
    sqlx::raw_sql(seed).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.run_once().await.unwrap();
    let sent: Vec<String> = mem.sent().into_iter().map(|m| m.to.address).collect();
    assert_eq!(
        sent,
        ["manual@example.com"],
        "only what the old log never sent goes out"
    );
    let untold: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM status_page_notices WHERE source_ref = $1 AND payload <> '{}'::jsonb",
    )
    .bind(untold_update)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(untold, 1);
    mem.clear();

    close_all(&pool, &open, 8, 6).await;
    disp.run_once().await.unwrap();
    let mails = mem.sent();
    assert_eq!(
        mails.len(),
        5,
        "each seeded outage ends on its own: {mails:?}"
    );
    assert!(mails.iter().all(|m| m.to.address == "open@example.com"));
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_pages_first_subscriber_gets_no_backlog() {
    let Some((pool, name)) = isolated("subfirst").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    add_component(&pool, org, page, target).await;
    let incident = seed_incident(&pool, org, target, "public").await;
    add_update(&pool, org, incident, 2).await;
    let disp = dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()));
    disp.plan_once().await.unwrap();

    let sub = confirmed_subscriber(&pool, org, page, "first@example.com").await;
    disp.plan_once().await.unwrap();
    assert!(
        owed(&pool, sub).await.is_empty(),
        "posted before they subscribed"
    );
    let next = add_update(&pool, org, incident, 0).await;
    disp.plan_once().await.unwrap();
    assert_eq!(sources(&owed(&pool, sub).await, "incident_update"), [next]);
    done(pool, &name).await;
}

fn components(mail: &TransactionalEmail) -> Vec<String> {
    match &mail.template {
        EmailTemplate::SubscriberOutage(o) => o.components.iter().map(|c| c.name.clone()).collect(),
        other => panic!("expected an outage mail, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_monitor_that_flapped_back_in_is_one_component() {
    let Some((pool, name)) = isolated("subflap").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let page = seed_page(&pool, org).await;
    let target = seed_target(&pool, org, "API").await;
    add_component(&pool, org, page, target).await;
    confirmed_subscriber(&pool, org, page, "flap@example.com").await;
    // Down 30 to 28 minutes ago, close posted 27; down again 25 to 23, close 22.
    for (start, end, posted) in [(30, 28, 27), (25, 23, 22)] {
        let incident: Uuid = sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, ended_at, resolved_at, state,
                                    status_at_start, visibility)
             VALUES ($1, $2, now() - make_interval(mins => $3), now() - make_interval(mins => $4),
                     now() - make_interval(mins => $5), 'resolved', 'down', 'public')
             RETURNING id",
        )
        .bind(org)
        .bind(target)
        .bind(start)
        .bind(end)
        .bind(posted)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated,
                                           posted_at)
             VALUES ($1, $2, 'investigating', 'Automatically detected', 'system', true,
                     now() - make_interval(mins => $3))",
        )
        .bind(org)
        .bind(incident)
        .bind(start - 1)
        .execute(&pool)
        .await
        .unwrap();
    }
    let mem = Arc::new(InMemoryEmailSender::new());
    dispatcher(pool.clone(), mem.clone())
        .run_once()
        .await
        .unwrap();
    let mails = mails_to(&mem, "flap@example.com");
    assert_eq!(mails.len(), 1, "one summary for both flaps");
    assert_eq!(components(&mails[0]), ["API"]);
    let rendered = mails[0].template.render("Uptimepage");
    assert!(
        rendered.subject.contains("API down"),
        "{}",
        rendered.subject
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_started_mail_lists_what_is_down_and_the_resolved_mail_what_came_back() {
    let Some((pool, name)) = isolated("subleave").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    confirmed_subscriber(&pool, outage.org, outage.page, "leave@example.com").await;
    // Website came back inside the first minute.
    sqlx::query(
        "UPDATE incidents SET ended_at = now() - interval '90 seconds',
                              resolved_at = now() - interval '80 seconds', state = 'resolved'
         WHERE id = $1",
    )
    .bind(outage.incidents[0])
    .execute(&pool)
    .await
    .unwrap();
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.run_once().await.unwrap();
    let started = mails_to(&mem, "leave@example.com");
    assert_eq!(
        components(&started[0]),
        ["API", "Dashboard", "Checkout", "Webhooks"]
    );

    // Webhooks is taken off the page while still down; the rest recover.
    sqlx::query(
        "DELETE FROM status_page_components WHERE status_page_id = $1
           AND target_id = (SELECT target_id FROM incidents WHERE id = $2)",
    )
    .bind(outage.page)
    .bind(outage.incidents[4])
    .execute(&pool)
    .await
    .unwrap();
    disp.run_once().await.unwrap();
    // Webhooks' incident ends later too, off the page.
    let rest = Outage {
        org: outage.org,
        page: outage.page,
        incidents: outage.incidents[1..5].to_vec(),
    };
    close_all(&pool, &rest, 8, 7).await;
    // Website's early recovery and Webhooks' removal closed their memberships
    // at the passes above; let the settle run out for them too.
    sqlx::query(
        "UPDATE status_page_outage_members SET closed_at = now() - interval '6 minutes'
         WHERE incident_id = ANY($1)",
    )
    .bind(vec![outage.incidents[0], outage.incidents[4]])
    .execute(&pool)
    .await
    .unwrap();
    disp.run_once().await.unwrap();
    let mails = mails_to(&mem, "leave@example.com");
    assert_eq!(mails.len(), 2);
    assert_eq!(
        components(&mails[1]),
        ["Website", "API", "Dashboard", "Checkout"],
        "Webhooks is still down: the resolved mail does not call it back up"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_page_that_kept_a_monitor_incident_mails_its_automatic_closer() {
    let Some((pool, name)) = isolated("subkeptauto").await else {
        return;
    };
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org, "mon").await;
    let page = seed_page(&pool, org).await;
    let sub = confirmed_subscriber(&pool, org, page, "keptauto@example.com").await;
    let incident = seed_incident(&pool, org, target, "public").await;
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
    sqlx::query(
        "UPDATE incidents SET ended_at = now(), resolved_at = now(), state = 'resolved' WHERE id = $1",
    )
    .bind(incident)
    .execute(&pool)
    .await
    .unwrap();
    let closer: Uuid = sqlx::query_scalar(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated)
         VALUES ($1, $2, 'resolved', 'Automatically resolved', 'system', true) RETURNING id",
    )
    .bind(org)
    .bind(incident)
    .fetch_one(&pool)
    .await
    .unwrap();
    dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()))
        .plan_once()
        .await
        .unwrap();
    assert_eq!(
        sources(&owed(&pool, sub).await, "incident_update"),
        [closer],
        "no outage on this page tells it, so the closer goes out itself"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_outage_taken_off_the_page_before_it_was_announced_tells_only_its_end() {
    let Some((pool, name)) = isolated("subhidden").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    confirmed_subscriber(&pool, outage.org, outage.page, "hidden@example.com").await;
    for incident in &outage.incidents {
        mark_published(&pool, outage.org, *incident).await;
    }
    sqlx::query("UPDATE incidents SET visibility = 'internal' WHERE org_id = $1")
        .bind(outage.org)
        .execute(&pool)
        .await
        .unwrap();
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.run_once().await.unwrap();
    assert!(mem.is_empty(), "unpublished: no started mail");

    close_all(&pool, &outage, 7, 6).await;
    disp.run_once().await.unwrap();
    let mails = mails_to(&mem, "hidden@example.com");
    assert_eq!(mails.len(), 1, "it was published once, so its end is told");
    assert_eq!(outage_mail(&mails[0]), (true, 5));
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_deleted_monitors_close_is_told_by_its_outage_once() {
    let Some((pool, name)) = isolated("subdeleted").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    let sub = confirmed_subscriber(&pool, outage.org, outage.page, "deleted@example.com").await;
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.run_once().await.unwrap();

    // What deleting a monitor leaves: its incident closed, kept on the page,
    // the component gone, and an automatic closer.
    let gone = outage.incidents[0];
    sqlx::query(
        "UPDATE incidents SET ended_at = now(), resolved_at = now(), state = 'resolved' WHERE id = $1",
    )
    .bind(gone)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO incident_status_pages (org_id, incident_id, status_page_id, component_name)
         VALUES ($1, $2, $3, 'Website')",
    )
    .bind(outage.org)
    .bind(gone)
    .bind(outage.page)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM status_page_components WHERE status_page_id = $1
           AND target_id = (SELECT target_id FROM incidents WHERE id = $2)",
    )
    .bind(outage.page)
    .bind(gone)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated)
         VALUES ($1, $2, 'resolved', 'The monitor was deleted', 'system', true)",
    )
    .bind(outage.org)
    .bind(gone)
    .execute(&pool)
    .await
    .unwrap();
    disp.plan_once().await.unwrap();
    assert!(
        sources(&owed(&pool, sub).await, "incident_update").is_empty(),
        "the outage tells it; no mail of its own"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_component_taken_off_the_page_while_down_still_has_its_recovery_mailed() {
    let Some((pool, name)) = isolated("subleftclose").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    let sub = confirmed_subscriber(&pool, outage.org, outage.page, "leftclose@example.com").await;
    let disp = dispatcher(pool.clone(), Arc::new(InMemoryEmailSender::new()));
    disp.run_once().await.unwrap();
    // Removing a component keeps its incident on the page.
    let left = outage.incidents[4];
    sqlx::query(
        "INSERT INTO incident_status_pages (org_id, incident_id, status_page_id, component_name)
         VALUES ($1, $2, $3, 'Webhooks')",
    )
    .bind(outage.org)
    .bind(left)
    .bind(outage.page)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM status_page_components WHERE status_page_id = $1
           AND target_id = (SELECT target_id FROM incidents WHERE id = $2)",
    )
    .bind(outage.page)
    .bind(left)
    .execute(&pool)
    .await
    .unwrap();
    disp.run_once().await.unwrap();

    sqlx::query(
        "UPDATE incidents SET ended_at = now(), resolved_at = now(), state = 'resolved' WHERE id = $1",
    )
    .bind(left)
    .execute(&pool)
    .await
    .unwrap();
    let closer: Uuid = sqlx::query_scalar(
        "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated)
         VALUES ($1, $2, 'resolved', 'Automatically resolved', 'system', true) RETURNING id",
    )
    .bind(outage.org)
    .bind(left)
    .fetch_one(&pool)
    .await
    .unwrap();
    disp.plan_once().await.unwrap();
    assert_eq!(
        sources(&owed(&pool, sub).await, "incident_update"),
        [closer],
        "its outage will not tell it, so its own closer does"
    );
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_outage_that_leaves_the_page_and_comes_back_is_not_announced_twice() {
    let Some((pool, name)) = isolated("subback").await else {
        return;
    };
    let outage = five_down(&pool, 2).await;
    confirmed_subscriber(&pool, outage.org, outage.page, "back@example.com").await;
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.run_once().await.unwrap();
    assert_eq!(mails_to(&mem, "back@example.com").len(), 1);

    let page_on = async |on: bool| {
        sqlx::query("UPDATE status_pages SET enabled = $2 WHERE id = $1")
            .bind(outage.page)
            .bind(on)
            .execute(&pool)
            .await
            .unwrap();
    };
    page_on(false).await;
    disp.run_once().await.unwrap();
    // The page stays off past the settle; the outage ends with nothing to say.
    sqlx::query("UPDATE status_page_outage_members SET closed_at = now() - interval '6 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE status_page_outages SET recovered_at = now() - interval '6 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    disp.run_once().await.unwrap();
    page_on(true).await;
    disp.run_once().await.unwrap();
    assert_eq!(
        mails_to(&mem, "back@example.com").len(),
        1,
        "still the same outage to its readers: no second started mail"
    );

    close_all(&pool, &outage, 7, 6).await;
    disp.run_once().await.unwrap();
    let mails = mails_to(&mem, "back@example.com");
    assert_eq!(mails.len(), 2, "and one resolved mail at the end");
    assert_eq!(outage_mail(&mails[1]), (true, 5));
    done(pool, &name).await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_reopened_incident_is_news_again() {
    let Some((pool, name)) = isolated("subreopen").await else {
        return;
    };
    let outage = five_down(&pool, 20).await;
    close_all(&pool, &outage, 18, 15).await;
    confirmed_subscriber(&pool, outage.org, outage.page, "reopen@example.com").await;
    let mem = Arc::new(InMemoryEmailSender::new());
    let disp = dispatcher(pool.clone(), mem.clone());
    disp.run_once().await.unwrap();
    assert_eq!(mails_to(&mem, "reopen@example.com").len(), 1, "the summary");

    sqlx::query(
        "UPDATE incidents SET ended_at = NULL, resolved_at = NULL, state = 'triggered'
         WHERE id = $1",
    )
    .bind(outage.incidents[0])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE incident_updates SET posted_at = now() - interval '2 minutes' WHERE incident_id = $1 AND phase = 'investigating'")
        .bind(outage.incidents[0])
        .execute(&pool)
        .await
        .unwrap();
    disp.run_once().await.unwrap();
    let mails = mails_to(&mem, "reopen@example.com");
    assert_eq!(mails.len(), 2, "down again is told");
    assert_eq!(outage_mail(&mails[1]), (false, 1));
    done(pool, &name).await;
}
