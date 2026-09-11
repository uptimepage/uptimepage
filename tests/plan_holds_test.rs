//! Holding what a shrunken plan no longer covers, and releasing it when the
//! plan grows back. Live PG only; no-ops without `DATABASE_URL`.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    body_json, build_test_app_with_pg_store, default_http_check, make_user, pg_pool_from_env,
    unique_slug,
};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::config::AppConfig;
use uptimepage::domain::{AccountId, CheckSpec, ExpectedStatus, OrgId, Plan, UserId};
use uptimepage::quotas::QuotaService;
use uptimepage::quotas::holds::{
    PlanSource, accounts_needing_reconcile, list_held, reconcile_account,
};
use uuid::Uuid;

fn fixed(plan: &Plan) -> PlanSource<'static> {
    PlanSource::Fixed(Arc::new(plan.clone()))
}

/// The org's real plan, resolved the way every request resolves it, so a test
/// that then overrides one cap is still exercising a plan the catalog ships.
async fn plan_for(pool: &PgPool, org: OrgId) -> Plan {
    let cfg = AppConfig::load().expect("config");
    let quotas = QuotaService::new(&cfg, Some(pool.clone()));
    (*quotas.limit_for_org(org).await.expect("plan")).clone()
}

/// An account on `plan`, one org, and `n` monitors created oldest-first so the
/// hold order is deterministic rather than dependent on clock resolution.
async fn account_with_targets(
    pool: &PgPool,
    plan: &str,
    n: usize,
) -> (AccountId, OrgId, Vec<Uuid>, UserId) {
    let user = make_user(pool, "holds").await;
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
    .bind(unique_slug("hold"))
    .bind("Held")
    .bind(account)
    .fetch_one(pool)
    .await
    .expect("org");
    let mut ids = Vec::new();
    for i in 0..n {
        ids.push(add_target(pool, OrgId(org), &format!("m{i}"), i as i64, false).await);
    }
    (AccountId(account), OrgId(org), ids, user)
}

/// `age` orders creation explicitly: the reconcile ranks on `created_at`, and
/// rows inserted in one test can otherwise share a timestamp.
async fn add_target(pool: &PgPool, org: OrgId, name: &str, age: i64, flow: bool) -> Uuid {
    let spec = if flow {
        serde_json::json!({
            "type": "flow",
            "steps": [{
                "name": "open",
                "action": {"type": "goto", "url": "https://example.com"}
            }]
        })
    } else {
        serde_json::to_value(CheckSpec::Http(default_http_check(
            "https://example.com".parse().expect("url"),
            ExpectedStatus::Exact(200),
        )))
        .expect("spec")
    };
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, created_at) \
         VALUES ($1, $2, $3, 300, true, now() - make_interval(secs => $4)) RETURNING id",
    )
    .bind(org.0)
    .bind(name)
    .bind(spec)
    .bind((1000 - age) as f64)
    .fetch_one(pool)
    .await
    .expect("target");
    id
}

async fn add_page(pool: &PgPool, org: OrgId, name: &str, age: i64) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled, created_at) \
         VALUES ($1, $2, $3, true, now() - make_interval(secs => $4)) RETURNING id",
    )
    .bind(org.0)
    .bind(unique_slug("pg"))
    .bind(name)
    .bind((1000 - age) as f64)
    .fetch_one(pool)
    .await
    .expect("page");
    id
}

