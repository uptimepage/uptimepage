//! Plan ceilings applied to the scheduler set, the agent pull, and its etag.
//! Live PG only; no-ops without `DATABASE_URL`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, build_test_app_with_pg_store, default_http_check, make_user, pg_pool_from_env,
    unique_slug,
};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::config::AppConfig;
use uptimepage::domain::{CheckSpec, ExpectedStatus, HeartbeatCheck, OrgId, Target, UserId};
use uptimepage::quotas::effective::{RegionCaps, plan_digest, region_targets, resolve_plans};
use uptimepage::quotas::{PlanGoverned, QuotaService, governed_interval};
use uptimepage::storage::admin::{AdminRepo, EnabledTargetStream};
use uuid::Uuid;

async fn org_on_plan(pool: &PgPool, plan: &str, interval_secs: i32) -> (OrgId, Uuid, UserId) {
    let check = CheckSpec::Http(default_http_check(
        "https://example.com".parse().expect("url"),
        ExpectedStatus::Exact(200),
    ));
    org_on_plan_watching(pool, plan, interval_secs, check).await
}

/// The kind is fixed at insert: a monitor's check type cannot change later,
/// so a test that needs another kind starts with it.
async fn org_on_plan_watching(
    pool: &PgPool,
    plan: &str,
    interval_secs: i32,
    check: CheckSpec,
) -> (OrgId, Uuid, UserId) {
    let user = make_user(pool, "govern").await;
    let (account,): (Uuid,) = sqlx::query_as(
        "INSERT INTO accounts (owner_user_id, plan_id) VALUES ($1, $2) \
         ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL \
         DO UPDATE SET plan_id = excluded.plan_id RETURNING id",
    )
    .bind(user.0)
    .bind(plan)
    .fetch_one(pool)
    .await
    .expect("account");
    let (org,): (Uuid,) = sqlx::query_as(
        "INSERT INTO organizations (slug, name, account_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(unique_slug("gov"))
    .bind("Governed")
    .bind(account)
    .fetch_one(pool)
    .await
    .expect("org");
    let (target,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 'probe', $2, $3, true) RETURNING id",
    )
    .bind(org)
    // decode_targets_skipping silently drops a row whose spec will not parse.
    .bind(serde_json::to_value(check).expect("check spec"))
    .bind(interval_secs)
    .fetch_one(pool)
    .await
    .expect("target");
    (OrgId(org), target, user)
}

async fn cleanup(pool: &PgPool, org: OrgId, user: UserId) {
    let account: Option<(Uuid,)> =
        sqlx::query_as("SELECT account_id FROM organizations WHERE id = $1")
            .bind(org.0)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(pool)
        .await;
    if let Some((account,)) = account {
        let _ = sqlx::query("DELETE FROM plan_overrides WHERE account_id = $1")
            .bind(account)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account)
            .execute(pool)
            .await;
    }
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(pool)
        .await;
}

fn sms_config(to: &str, auth_token: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "sms",
        "provider": "twilio",
        "to": to,
        "account_sid": "AC00000000000000000000000000000000",
        "auth_token": auth_token,
        "from": "+15559876543"
    })
}

fn sms_create_request(name: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/notification-channels")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": name,
                "config": sms_config("+15551234567", "tok"),
            })
            .to_string(),
        ))
        .unwrap()
}

/// A region of its own per test: the seeded catalog is shared with every
/// other suite on this database.
async fn fresh_region(pool: &PgPool, prefix: &str, default_selected: bool) -> String {
    let region = unique_slug(prefix);
    sqlx::query(
        "INSERT INTO regions (id, name, enabled, default_selected) VALUES ($1, $1, true, $2)",
    )
    .bind(&region)
    .bind(default_selected)
    .execute(pool)
    .await
    .expect("region");
    region
}

async fn assign_region(pool: &PgPool, target: Uuid, region: &str) {
    sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
        .bind(target)
        .bind(region)
        .execute(pool)
        .await
        .expect("assign region");
}

