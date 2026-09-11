//! Moving an account between plans through the one path that may do it, and
//! the operator door in front of it. Live PG only; no-ops without
//! `DATABASE_URL`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{
    body_json, build_test_app_with_pg_store, default_http_check, make_user, pg_pool_from_env,
    unique_slug,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::billing::{Actor, PlanRequest, set_plan};
use uptimepage::config::AppConfig;
use uptimepage::domain::{AccountId, CheckSpec, ExpectedStatus, OrgId, UserId};
use uptimepage::quotas::{QuotaService, overrides};
use uuid::Uuid;

const OPERATOR_TOKEN: &str = "operator-secret-for-tests";

fn quotas(pool: &PgPool) -> QuotaService {
    let cfg = AppConfig::load().expect("config");
    QuotaService::new(&cfg, Some(pool.clone()))
}

fn operator(req: PlanRequest<'_>) -> PlanRequest<'_> {
    PlanRequest {
        actor: Actor::Operator,
        ..req
    }
}

fn to_plan(plan_id: &str) -> PlanRequest<'_> {
    operator(PlanRequest {
        plan_id,
        fallback_plan_id: None,
        reason: "test",
        actor: Actor::Operator,
    })
}

/// An account on `plan` with one org and `n` monitors, created oldest-first so
/// the hold order is deterministic.
async fn account_with_targets(
    pool: &PgPool,
    plan: &str,
    n: usize,
) -> (AccountId, OrgId, Vec<Uuid>, UserId) {
    let user = make_user(pool, "assign").await;
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
    .bind(unique_slug("asg"))
    .bind("Assigned")
    .bind(account)
    .fetch_one(pool)
    .await
    .expect("org");
    let mut ids = Vec::new();
    for i in 0..n {
        ids.push(add_target(pool, OrgId(org), &format!("m{i}"), i as i64).await);
    }
    (AccountId(account), OrgId(org), ids, user)
}

