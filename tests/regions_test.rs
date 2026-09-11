//! Live-PG tests for the region-scoped target sources that drive the agent
//! config-pull API and the dashboard's home-region scheduler filter.

mod common;

use sqlx::PgPool;
use uptimepage::storage::AdminRepo;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

fn check_spec() -> serde_json::Value {
    serde_json::json!({
        "type": "http",
        "url": "https://example.com/",
        "method": "GET",
        "timeout": 5000,
        "follow_redirects": false,
        "max_redirects": 0,
        "expected_status": {"kind": "exact", "value": 200},
        "headers": {},
        "verify_tls": true,
    })
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let slug = format!("rg{}", &Uuid::new_v4().simple().to_string()[..12]);
    let (org_id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 's', a.id FROM a RETURNING id",
    )
    .bind(&slug)
    .fetch_one(pool)
    .await
    .unwrap();
    org_id
}

async fn seed_target(pool: &PgPool, org_id: Uuid) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 't', $2::jsonb, 60, true) RETURNING id",
    )
    .bind(org_id)
    .bind(check_spec())
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

async fn ensure_region(pool: &PgPool, id: &str) {
    sqlx::query("INSERT INTO regions (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn assign(pool: &PgPool, target_id: Uuid, region: &str) {
    sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
        .bind(target_id)
        .bind(region)
        .execute(pool)
        .await
        .unwrap();
}

async fn insert_agent(pool: &PgPool, region: &str, name: &str, enabled: bool, flow_capable: bool) {
    sqlx::query(
        "INSERT INTO agents (region, name, token_hash, token_prefix, enabled, flow_capable) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(region)
    .bind(name)
    .bind(format!("h-{name}"))
    .bind(format!("p-{name}"))
    .bind(enabled)
    .bind(flow_capable)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore]
async fn reconcile_upserts_region_and_backfills_unassigned() {
    // Own DB: reconcile_regions backfills every unassigned target across the
    // whole database, which races other suites' targets on the shared pool.
    // Isolation keeps that global mutation contained.
    let Some((db_url, db_name)) = common::fresh_test_db("regions_reconcile").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let region = "eu-reconcile";
    let org = seed_org(&pool).await;
    // Seeded directly (not via the store) so it carries no region assignment.
    let orphan = seed_target(&pool, org).await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    // Region row does not exist yet — reconcile must create it, then backfill.
    repo.reconcile_regions(region, region).await.unwrap();

    const COUNT_SQL: &str =
        "SELECT count(*) FROM target_regions WHERE target_id = $1 AND region = $2";
    let assigned: i64 = sqlx::query_scalar(COUNT_SQL)
        .bind(orphan)
        .bind(region)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        assigned, 1,
        "reconcile must assign an unassigned target to the default region"
    );

    // Idempotent: a second run neither errors nor duplicates the assignment.
    repo.reconcile_regions(region, region).await.unwrap();
    let assigned: i64 = sqlx::query_scalar(COUNT_SQL)
        .bind(orphan)
        .bind(region)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(assigned, 1, "reconcile must not duplicate assignments");

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

#[tokio::test]
#[ignore]
async fn region_source_returns_only_that_region() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    ensure_region(&pool, "eu-rgtest2").await;
    ensure_region(&pool, "eu-rgother").await;
    let org = seed_org(&pool).await;
    let eu = seed_target(&pool, org).await;
    let other = seed_target(&pool, org).await;
    assign(&pool, eu, "eu-rgtest2").await;
    assign(&pool, other, "eu-rgother").await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let ids: Vec<Uuid> = repo
        .list_enabled_targets_for_region("eu-rgtest2", true, &Default::default())
        .await
        .unwrap()
        .into_iter()
        .map(|(_, t)| t.id)
        .collect();

    assert!(ids.contains(&eu));
    assert!(
        !ids.contains(&other),
        "another region's target must not leak into the eu pull"
    );
}

#[tokio::test]
#[ignore]
async fn pull_etag_reflects_membership_swap_at_equal_count() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    ensure_region(&pool, "eu-rgetag").await;
    let org = seed_org(&pool).await;
    let a = seed_target(&pool, org).await;
    let b = seed_target(&pool, org).await;
    assign(&pool, a, "eu-rgetag").await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let e1 = repo
        .region_pull_etag("eu-rgetag", &Default::default(), "")
        .await
        .unwrap();
    let e1b = repo
        .region_pull_etag("eu-rgetag", &Default::default(), "")
        .await
        .unwrap();
    assert_eq!(e1, e1b, "etag stable when nothing changes");

    // Swap membership keeping the count at 1.
    sqlx::query("DELETE FROM target_regions WHERE target_id = $1 AND region = $2")
        .bind(a)
        .bind("eu-rgetag")
        .execute(&pool)
        .await
        .unwrap();
    assign(&pool, b, "eu-rgetag").await;
    let e2 = repo
        .region_pull_etag("eu-rgetag", &Default::default(), "")
        .await
        .unwrap();
    assert_ne!(e1, e2, "etag must change on a same-count membership swap");
}

#[tokio::test]
#[ignore]
async fn assigned_targets_map_carries_authoritative_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    ensure_region(&pool, "eu-rgtest3").await;
    let org = seed_org(&pool).await;
    let t = seed_target(&pool, org).await;
    assign(&pool, t, "eu-rgtest3").await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let map = repo
        .assigned_targets_for_region("eu-rgtest3")
        .await
        .unwrap();

    assert_eq!(map.get(&t).map(|o| o.0), Some(org));
}