async fn drop_region(pool: &PgPool, region: &str) {
    let _ = sqlx::query("DELETE FROM regions WHERE id = $1")
        .bind(region)
        .execute(pool)
        .await;
}

async fn region_plans(pool: &PgPool, region: &str) -> uptimepage::quotas::effective::PlanMap {
    let cfg = AppConfig::load().expect("config");
    let quotas = QuotaService::new(&cfg, Some(pool.clone()));
    let repo = AdminRepo::new(pool.clone(), None, "plan_governance_test");
    resolve_plans(&quotas, repo.region_org_ids(region).await.expect("org ids")).await
}

/// The validator an agent polling `region` would be handed.
async fn region_etag(pool: &PgPool, region: &str) -> String {
    let repo = AdminRepo::new(pool.clone(), None, "plan_governance_test");
    let plans = region_plans(pool, region).await;
    repo.region_pull_etag(region, &RegionCaps::from(&plans), &plan_digest(&plans))
        .await
        .expect("etag")
}

/// What the scheduler in `region`, or an agent pulling for it, is handed.
async fn region_set(pool: &PgPool, region: &str) -> Vec<(OrgId, Target)> {
    let repo = AdminRepo::new(pool.clone(), None, "plan_governance_test");
    let plans = region_plans(pool, region).await;
    region_targets(&repo, region, true, &plans)
        .await
        .expect("region set")
}

fn governed_writer(pool: &PgPool, cfg: &AppConfig) -> PlanGoverned<AdminRepo> {
    let repo = AdminRepo::new(pool.clone(), None, "plan_governance_test");
    let quotas = Arc::new(QuotaService::new(cfg, Some(pool.clone())));
    PlanGoverned::new(Arc::new(repo), quotas)
}

/// Every enabled target as the incident writer pages through them.
async fn writer_walk(pool: &PgPool, cfg: &AppConfig) -> Vec<(OrgId, Target)> {
    let source = governed_writer(pool, cfg);
    let mut cursor = None;
    let mut paged = Vec::new();
    loop {
        let page = source
            .next_enabled_target_page(cursor, 500)
            .await
            .expect("page");
        let Some((last_org, last)) = page.last() else {
            break;
        };
        cursor = Some(uptimepage::storage::admin::PublicTargetCursor::after(
            *last_org, last.id,
        ));
        paged.extend(page);
    }
    paged
}

fn ids_of(targets: &[(OrgId, Target)]) -> Vec<Uuid> {
    targets.iter().map(|(_, t)| t.id).collect()
}

async fn set_override(pool: &PgPool, org: OrgId, override_json: &str) {
    sqlx::query(
        "INSERT INTO plan_overrides (account_id, override_json, reason) \
         SELECT account_id, $2::jsonb, 'test' FROM organizations WHERE id = $1 \
         ON CONFLICT (account_id) DO UPDATE SET override_json = excluded.override_json",
    )
    .bind(org.0)
    .bind(override_json)
    .execute(pool)
    .await
    .expect("override");
}