async fn add_target(pool: &PgPool, org: OrgId, name: &str, age: i64) -> Uuid {
    let spec = serde_json::to_value(CheckSpec::Http(default_http_check(
        "https://example.com".parse().expect("url"),
        ExpectedStatus::Exact(200),
    )))
    .expect("spec");
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

async fn held_count(pool: &PgPool, org: OrgId) -> i64 {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM targets WHERE org_id = $1 AND plan_hold_at IS NOT NULL",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .expect("held");
    n
}

async fn account_row(pool: &PgPool, account: AccountId) -> (String, Option<String>) {
    sqlx::query_as("SELECT plan_id, fallback_plan_id FROM accounts WHERE id = $1")
        .bind(account.0)
        .fetch_one(pool)
        .await
        .expect("account row")
}

async fn ledger(pool: &PgPool, account: AccountId) -> Vec<(String, Value)> {
    sqlx::query_as(
        "SELECT kind, payload FROM account_billing_events WHERE account_id = $1 \
         ORDER BY created_at, id",
    )
    .bind(account.0)
    .fetch_all(pool)
    .await
    .expect("ledger")
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
async fn a_smaller_plan_holds_the_excess_and_the_way_back_releases_it() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "founding", 25).await;
    let quotas = quotas(&pool);

    let down = set_plan(&pool, &quotas, account, to_plan("free"))
        .await
        .expect("downgrade");
    assert_eq!((down.from.as_str(), down.to.as_str()), ("founding", "free"));
    assert_eq!(down.reconciled.held, 5, "free covers 20 of the 25");
    assert_eq!(held_count(&pool, org).await, 5);
    assert_eq!(
        account_row(&pool, account).await,
        ("free".into(), Some("founding".into())),
        "the plan the account left first is where it falls back to"
    );

    let up = set_plan(&pool, &quotas, account, to_plan("founding"))
        .await
        .expect("upgrade");
    assert_eq!(up.reconciled.released, 5);
    assert_eq!(held_count(&pool, org).await, 0);

    let rows = ledger(&pool, account).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "plan_changed");
    assert_eq!(rows[0].1["from"], "founding");
    assert_eq!(rows[0].1["to"], "free");
    assert_eq!(rows[0].1["reason"], "test");
    assert_eq!(rows[0].1["actor"]["kind"], "operator");
    assert_eq!(rows[1].1["to"], "founding");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_same_plan_again_writes_nothing() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "founding", 1).await;
    let quotas = quotas(&pool);

    let same = set_plan(&pool, &quotas, account, to_plan("founding"))
        .await
        .expect("no-op");
    assert_eq!(same.from, same.to);
    assert!(!same.reconciled.changed());
    assert!(ledger(&pool, account).await.is_empty());
    assert_eq!(account_row(&pool, account).await, ("founding".into(), None));
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_unknown_plan_is_refused_before_anything_is_written() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "founding", 1).await;
    let quotas = quotas(&pool);

    let err = set_plan(&pool, &quotas, account, to_plan("platinum"))
        .await
        .expect_err("no such plan");
    assert!(err.to_string().contains("platinum"), "{err}");
    assert_eq!(account_row(&pool, account).await, ("founding".into(), None));
    assert!(ledger(&pool, account).await.is_empty());

    let missing = AccountId(Uuid::now_v7());
    set_plan(&pool, &quotas, missing, to_plan("free"))
        .await
        .expect_err("no such account");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_new_plan_applies_on_the_next_request_not_after_the_cache_ttl() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "founding", 1).await;
    let quotas = quotas(&pool);

    assert_eq!(
        quotas.limit_for_org(org).await.expect("warm").id,
        "founding"
    );
    set_plan(&pool, &quotas, account, to_plan("team"))
        .await
        .expect("upgrade");
    assert_eq!(
        quotas.limit_for_org(org).await.expect("fresh").id,
        "team",
        "the cached entry was dropped, not left to expire"
    );
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_fallback_survives_later_moves_unless_named() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "founding", 1).await;
    let quotas = quotas(&pool);

    set_plan(&pool, &quotas, account, to_plan("team"))
        .await
        .expect("founding -> team");
    set_plan(&pool, &quotas, account, to_plan("pro"))
        .await
        .expect("team -> pro");
    assert_eq!(
        account_row(&pool, account).await,
        ("pro".into(), Some("founding".into())),
        "a switch between paid plans keeps the original fallback"
    );

    let named = set_plan(
        &pool,
        &quotas,
        account,
        operator(PlanRequest {
            plan_id: "pro",
            fallback_plan_id: Some("free"),
            reason: "late signup, not founding",
            actor: Actor::Operator,
        }),
    )
    .await
    .expect("fallback only");
    assert_eq!(named.fallback.as_deref(), Some("free"));
    assert_eq!(
        account_row(&pool, account).await,
        ("pro".into(), Some("free".into()))
    );
    let rows = ledger(&pool, account).await;
    assert_eq!(rows.len(), 3, "naming the fallback alone is still a change");
    assert_eq!(rows[2].1["fallback"], "free");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_plan_change_never_waits_on_a_second_connection() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "founding", 25).await;

    // A plan lookup that took a second connection would wait on itself.
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&url)
        .await
        .expect("single-connection pool");
    let quotas = quotas(&single);

    let down = set_plan(&single, &quotas, account, to_plan("free"))
        .await
        .expect("a cold cache must load on the transaction's own connection");
    assert_eq!(down.reconciled.held, 5);
    assert_eq!(quotas.limit_for_org(org).await.expect("cached").id, "free");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_lookup_on_a_held_connection_never_joins_one_queued_on_the_pool() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (_, org, _, user) = account_with_targets(&pool, "founding", 1).await;
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let single = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&url)
        .await
        .expect("single-connection pool");
    let quotas = quotas(&single);

    // A pooled lookup for the same org queues behind the held connection
    // before the direct lookup begins.
    let mut held = single.acquire().await.expect("hold the only connection");
    let queued = {
        let quotas = quotas.clone();
        tokio::spawn(async move { quotas.limit_for_org(org).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let direct = quotas
        .limit_for_org_on(&mut held, org)
        .await
        .expect("must load on the held connection rather than wait with the queued lookup");
    assert_eq!(direct.id, "founding");
    drop(held);
    let queued = queued
        .await
        .expect("join")
        .expect("the queued lookup completes once freed");
    assert_eq!(queued.id, "founding");
    cleanup(&pool, org, user).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn clearing_an_override_that_is_already_gone_still_reconciles() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (account, org, _, user) = account_with_targets(&pool, "founding", 5).await;
    let quotas = quotas(&pool);
    let lowered = json!({ "max_targets": 3 });
    let set = overrides::set(&pool, &quotas, account, &lowered, "shrink", None)
        .await
        .expect("set");
    assert_eq!(set.held, 2);

    // As if the delete committed and the reconcile after it never ran.
    sqlx::query("DELETE FROM plan_overrides WHERE account_id = $1")
        .bind(account.0)
        .execute(&pool)
        .await
        .expect("delete by hand");
    assert_eq!(
        held_count(&pool, org).await,
        2,
        "still held from the override"
    );

    let cleared = overrides::clear(&pool, &quotas, account)
        .await
        .expect("clear");
    assert_eq!(
        cleared.released, 2,
        "a retry converges rather than reporting nothing to do"
    );
    assert_eq!(held_count(&pool, org).await, 0);
    assert_eq!(
        ledger(&pool, account).await.len(),
        1,
        "only the set was recorded; the retry found nothing to remove"
    );
    cleanup(&pool, org, user).await;
}

async fn operator_call(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let req = match body {
        Some(body) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let body = if status == StatusCode::NO_CONTENT {
        Value::Null
    } else {
        body_json(resp).await
    };
    (status, body)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_operator_door_needs_the_admin_token() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = build_test_app_with_pg_store(pool.clone(), |cfg| {
        cfg.operator.admin_token = secrecy::SecretString::from(OPERATOR_TOKEN.to_string());
    })
    .await;
    let account = uptimepage::storage::accounts::account_for_org(&pool, org)
        .await
        .expect("account");
    let path = format!("/operator/accounts/{}/plan", account.0);
    let body = json!({ "plan_id": "founding", "reason": "test" });

    let (status, _) = operator_call(&app, "PUT", &path, None, Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = operator_call(&app, "PUT", &path, Some("wrong"), Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(account_row(&pool, account).await.0, "free");

    let (disabled, _) = build_test_app_with_pg_store(pool.clone(), |_| {}).await;
    let (status, body) =
        operator_call(&disabled, "PUT", &path, Some(OPERATOR_TOKEN), Some(body)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "OPERATOR_DISABLED");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_operator_moves_the_plan_and_shapes_the_caps() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, org) = build_test_app_with_pg_store(pool.clone(), |cfg| {
        cfg.operator.admin_token = secrecy::SecretString::from(OPERATOR_TOKEN.to_string());
    })
    .await;
    let account = uptimepage::storage::accounts::account_for_org(&pool, org)
        .await
        .expect("account");
    for i in 0..25 {
        add_target(&pool, org, &format!("op{i}"), i).await;
    }
    let plan_path = format!("/operator/accounts/{}/plan", account.0);
    let overrides_path = format!("/operator/accounts/{}/overrides", account.0);
    let auth = Some(OPERATOR_TOKEN);

    let (status, body) = operator_call(
        &app,
        "PUT",
        &plan_path,
        auth,
        Some(json!({ "plan_id": "founding", "reason": "grant" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["plan_id"], "founding");
    assert_eq!(body["previous_plan_id"], "free");
    assert_eq!(body["fallback_plan_id"], "free");
    assert_eq!(body["held"], 0, "founding covers all 25");

    let (status, body) = operator_call(
        &app,
        "PUT",
        &plan_path,
        auth,
        Some(json!({ "plan_id": "platinum", "reason": "typo" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "PLAN_NOT_FOUND");

    let (status, body) = operator_call(
        &app,
        "PUT",
        &plan_path,
        auth,
        Some(json!({ "plan_id": "team", "reason": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "REASON_INVALID");

    let (status, body) = operator_call(
        &app,
        "PUT",
        &overrides_path,
        auth,
        Some(json!({ "caps": { "max_target": 22 }, "reason": "typo" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "PLAN_OVERRIDE_INVALID");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_target"),
        "names the offending key: {body}"
    );

    for bad in [
        json!("unlimited"),
        json!(-1),
        json!(1.5),
        json!(3_000_000_000u64),
    ] {
        let (status, body) = operator_call(
            &app,
            "PUT",
            &overrides_path,
            auth,
            Some(json!({ "caps": { "max_orgs": bad }, "reason": "bad value" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(body["error"]["code"], "PLAN_OVERRIDE_INVALID", "{bad}");
    }

    let (status, body) = operator_call(
        &app,
        "PUT",
        &overrides_path,
        auth,
        Some(json!({ "caps": { "max_targets": 22 }, "reason": "lower it" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["held"], 3,
        "an override can lower a cap, and the excess is held at once"
    );
    assert_eq!(held_count(&pool, org).await, 3);

    let (status, body) = operator_call(&app, "DELETE", &overrides_path, auth, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["released"], 3);
    assert_eq!(held_count(&pool, org).await, 0);

    let (status, body) = operator_call(&app, "DELETE", &overrides_path, auth, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["released"], 0, "clearing twice is a no-op");

    let kinds: Vec<String> = ledger(&pool, account)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        ["plan_changed", "overrides_set", "overrides_cleared"]
    );

    let (status, body) = operator_call(
        &app,
        "PUT",
        &format!("/operator/accounts/{}/overrides", Uuid::now_v7()),
        auth,
        Some(json!({ "caps": { "max_targets": 1 }, "reason": "nobody" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "ACCOUNT_NOT_FOUND");
}