async fn held_ids(pool: &PgPool, org: OrgId) -> Vec<Uuid> {
    sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM targets WHERE org_id = $1 AND plan_hold_at IS NOT NULL ORDER BY name",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .expect("held")
    .into_iter()
    .map(|(i,)| i)
    .collect()
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

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_newest_monitors_are_held_and_the_oldest_keep_running() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 5).await;
    let plan = Plan {
        max_targets: 3,
        ..plan_for(&pool, org).await
    };

    let r = reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    assert_eq!(r.held, 2, "two over a cap of three");
    assert_eq!(r.released, 0);

    let held = held_ids(&pool, org).await;
    assert_eq!(
        held,
        vec![ids[3], ids[4]],
        "the two newest are held; the three the account was built on keep their slots"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn reconciling_twice_changes_nothing() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "free", 5).await;
    let plan = Plan {
        max_targets: 3,
        ..plan_for(&pool, org).await
    };
    let first = reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("first");
    let second = reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("second");
    assert_eq!(first.held, 2);
    assert!(
        !second.changed(),
        "the statement converges, so a repeat is a no-op: {second:?}"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_growing_back_releases_the_oldest_holds_first() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 5).await;
    let small = Plan {
        max_targets: 2,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&small), None)
        .await
        .expect("hold");
    assert_eq!(held_ids(&pool, org).await.len(), 3);

    let bigger = Plan {
        max_targets: 4,
        ..plan_for(&pool, org).await
    };
    let r = reconcile_account(&pool, account, fixed(&bigger), None)
        .await
        .expect("release");
    assert_eq!(r.released, 2);
    assert_eq!(r.held, 0);
    assert_eq!(
        held_ids(&pool, org).await,
        vec![ids[4]],
        "release mirrors hold: the newest row is the last one back"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_customers_pick_survives_a_reconcile() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 5).await;
    let plan = Plan {
        max_targets: 2,
        ..plan_for(&pool, org).await
    };
    // The newest monitor is the one that matters; default order would hold it.
    uptimepage::quotas::holds::set_keep(&pool, account, Some(&[ids[4]]), None)
        .await
        .expect("set keep");
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    let held = held_ids(&pool, org).await;
    assert!(
        !held.contains(&ids[4]),
        "a named monitor keeps its slot even though it is the newest"
    );
    assert_eq!(
        held.len(),
        4,
        "one seat of the two goes unused: naming one monitor is also declining \
         the rest, and the spare is not refilled from them"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_pick_smaller_than_the_plan_leaves_the_spare_seats_empty() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 5).await;
    let plan = Plan {
        max_targets: 4,
        ..plan_for(&pool, org).await
    };
    // Four seats, two named. Refilling the other two from the rows the
    // customer just gave up is what makes un-picking a monitor do nothing.
    uptimepage::quotas::holds::set_keep(&pool, account, Some(&[ids[0], ids[1]]), None)
        .await
        .expect("set keep");
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    let mut held = held_ids(&pool, org).await;
    held.sort();
    let mut want = vec![ids[2], ids[3], ids[4]];
    want.sort();
    assert_eq!(held, want, "everything the pick left out is held");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_that_covers_everything_releases_what_the_pick_left_out() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 5).await;
    let small = Plan {
        max_targets: 4,
        ..plan_for(&pool, org).await
    };
    uptimepage::quotas::holds::set_keep(&pool, account, Some(&[ids[0]]), None)
        .await
        .expect("set keep");
    reconcile_account(&pool, account, fixed(&small), None)
        .await
        .expect("hold");
    assert_eq!(held_ids(&pool, org).await.len(), 4);

    // A hold is the plan's mechanism. Once the plan covers the whole pool
    // there is nothing left for the pick to ration, and a customer who wants
    // one monitor quiet inside their plan pauses it instead.
    let big = Plan {
        max_targets: 5,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&big), None)
        .await
        .expect("release");
    assert!(
        held_ids(&pool, org).await.is_empty(),
        "the pick stops binding once no cap is exceeded"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_flow_cap_holds_flows_before_it_touches_ordinary_monitors() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 2).await;
    let f1 = add_target(&pool, org, "f1", 10, true).await;
    let f2 = add_target(&pool, org, "f2", 11, true).await;

    // Room for every monitor, but only one of the two flows.
    let plan = Plan {
        max_targets: 10,
        max_flow_checks: 1,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    let held = held_ids(&pool, org).await;
    assert_eq!(held, vec![f2], "only the newer flow is held");
    assert!(!held.contains(&ids[0]) && !held.contains(&ids[1]) && !held.contains(&f1));
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_flow_shortage_does_not_hold_the_monitors_nobody_picked() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 3).await;
    let flows = [
        add_target(&pool, org, "f0", 10, true).await,
        add_target(&pool, org, "f1", 11, true).await,
    ];
    // Only the flow cap is breached. A pick made about monitors says nothing
    // about a shortage in a cap it was never shown.
    uptimepage::quotas::holds::set_keep(&pool, account, Some(&[ids[0], flows[0]]), None)
        .await
        .expect("set keep");
    let plan = Plan {
        max_targets: 50,
        max_flow_checks: 1,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    assert_eq!(
        held_ids(&pool, org).await,
        vec![flows[1]],
        "the flow cap holds a flow; the ordinary monitors are inside their own cap"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_pick_is_forgotten_once_the_plan_covers_everything() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 4).await;
    uptimepage::quotas::holds::set_keep(&pool, account, Some(&[ids[0]]), None)
        .await
        .expect("set keep");
    let small = Plan {
        max_targets: 2,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&small), None)
        .await
        .expect("hold");

    let big = Plan {
        max_targets: 10,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&big), None)
        .await
        .expect("release");
    let (kept,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM targets WHERE org_id = $1 AND plan_keep")
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(
        kept, 0,
        "a spent pick would arm the next shortage against rows nobody declined"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn naming_no_status_pages_leaves_the_monitor_pick_alone() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 3).await;
    uptimepage::quotas::holds::set_keep(&pool, account, Some(&[ids[2]]), None)
        .await
        .expect("set monitors");
    // A picker showing only status pages saves only status pages.
    uptimepage::quotas::holds::set_keep(&pool, account, None, Some(&[]))
        .await
        .expect("set pages");
    let (kept,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM targets WHERE org_id = $1 AND plan_keep")
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(kept, 1, "the monitor pick was not part of that request");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_held_monitor_still_counts_against_the_cap() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "free", 5).await;
    let plan = Plan {
        max_targets: 3,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");

    let sql = format!(
        "SELECT count(*) FROM targets WHERE org_id IN ({})",
        uptimepage::storage::accounts::live_orgs("$1")
    );
    let (n,): (i64,) = sqlx::query_as(&sql)
        .bind(account.0)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(
        n, 5,
        "a held row keeps its slot; counting only live rows would let the \
         account create five more on top of the ones it is waiting on"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_held_monitor_leaves_the_scheduler_set() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 3).await;
    let repo = uptimepage::storage::admin::AdminRepo::new(pool.clone(), None, "plan holds test");
    let before = repo
        .list_all_enabled_targets()
        .await
        .expect("before")
        .into_iter()
        .filter(|(o, _)| *o == org)
        .count();
    assert_eq!(before, 3);

    let plan = Plan {
        max_targets: 1,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");

    let after: Vec<Uuid> = repo
        .list_all_enabled_targets()
        .await
        .expect("after")
        .into_iter()
        .filter(|(o, _)| *o == org)
        .map(|(_, t)| t.id)
        .collect();
    assert_eq!(after, vec![ids[0]], "only the covered monitor is scheduled");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_held_page_stops_resolving_publicly() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "free", 0).await;
    let first = add_page(&pool, org, "kept", 0).await;
    let second = add_page(&pool, org, "held", 1).await;
    let slug: (String,) = sqlx::query_as("SELECT slug::text FROM status_pages WHERE id = $1")
        .bind(second)
        .fetch_one(&pool)
        .await
        .expect("slug");

    assert!(
        uptimepage::storage::orgs::find_public_status_page_by_slug(&pool, &slug.0)
            .await
            .expect("resolve")
            .is_some(),
        "the page is live before the plan moves"
    );

    let plan = Plan {
        max_status_pages: 1,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");

    assert!(
        uptimepage::storage::orgs::find_public_status_page_by_slug(&pool, &slug.0)
            .await
            .expect("resolve")
            .is_none(),
        "a held page 404s rather than serving a page the plan does not cover"
    );
    let (still_enabled,): (bool,) =
        sqlx::query_as("SELECT enabled FROM status_pages WHERE id = $1")
            .bind(second)
            .fetch_one(&pool)
            .await
            .expect("enabled");
    assert!(
        still_enabled,
        "the customer's own switch is untouched, so a release restores exactly what they had"
    );
    let _ = first;
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_sweep_finds_an_over_cap_account_and_ignores_a_fitting_one() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // `free` sells 20 monitors, so 21 is over and 1 is not.
    let (over, over_org, _, over_user) = account_with_targets(&pool, "free", 21).await;
    let (under, under_org, _, under_user) = account_with_targets(&pool, "free", 1).await;

    let due: Vec<uptimepage::domain::AccountId> = accounts_needing_reconcile(&pool)
        .await
        .expect("candidates")
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    assert!(due.contains(&over), "the over-cap account is swept");
    assert!(
        !due.contains(&under),
        "an account that fits is skipped rather than locked and audited for a no-op"
    );

    // And once it holds something it stays a candidate, so a later upgrade is
    // noticed and the holds come back.
    let plan = Plan {
        max_targets: 5,
        ..plan_for(&pool, over_org).await
    };
    reconcile_account(&pool, over, fixed(&plan), None)
        .await
        .expect("reconcile");
    let (held, _) = list_held(&pool, over).await.expect("held");
    assert_eq!(held.len(), 16);
    let due2: Vec<uptimepage::domain::AccountId> = accounts_needing_reconcile(&pool)
        .await
        .expect("candidates")
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    assert!(
        due2.contains(&over),
        "an account holding rows stays in scope"
    );

    cleanup(&pool, over_org, over_user).await;
    cleanup(&pool, under_org, under_user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn holding_and_releasing_are_both_audited() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "free", 4).await;
    let small = Plan {
        max_targets: 2,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&small), None)
        .await
        .expect("hold");
    reconcile_account(&pool, account, fixed(&plan_for(&pool, org).await), None)
        .await
        .expect("release");

    let actions: Vec<(String, i64)> = sqlx::query_as(
        "SELECT action, (metadata->>'count')::bigint FROM org_audit_log \
         WHERE org_id = $1 AND action LIKE 'target.plan_%' ORDER BY occurred_at",
    )
    .bind(org.0)
    .fetch_all(&pool)
    .await
    .expect("audit");
    assert_eq!(
        actions,
        vec![
            ("target.plan_hold".to_string(), 2),
            ("target.plan_release".to_string(), 2)
        ],
        "the trail names what left and what came back, so a customer asking \
         why a monitor stopped has an answer"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_customers_pick_survives_the_next_sweep() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 5).await;
    let plan = Plan {
        max_targets: 2,
        ..plan_for(&pool, org).await
    };
    uptimepage::quotas::holds::set_keep(&pool, account, Some(&[ids[4]]), None)
        .await
        .expect("set keep");
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    assert!(!held_ids(&pool, org).await.contains(&ids[4]));

    // A later run knows nothing about the request that made the choice, so the
    // choice has to live in the row. Reconciling again with the same plan is
    // exactly what the daily sweep does.
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("again");
    assert!(
        !held_ids(&pool, org).await.contains(&ids[4]),
        "the pick is stored, so a later sweep cannot put it back on hold"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_paused_monitor_is_held_before_a_live_one() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, ids, user) = account_with_targets(&pool, "free", 3).await;
    // The oldest is switched off, so by age alone it would keep a slot while a
    // live monitor lost one.
    sqlx::query("UPDATE targets SET enabled = false WHERE id = $1")
        .bind(ids[0])
        .execute(&pool)
        .await
        .expect("pause");

    let plan = Plan {
        max_targets: 2,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");

    assert_eq!(
        held_ids(&pool, org).await,
        vec![ids[0]],
        "holding the monitor that was not running anyway costs the customer nothing"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_held_monitor_refuses_an_interactive_check() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = build_test_app_with_pg_store(pool.clone(), |_| {}).await;
    let account = uptimepage::storage::accounts::account_for_org(&pool, org)
        .await
        .expect("account");
    let live = add_target(&pool, org, "live", 0, false).await;
    let doomed = add_target(&pool, org, "doomed", 1, false).await;

    let plan = Plan {
        max_targets: 1,
        ..plan_for(&pool, org).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    assert_eq!(held_ids(&pool, org).await, vec![doomed]);

    // The guard belongs to the dispatch both front doors share, not to this
    // handler: the MCP tool reaches probing through the same function, so a
    // check placed in either handler alone would leave the other able to probe
    // a held monitor. It also has to precede region resolution, or a fleet with
    // no agent would answer 503 and hide the refusal.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/targets/{doomed}/check-now"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("check-now");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "PLAN_HOLD");
    let _ = live;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_org_owner_who_is_not_the_account_owner_cannot_touch_the_pool() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // One account, two orgs sharing its pool. The account owner delegates
    // ownership of the first org to somebody else, which is a membership role
    // any org owner can grant.
    let (account, first, _, owner) = account_with_targets(&pool, "free", 1).await;
    let (second,): (Uuid,) = sqlx::query_as(
        "INSERT INTO organizations (slug, name, account_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(unique_slug("sib"))
    .bind("Sibling")
    .bind(account.0)
    .fetch_one(&pool)
    .await
    .expect("sibling org");
    let secret = add_target(&pool, OrgId(second), "sibling-only", 5, false).await;

    let delegate = make_user(&pool, "delegate").await;
    for (u, o) in [(owner, first.0), (delegate, first.0)] {
        sqlx::query(
            "INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner') \
             ON CONFLICT DO NOTHING",
        )
        .bind(u.0)
        .bind(o)
        .execute(&pool)
        .await
        .expect("membership");
    }

    let plan = Plan {
        max_targets: 1,
        ..plan_for(&pool, first).await
    };
    reconcile_account(&pool, account, fixed(&plan), None)
        .await
        .expect("reconcile");
    let (held, _) = list_held(&pool, account).await.expect("held");
    assert!(
        held.iter().any(|h| h.id == secret),
        "the sibling org's monitor is what the pool gave up"
    );

    let app = common::with_session(
        common::build_saas_router_with_pg_targets(pool.clone()).await,
        delegate,
        Some(first),
        None,
    );
    // Owning one org is not owning the account. Reading would name a monitor in
    // an org this user is not a member of; writing would let them push the
    // holds onto it, since the pool is zero-sum.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/account/holds")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("list");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(resp).await["error"]["code"],
        "ACCOUNT_OWNER_REQUIRED"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/account/holds")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "keep_monitors": [secret] }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("put");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(second)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(delegate.0)
        .execute(&pool)
        .await;
    cleanup(&pool, first, owner).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_malformed_override_does_not_stop_the_sweep_for_everyone() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    // `plan_overrides` is hand-written operator JSON with no write path to
    // validate it, and the sweep reads every account in one statement. An
    // unguarded cast would abort the whole query, so one typo would freeze
    // holds and, worse, releases for the entire fleet.
    let (bad, bad_org, _, bad_user) = account_with_targets(&pool, "free", 1).await;
    for value in [
        serde_json::json!("unlimited"),
        serde_json::json!(99_999_999_999i64),
        serde_json::json!(true),
        serde_json::json!({"nested": 1}),
    ] {
        sqlx::query(
            "INSERT INTO plan_overrides (account_id, override_json, reason) \
             VALUES ($1, jsonb_build_object('max_targets', $2::jsonb), 'test') \
             ON CONFLICT (account_id) DO UPDATE SET override_json = EXCLUDED.override_json",
        )
        .bind(bad.0)
        .bind(value.to_string())
        .execute(&pool)
        .await
        .expect("write override");

        let due = accounts_needing_reconcile(&pool)
            .await
            .unwrap_or_else(|e| panic!("a {value} override must not abort the sweep: {e}"));
        assert!(
            !due.contains(&(bad, bad_org)),
            "a value that is not a number cannot lower a cap, so it is out of scope: {value}"
        );
    }

    // A genuinely lowering override is still picked up.
    sqlx::query(
        "UPDATE plan_overrides SET override_json = '{\"max_targets\": 0}' WHERE account_id = $1",
    )
    .bind(bad.0)
    .execute(&pool)
    .await
    .expect("lowering override");
    let due = accounts_needing_reconcile(&pool).await.expect("sweep");
    assert!(
        due.iter().any(|(a, _)| *a == bad),
        "an override below the plan's cap is exactly what the clause is for"
    );

    let _ = sqlx::query("DELETE FROM plan_overrides WHERE account_id = $1")
        .bind(bad.0)
        .execute(&pool)
        .await;
    cleanup(&pool, bad_org, bad_user).await;
}