fn interval_of(targets: &[(OrgId, Target)], id: Uuid) -> Duration {
    targets
        .iter()
        .find(|(_, t)| t.id == id)
        .map(|(_, t)| t.interval)
        .expect("target present in the scheduler set")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_below_its_plan_floor_is_slowed_for_the_scheduler() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "free", 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;

    let listed = region_set(&pool, &region).await;
    assert_eq!(interval_of(&listed, target), Duration::from_secs(180));

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn governing_never_rewrites_the_stored_interval() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "free", 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;

    let _ = region_set(&pool, &region).await;

    let (stored,): (i32,) = sqlx::query_as("SELECT interval_secs FROM targets WHERE id = $1")
        .bind(target)
        .fetch_one(&pool)
        .await
        .expect("stored interval");
    assert_eq!(stored, 60, "the clamp must not write back");

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_above_its_plan_floor_is_left_alone() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "team", 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;

    let listed = region_set(&pool, &region).await;
    assert_eq!(interval_of(&listed, target), Duration::from_secs(60));

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_downgrade_takes_effect_without_touching_the_monitor() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "team", 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;

    let before = region_set(&pool, &region).await;
    assert_eq!(interval_of(&before, target), Duration::from_secs(60));

    sqlx::query(
        "UPDATE accounts SET plan_id = 'free' \
         WHERE id = (SELECT account_id FROM organizations WHERE id = $1)",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .expect("downgrade");

    let after = region_set(&pool, &region).await;
    assert_eq!(interval_of(&after, target), Duration::from_secs(180));

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_incident_writer_walks_the_same_slowed_interval_as_the_scheduler() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let cfg = AppConfig::load().expect("config");
    let (org, target, user) = org_on_plan(&pool, "free", 30).await;

    let paged = writer_walk(&pool, &cfg).await;
    assert_eq!(interval_of(&paged, target), Duration::from_secs(180));

    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_override_lifts_the_floor_for_one_account() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "free", 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;
    set_override(&pool, org, r#"{"min_check_interval_secs": 30}"#).await;

    let listed = region_set(&pool, &region).await;
    assert_eq!(interval_of(&listed, target), Duration::from_secs(60));

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_over_its_region_cap_is_probed_from_its_first_regions_only() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "team", 60).await;
    // Sorts ahead by id, yet the default-on region comes first.
    let extra = fresh_region(&pool, "a-reg", false).await;
    let home = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &extra).await;
    assign_region(&pool, target, &home).await;

    assert!(ids_of(&region_set(&pool, &home).await).contains(&target));
    assert!(ids_of(&region_set(&pool, &extra).await).contains(&target));

    set_override(&pool, org, r#"{"max_regions": 1}"#).await;

    assert!(
        ids_of(&region_set(&pool, &home).await).contains(&target),
        "the default-on region keeps the monitor"
    );
    assert!(
        !ids_of(&region_set(&pool, &extra).await).contains(&target),
        "the region past the cap is no longer served"
    );
    let (assigned,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM target_regions WHERE target_id = $1")
            .bind(target)
            .fetch_one(&pool)
            .await
            .expect("assignments");
    assert_eq!(assigned, 2, "the cap must not unassign a region");

    cleanup(&pool, org, user).await;
    drop_region(&pool, &extra).await;
    drop_region(&pool, &home).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_disabled_region_does_not_take_a_slot_from_the_cap() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "team", 60).await;
    let dark = fresh_region(&pool, "reg", true).await;
    sqlx::query("UPDATE regions SET enabled = false WHERE id = $1")
        .bind(&dark)
        .execute(&pool)
        .await
        .expect("disable region");
    let live = fresh_region(&pool, "reg", false).await;
    assign_region(&pool, target, &dark).await;
    assign_region(&pool, target, &live).await;
    set_override(&pool, org, r#"{"max_regions": 1}"#).await;

    assert!(ids_of(&region_set(&pool, &live).await).contains(&target));

    cleanup(&pool, org, user).await;
    drop_region(&pool, &dark).await;
    drop_region(&pool, &live).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_region_etag_changes_when_the_region_cap_does() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "team", 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;

    let before = region_etag(&pool, &region).await;
    set_override(&pool, org, r#"{"max_regions": 1}"#).await;
    let after = region_etag(&pool, &region).await;
    assert_ne!(
        before, after,
        "a region cap change must invalidate the pull"
    );

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_region_etag_changes_when_another_region_frees_a_slot() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "team", 60).await;
    let home = fresh_region(&pool, "reg", true).await;
    let spare = fresh_region(&pool, "reg", false).await;
    assign_region(&pool, target, &home).await;
    assign_region(&pool, target, &spare).await;
    set_override(&pool, org, r#"{"max_regions": 1}"#).await;

    let before = region_etag(&pool, &spare).await;
    assert!(!ids_of(&region_set(&pool, &spare).await).contains(&target));

    sqlx::query("UPDATE regions SET enabled = false WHERE id = $1")
        .bind(&home)
        .execute(&pool)
        .await
        .expect("disable region");

    assert!(
        ids_of(&region_set(&pool, &spare).await).contains(&target),
        "the freed slot must hand the monitor to the next region"
    );
    assert_ne!(
        before,
        region_etag(&pool, &spare).await,
        "a monitor arriving from another region's slot must invalidate the pull"
    );

    cleanup(&pool, org, user).await;
    drop_region(&pool, &home).await;
    drop_region(&pool, &spare).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_region_etag_changes_when_the_plan_does() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, target, user) = org_on_plan(&pool, "team", 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;

    let before = region_etag(&pool, &region).await;

    sqlx::query(
        "UPDATE accounts SET plan_id = 'free' \
         WHERE id = (SELECT account_id FROM organizations WHERE id = $1)",
    )
    .bind(org.0)
    .execute(&pool)
    .await
    .expect("downgrade");

    let after = region_etag(&pool, &region).await;
    assert_ne!(
        before, after,
        "a plan change must invalidate the region pull"
    );

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_region_etag_changes_when_the_plans_floor_is_retuned() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // Its own plan row: retuning a seeded tier would race every other suite
    // sharing this database.
    let plan = unique_slug("pl").replace('-', "_");
    sqlx::query(
        "INSERT INTO plans (id, name, description, max_orgs, max_targets, \
         min_check_interval_secs, retention_days, max_members, \
         max_pending_invitations, max_api_tokens_per_user, max_public_components, \
         max_status_pages, max_share_links_per_monitor, max_shared_monitors, \
         max_maintenance_windows, max_notification_channels, max_logo_size_bytes, \
         api_writes_per_minute, api_reads_per_minute, bulk_ops_per_minute, \
         test_now_per_minute, check_now_per_minute) \
         VALUES ($1, 'Throwaway', 'test', 1, 1, 30, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, \
                 1, 1, 1, 1, 1)",
    )
    .bind(&plan)
    .execute(&pool)
    .await
    .expect("throwaway plan");
    let (org, target, user) = org_on_plan(&pool, &plan, 60).await;
    let region = fresh_region(&pool, "reg", true).await;
    assign_region(&pool, target, &region).await;

    let before = region_etag(&pool, &region).await;

    sqlx::query("UPDATE plans SET min_check_interval_secs = 45 WHERE id = $1")
        .bind(&plan)
        .execute(&pool)
        .await
        .expect("retune");
    let after = region_etag(&pool, &region).await;

    assert_ne!(before, after, "a retuned floor must invalidate the pull");

    cleanup(&pool, org, user).await;
    drop_region(&pool, &region).await;
    let _ = sqlx::query("DELETE FROM plans WHERE id = $1")
        .bind(&plan)
        .execute(&pool)
        .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_without_sms_refuses_a_text_message_channel() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/notification-channels")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "pager",
                        "config": {
                            "type": "sms",
                            "provider": "twilio",
                            "to": "+15551234567",
                            "account_sid": "AC00000000000000000000000000000000",
                            "auth_token": "tok",
                            "from": "+15559876543"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("request");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "SMS_ALERTS_DISABLED");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_heartbeat_keeps_its_own_interval_under_a_slower_plan() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let cfg = AppConfig::load().expect("config");
    let heartbeat = CheckSpec::Heartbeat(HeartbeatCheck {
        period: Duration::from_secs(300),
        grace: Duration::from_secs(60),
        max_runtime: None,
    });
    let (org, target, user) = org_on_plan_watching(&pool, "free", 60, heartbeat).await;

    let listed = writer_walk(&pool, &cfg).await;
    assert_eq!(interval_of(&listed, target), Duration::from_secs(60));

    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_without_sms_refuses_a_patch_into_a_text_message_channel() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = true).await;

    let created = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/notification-channels")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": unique_slug("hook"),
                        "config": {"type": "webhook", "url": "https://example.com/hook"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("create");
    assert_eq!(created.status(), StatusCode::CREATED);
    let id = body_json(created).await["id"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/notification-channels/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "config": {
                            "type": "sms",
                            "provider": "twilio",
                            "to": "+15551234567",
                            "account_sid": "AC00000000000000000000000000000000",
                            "auth_token": "tok",
                            "from": "+15559876543"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("patch");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "SMS_ALERTS_DISABLED");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_self_hosted_install_may_add_a_text_message_channel() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = false).await;

    let resp = app
        .oneshot(sms_create_request(&unique_slug("sms")))
        .await
        .expect("create");

    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the plans row is the operator's own on a self-hosted install"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_existing_text_message_channel_stays_editable_after_a_downgrade() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // One org on a plan without SMS, holding a channel from when it had one:
    // seeded directly, because the create path is exactly what now refuses it.
    let (app, org) = build_test_app_with_pg_store(pool.clone(), |cfg| {
        cfg.marketing.enabled = true;
    })
    .await;
    let name = unique_slug("sms");
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO notification_channels (org_id, name, kind, config, verified_at) \
         VALUES ($1, $2, 'sms', $3, now()) RETURNING id",
    )
    .bind(org.0)
    .bind(&name)
    .bind(sms_config("+15551234567", "leaked"))
    .fetch_one(&pool)
    .await
    .expect("seed the channel the plan no longer grants");

    // Replacing the config is the only path the gate sees; a rename never
    // reaches it. Rotating a leaked token must not need the plan back.
    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/notification-channels/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "config": sms_config("+15557654321", "rotated") })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("patch");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a stored SMS channel must stay correctable once the plan drops SMS"
    );

    let (stored,): (serde_json::Value,) =
        sqlx::query_as("SELECT config FROM notification_channels WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read back");
    assert_eq!(stored["to"], "+15557654321", "the edit must have landed");

    sqlx::query("DELETE FROM notification_channels WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_without_sms_refuses_a_test_send_of_one() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // The rehearsal for a create the plan would refuse has to refuse too, or
    // the entitlement is decorative: the send goes out all the same.
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/notification-channels/test")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "config": sms_config("+15551234567", "tok") }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("test send");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "SMS_ALERTS_DISABLED");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_patch_to_an_unknown_channel_is_not_found_not_forbidden() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // The gate must not answer ahead of the lookup, or a caller learns the
    // plan from an id that was never theirs.
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/notification-channels/{}", Uuid::now_v7()))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "config": sms_config("+15551234567", "tok") }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("patch");

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[test]
fn the_floor_only_ever_slows_a_monitor_down() {
    let mut plan = uptimepage::domain::quota::Plan {
        min_check_interval_secs: 120,
        ..plan_fixture()
    };
    assert_eq!(
        governed_interval(Duration::from_secs(30), &plan, "http"),
        Duration::from_secs(120)
    );
    plan.min_check_interval_secs = 10;
    assert_eq!(
        governed_interval(Duration::from_secs(30), &plan, "http"),
        Duration::from_secs(30)
    );
}

fn plan_fixture() -> uptimepage::domain::quota::Plan {
    let now = chrono::Utc::now();
    uptimepage::domain::quota::Plan {
        id: "fixture".into(),
        name: "Fixture".into(),
        description: String::new(),
        max_targets: 1,
        min_check_interval_secs: 1,
        retention_days: 1,
        raw_days: 1,
        evidence_days: 1,
        max_members: 1,
        max_pending_invitations: 1,
        max_api_tokens_per_user: 1,
        max_public_components: 1,
        max_status_pages: 1,
        max_share_links_per_monitor: 1,
        max_shared_monitors: 1,
        max_maintenance_windows: 1,
        max_notification_channels: 1,
        max_escalation_policies: 1,
        max_on_call_schedules: 1,
        max_logo_size_bytes: 1,
        max_regions: 1,
        max_orgs: 1,
        api_writes_per_minute: 1,
        api_reads_per_minute: 1,
        bulk_ops_per_minute: 1,
        test_now_per_minute: 1,
        check_now_per_minute: 1,
        custom_domain_enabled: false,
        white_label_enabled: false,
        sms_alerts_enabled: false,
        incident_narration_enabled: true,
        on_call_enabled: true,
        max_flow_checks: 0,
        max_flow_steps: 30,
        is_listed: false,
        created_at: now,
        updated_at: now,
    }
}

fn escalation_policy_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/escalation-policies")
        .header("content-type", "application/json")
        .body(Body::from(
            // No steps: the payload has to be valid for the plan gate to be
            // what answers, and an empty ladder is a legal one.
            serde_json::json!({ "name": "ladder", "steps": [] }).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_without_on_call_refuses_a_new_escalation_policy() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // The test app's org lands on the seeded default, which 060 left without
    // on-call; the gate must answer before the cap does, so the customer is
    // told the tier is the reason rather than being shown a limit of zero.
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = true).await;

    let resp = app
        .oneshot(escalation_policy_request())
        .await
        .expect("create");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "ON_CALL_DISABLED");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_without_on_call_refuses_a_new_schedule() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = true).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/on-call/schedules")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "name": "primary",
                        "timezone": "UTC",
                        "layers": []
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("create");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "ON_CALL_DISABLED");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn clearing_an_escalation_binding_is_allowed_without_the_feature() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = true).await;

    // Unwiring is never new coverage, so an org that has lost the tier can
    // still take itself off a ladder it is already on.
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/escalation-policies/default")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "policy_id": null }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("clear");

    assert_ne!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_self_hosted_install_may_build_an_on_call_ladder() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, |cfg| cfg.marketing.enabled = false).await;

    let resp = app
        .oneshot(escalation_policy_request())
        .await
        .expect("create");

    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "the plans row is the operator's own on a self-hosted install"
    );
}