#[tokio::test]
#[ignore]
async fn set_and_read_target_regions_honours_enabled_and_org() {
    use uptimepage::domain::OrgId;
    use uptimepage::storage::{PostgresTargetStore, TargetStore};

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    ensure_region(&pool, "eu-assign").await;
    ensure_region(&pool, "us-assign").await;
    sqlx::query("UPDATE regions SET enabled = false WHERE id = 'us-assign'")
        .execute(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    let target = seed_target(&pool, org).await;
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    // available_regions excludes a disabled region.
    let avail = store.available_regions().await.unwrap();
    assert!(avail.contains(&"eu-assign".to_string()));
    assert!(!avail.contains(&"us-assign".to_string()));

    // Assignment replaces the set and reads back.
    assert!(
        store
            .set_target_regions(OrgId(org), target, &["eu-assign".to_string()])
            .await
            .unwrap()
    );
    assert_eq!(
        store.regions_for_target(OrgId(org), target).await.unwrap(),
        Some(vec!["eu-assign".to_string()])
    );

    // Cross-org: a foreign org reading this target sees not-found, not data.
    let other = seed_org(&pool).await;
    assert_eq!(
        store
            .regions_for_target(OrgId(other), target)
            .await
            .unwrap(),
        None
    );
    assert!(
        !store
            .set_target_regions(OrgId(other), target, &["eu-assign".to_string()])
            .await
            .unwrap()
    );
}

#[tokio::test]
#[ignore]
async fn available_regions_detailed_carries_labels_and_excludes_disabled() {
    use uptimepage::storage::{PostgresTargetStore, TargetStore};

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO regions (id, name, city, enabled) \
         VALUES ('eu-detail', 'EU Detail', 'Helsinki', true) \
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, \
         city = excluded.city, enabled = true",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO regions (id, name, city, enabled) \
         VALUES ('us-detail-off', 'US Off', 'Ashburn', false) \
         ON CONFLICT (id) DO UPDATE SET enabled = false",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    let detailed = store.available_regions_detailed().await.unwrap();
    let eu = detailed
        .iter()
        .find(|r| r.id == "eu-detail")
        .expect("enabled region present in the catalog");
    assert_eq!(eu.name, "EU Detail");
    assert_eq!(eu.city, "Helsinki");
    assert!(
        !detailed.iter().any(|r| r.id == "us-detail-off"),
        "a disabled region must not appear in the catalog"
    );
}

#[tokio::test]
#[ignore]
async fn flow_capable_regions_lists_only_enabled_capable_agents() {
    use uptimepage::storage::{PostgresTargetStore, TargetStore};

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    ensure_region(&pool, "eu-flowcap").await;
    ensure_region(&pool, "us-flowcap").await;
    ensure_region(&pool, "ap-flowcap").await;
    insert_agent(&pool, "eu-flowcap", "cap-eu", true, true).await;
    insert_agent(&pool, "us-flowcap", "cap-us-off", false, true).await;
    insert_agent(&pool, "ap-flowcap", "cap-ap-plain", true, false).await;

    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let caps = store.flow_capable_regions().await.unwrap();

    assert!(
        caps.contains(&"eu-flowcap".to_string()),
        "enabled + capable"
    );
    assert!(
        !caps.contains(&"us-flowcap".to_string()),
        "a disabled agent's region is not capable"
    );
    assert!(
        !caps.contains(&"ap-flowcap".to_string()),
        "a non-flow agent's region is not capable"
    );
}

fn target_body(name: &str, regions: Option<&[&str]>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "name": name,
        "check": check_spec(),
        "interval": 300,
    });
    if let Some(regions) = regions {
        body["regions"] = serde_json::json!(regions);
    }
    body
}

async fn post_json(
    router: &axum::Router,
    path: &str,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let req = axum::http::Request::post(path)
        .header("content-type", "application/json")
        .header("X-Requested-With", "uptimepage")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn assigned_regions(router: &axum::Router, id: &str) -> Vec<String> {
    use tower::ServiceExt;
    let req = axum::http::Request::get(format!("/api/v1/targets/{id}/regions"))
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    serde_json::from_value(v["regions"].clone()).unwrap()
}

#[tokio::test]
#[ignore]
async fn create_assigns_the_regions_the_body_names() {
    use uptimepage::storage::create_org_with_owner;

    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    ensure_region(&pool, "eu-pin").await;
    ensure_region(&pool, "us-pin").await;
    let user = common::make_user(&pool, "pin").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("pin"), "Pin")
        .await
        .unwrap()
        .expect("org")
        .id;
    let router = common::with_session(
        common::build_saas_router_with_pg_targets(pool.clone()).await,
        user,
        Some(org),
        Some("pin-test"),
    );

    let (status, v) = post_json(
        &router,
        "/api/v1/targets",
        target_body("pinned", Some(&["eu-pin"])),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap();
    assert_eq!(
        assigned_regions(&router, id).await,
        vec!["eu-pin".to_string()]
    );
    let (status, v) = post_json(&router, "/api/v1/targets", target_body("plain", None)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{v}");
    let server_default = assigned_regions(&router, v["id"].as_str().unwrap()).await;
    assert!(!server_default.is_empty());

    let (status, v) = post_json(
        &router,
        "/api/v1/targets/bulk",
        serde_json::json!([
            target_body("bulk-default", None),
            target_body("bulk-pinned", Some(&["us-pin"])),
        ]),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{v}");
    let items = v.as_array().unwrap();
    let by_name = |name: &str| {
        items
            .iter()
            .find(|t| t["name"] == name)
            .and_then(|t| t["id"].as_str())
            .unwrap()
            .to_string()
    };
    let pinned = assigned_regions(&router, &by_name("bulk-pinned")).await;
    assert_eq!(pinned, vec!["us-pin".to_string()]);
    let default = assigned_regions(&router, &by_name("bulk-default")).await;
    assert_eq!(
        default, server_default,
        "an item naming no regions gets what a single create gets"
    );
}