async fn seed_contact(pool: &PgPool, org: OrgId, user: UserId, name: &str) -> Uuid {
    let (channel,): (Uuid,) = sqlx::query_as(
        "INSERT INTO notification_channels (org_id, name, kind, config, verified_at) \
         VALUES ($1, $2, 'sms', $3, now()) RETURNING id",
    )
    .bind(org.0)
    .bind(unique_slug(name))
    .bind(sms_config("+15551234567", "tok"))
    .fetch_one(pool)
    .await
    .expect("channel");
    sqlx::query(
        "INSERT INTO user_contact_channels (org_id, user_id, channel_id) VALUES ($1, $2, $3)",
    )
    .bind(org.0)
    .bind(user.0)
    .bind(channel)
    .execute(pool)
    .await
    .expect("contact");
    channel
}

fn put_contacts(ids: &[Uuid]) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri("/api/v1/on-call/my-contacts")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "channel_ids": ids }).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn dropping_one_of_two_contacts_works_without_the_on_call_tier() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, _target, user) = org_on_plan(&pool, "free", 60).await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user.0)
        .bind(org.0)
        .execute(&pool)
        .await
        .expect("membership");
    let a = seed_contact(&pool, org, user, "ca").await;
    let b = seed_contact(&pool, org, user, "cb").await;

    let app = common::with_session(
        common::build_saas_router_with_pg_cfg(pool.clone(), |cfg| cfg.marketing.enabled = true)
            .await,
        user,
        Some(org),
        None,
    );

    // The endpoint replaces the whole set, so a removal arrives as a shorter
    // non-empty list. Gating on "not empty" would refuse exactly the case the
    // tier is not allowed to block: taking someone off the page rota.
    let resp = app
        .clone()
        .oneshot(put_contacts(&[a]))
        .await
        .expect("drop one");
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "removing a contact is not new paging coverage"
    );

    // Adding one back is, and stays refused.
    let resp = app.oneshot(put_contacts(&[a, b])).await.expect("add back");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(resp).await["error"]["code"], "ON_CALL_DISABLED");

    cleanup(&pool, org, user).await;
}
