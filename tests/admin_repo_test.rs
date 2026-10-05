//! Live-PG tests for `AdminRepo` — the cross-tenant lane the scheduler uses
//! to materialise its global registry. The load-bearing invariant is that
//! soft-deleted orgs disappear from this list immediately: their targets
//! must stop emitting check_results, otherwise 30 days of grace-window
//! data accumulate in ClickHouse and (worse) Slack/webhook alerts keep
//! pinging a customer who's already cancelled.

use crate::common;

use sqlx::PgPool;
use uptimepage::storage::AdminRepo;
use uptimepage::storage::admin::PublicTargetCursor;
use uuid::Uuid;

// Own DB per test: AdminRepo reads + decodes every enabled target across all
// orgs, so a foreign row with an invalid check_spec from another suite on the
// shared pool would fail the whole call. Isolation keeps the cross-org read clean.
use crate::common::MIGRATOR;

fn http_check_spec() -> serde_json::Value {
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

fn flow_check_spec() -> serde_json::Value {
    serde_json::json!({
        "type": "flow",
        "start_url": "https://example.com/login",
        "steps": [{"op": "assert_url", "contains": "/x"}],
        "timeout": 30000,
        "step_timeout": 10000,
        "verify_tls": true,
    })
}

async fn seed_org_with_target(pool: &PgPool, slug: &str) -> (Uuid, Uuid) {
    let (org_id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 's', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap();
    let (target_id,): (Uuid,) = sqlx::query_as(
        r#"INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled)
           VALUES ($1, 't', $2::jsonb, 60, true)
           RETURNING id"#,
    )
    .bind(org_id)
    .bind(http_check_spec())
    .fetch_one(pool)
    .await
    .unwrap();
    (org_id, target_id)
}

async fn seed_public_target(pool: &PgPool, org_id: Uuid) -> Uuid {
    let (target_id,): (Uuid,) = sqlx::query_as(
        r#"INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled)
           VALUES ($1, 't-pub', $2::jsonb, 60, true)
           RETURNING id"#,
    )
    .bind(org_id)
    .bind(http_check_spec())
    .fetch_one(pool)
    .await
    .unwrap();
    bind_to_new_page(pool, org_id, target_id).await;
    target_id
}

/// Insert a target with an explicit id, so tests can pin keyset ordering
/// deterministically (uuidv7 ids minted in the same millisecond don't have a
/// guaranteed order — the tail is random).
async fn insert_target_with_id(pool: &PgPool, org_id: Uuid, id: Uuid, spec: serde_json::Value) {
    sqlx::query(
        "INSERT INTO targets (id, org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, $2, 't', $3::jsonb, 60, true)",
    )
    .bind(id)
    .bind(org_id)
    .bind(spec)
    .execute(pool)
    .await
    .unwrap();
}

/// Put `target_id` on a brand-new enabled page for `org_id`. Publicness is a
/// `status_page_components` binding now, not a `targets` column; binding a
/// target to several pages exercises the writer-walk's `DISTINCT ON`.
async fn bind_to_new_page(pool: &PgPool, org_id: Uuid, target_id: Uuid) {
    let slug = format!("admpg{}", &Uuid::new_v4().simple().to_string()[..16]);
    let (page_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled) \
         VALUES ($1, $2, 'p', true) RETURNING id",
    )
    .bind(org_id)
    .bind(&slug)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO status_page_components (org_id, status_page_id, target_id) \
         VALUES ($1, $2, $3)",
    )
    .bind(org_id)
    .bind(page_id)
    .bind(target_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_org(pool: &PgPool, slug: &str) -> Uuid {
    let (org_id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 's', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(pool)
    .await
    .unwrap();
    org_id
}

/// Valid HTTP target in an existing org (no page binding), returning its id.
async fn seed_good_target(pool: &PgPool, org_id: Uuid) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 'g', $2::jsonb, 60, true) RETURNING id",
    )
    .bind(org_id)
    .bind(http_check_spec())
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

/// Enabled target whose `{}` check_spec has no `type` tag — the decoder rejects it.
async fn seed_bad_target(pool: &PgPool, org_id: Uuid) -> Uuid {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 'bad', '{}'::jsonb, 60, true) RETURNING id",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .unwrap();
    id
}

/// An empty enabled set must be `Ok(empty)`, never an error — the all-failed
/// guard keys on `total > 0`, so genuinely-zero targets stays a clean empty.
#[tokio::test]
#[ignore]
async fn empty_enabled_set_is_ok_not_error() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let repo = AdminRepo::new(pool.clone(), None, "test");
    assert!(
        repo.list_all_enabled_targets().await.unwrap().is_empty(),
        "no targets → Ok(empty), not Err"
    );
    assert!(
        repo.next_enabled_target_page(None, 10)
            .await
            .unwrap()
            .is_empty(),
        "empty walk returns empty, not Err"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// The region pull list skips an undecodable row and returns the rest.
#[tokio::test]
#[ignore]
async fn region_list_skips_undecodable_row() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let region = "eu-adm";
    sqlx::query("INSERT INTO regions (id, name) VALUES ($1, $1)")
        .bind(region)
        .execute(&pool)
        .await
        .unwrap();
    let org = seed_org(
        &pool,
        &format!("admrg-{}", &Uuid::new_v4().simple().to_string()[..8]),
    )
    .await;
    let good = seed_good_target(&pool, org).await;
    let bad = seed_bad_target(&pool, org).await;
    for t in [good, bad] {
        sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
            .bind(t)
            .bind(region)
            .execute(&pool)
            .await
            .unwrap();
    }

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let ids: Vec<Uuid> = repo
        .list_enabled_targets_for_region(region, true, &Default::default())
        .await
        .unwrap()
        .into_iter()
        .map(|(_, t)| t.id)
        .collect();
    assert!(ids.contains(&good));
    assert!(
        !ids.contains(&bad),
        "undecodable row skipped in region list"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// A flow monitor is withheld from a node that can't run it (`include_flow =
/// false`) and served to one that can (`true`). Non-flow kinds are unaffected.
#[tokio::test]
#[ignore]
async fn region_list_filters_flow_by_capability() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let region = "eu-flowcap";
    sqlx::query("INSERT INTO regions (id, name, city, enabled) VALUES ($1, $1, '', true)")
        .bind(region)
        .execute(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool, "flow-cap").await;
    let http = seed_good_target(&pool, org).await;
    let (flow,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 'f', $2::jsonb, 300, true) RETURNING id",
    )
    .bind(org)
    .bind(flow_check_spec())
    .fetch_one(&pool)
    .await
    .unwrap();
    for t in [http, flow] {
        sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
            .bind(t)
            .bind(region)
            .execute(&pool)
            .await
            .unwrap();
    }

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let without: Vec<Uuid> = repo
        .list_enabled_targets_for_region(region, false, &Default::default())
        .await
        .unwrap()
        .into_iter()
        .map(|(_, t)| t.id)
        .collect();
    assert!(without.contains(&http), "http always served");
    assert!(
        !without.contains(&flow),
        "flow withheld from a non-capable node"
    );

    let with: Vec<Uuid> = repo
        .list_enabled_targets_for_region(region, true, &Default::default())
        .await
        .unwrap()
        .into_iter()
        .map(|(_, t)| t.id)
        .collect();
    assert!(with.contains(&flow), "flow served to a capable node");
    assert!(with.contains(&http), "http still served alongside flow");

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// Regression: a bad row sorting LAST in the keyset must not abort the walk.
/// (uuidv7 ids are time-ordered, so the last-inserted target sorts last.) The
/// paginator advances past the trailing all-bad page instead of erroring.
#[tokio::test]
#[ignore]
async fn paginator_skips_trailing_undecodable_and_completes() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let org = seed_org(
        &pool,
        &format!("admpg-{}", &Uuid::new_v4().simple().to_string()[..8]),
    )
    .await;
    // Pinned ids so `bad` deterministically sorts LAST in the keyset.
    let g1 = Uuid::from_u128(1);
    let g2 = Uuid::from_u128(2);
    let bad = Uuid::from_u128(3);
    insert_target_with_id(&pool, org, g1, http_check_spec()).await;
    insert_target_with_id(&pool, org, g2, http_check_spec()).await;
    insert_target_with_id(&pool, org, bad, serde_json::json!({})).await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let mut seen: Vec<Uuid> = Vec::new();
    let mut cursor: Option<PublicTargetCursor> = None;
    let mut steps = 0;
    loop {
        steps += 1;
        assert!(steps <= 50, "walk must terminate");
        let page = repo
            .next_enabled_target_page(cursor, 1)
            .await
            .expect("walk must not error on a trailing undecodable row");
        let Some((org_id, last)) = page.last().map(|(o, t)| (*o, t.id)) else {
            break;
        };
        seen.extend(page.iter().map(|(_, t)| t.id));
        cursor = Some(PublicTargetCursor::after(org_id, last));
    }
    assert!(
        seen.contains(&g1) && seen.contains(&g2),
        "both good targets walked"
    );
    assert!(!seen.contains(&bad), "trailing bad row skipped");

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// A page that decodes to nothing in the MIDDLE of the walk (a bad row sorting
/// before a good one, page_size=1) must be skipped-and-advanced, not read as
/// end-of-walk.
#[tokio::test]
#[ignore]
async fn paginator_advances_past_all_bad_page() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let org = seed_org(
        &pool,
        &format!("admpg2-{}", &Uuid::new_v4().simple().to_string()[..8]),
    )
    .await;
    // Pinned ids so `bad` deterministically sorts FIRST — it fills the first
    // page_size=1 page, forcing the skip-and-advance path.
    let bad = Uuid::from_u128(1);
    let good = Uuid::from_u128(2);
    insert_target_with_id(&pool, org, bad, serde_json::json!({})).await;
    insert_target_with_id(&pool, org, good, http_check_spec()).await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    // page_size=1: first SQL row is the bad one → internal loop skips it and
    // returns the good row, not an empty (walk-complete) page.
    let page = repo.next_enabled_target_page(None, 1).await.unwrap();
    assert_eq!(
        page.len(),
        1,
        "skipped the leading bad page, returned the good row"
    );
    assert_eq!(page[0].1.id, good);
    assert_ne!(page[0].1.id, bad);

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

#[tokio::test]
#[ignore]
async fn enabled_targets_excludes_soft_deleted_orgs() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();
    let slug = format!("admrep-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let (org_id, target_id) = seed_org_with_target(&pool, &slug).await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let before = repo.list_all_enabled_targets().await.unwrap();
    assert!(
        before.iter().any(|(_, t)| t.id == target_id),
        "live org's target must be visible to the scheduler"
    );

    // Soft-delete the org. Per the SaaS contract, the scheduler must stop
    // seeing this org's targets *immediately* — no grace-window write
    // traffic, no post-cancellation alerts.
    sqlx::query("UPDATE organizations SET deleted_at = now() WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let after = repo.list_all_enabled_targets().await.unwrap();
    assert!(
        !after.iter().any(|(_, t)| t.id == target_id),
        "soft-deleted org's target must NOT remain in the scheduler's enabled set"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// Keyset paginator must:
///   * include every enabled target — public and private alike
///   * skip targets whose org is soft-deleted (load-bearing — same reason
///     as the scheduler walk above)
///   * never re-emit a row from a previous page
///   * order monotonically by `(org_id, id)` so the cursor is correct
#[tokio::test]
#[ignore]
async fn enabled_target_pagination_includes_all_and_skips_dead_org() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    let slug_live = format!("pubpg-{suffix}-l");
    let slug_dead = format!("pubpg-{suffix}-d");

    // Live org with one public + one private target.
    let (org_live, private_target) = seed_org_with_target(&pool, &slug_live).await;
    let public_target = seed_public_target(&pool, org_live).await;
    // Dead org with a public target — must be skipped.
    let (org_dead, _) = seed_org_with_target(&pool, &slug_dead).await;
    let dead_public = seed_public_target(&pool, org_dead).await;
    sqlx::query("UPDATE organizations SET deleted_at = now() WHERE id = $1")
        .bind(org_dead)
        .execute(&pool)
        .await
        .unwrap();

    let repo = AdminRepo::new(pool.clone(), None, "test");

    // Single-page walk: both of the live org's targets survive (public and
    // private); the dead org's target is filtered out.
    let page = repo.next_enabled_target_page(None, 1024).await.unwrap();
    let mut ours: Vec<Uuid> = page
        .iter()
        .filter(|(_, t)| t.id == public_target || t.id == private_target || t.id == dead_public)
        .map(|(_, t)| t.id)
        .collect();
    ours.sort();
    let mut want = vec![public_target, private_target];
    want.sort();
    assert_eq!(ours, want, "both live targets visible, dead org filtered");

    // Multi-page walk with page_size=1 must still terminate without
    // re-emitting `public_target`.
    let mut seen: Vec<Uuid> = Vec::new();
    let mut cursor: Option<PublicTargetCursor> = None;
    let stop_after = page.len() + 4;
    let mut steps = 0;
    loop {
        steps += 1;
        assert!(steps <= stop_after, "pagination must terminate");
        let p = repo.next_enabled_target_page(cursor, 1).await.unwrap();
        if p.is_empty() {
            break;
        }
        let (org, t) = p.into_iter().next().unwrap();
        seen.push(t.id);
        cursor = Some(PublicTargetCursor {
            org_id: org,
            target_id: t.id,
        });
    }
    assert!(
        seen.contains(&public_target),
        "paginated walk must include the live public target"
    );
    let dupes = seen.iter().filter(|id| **id == public_target).count();
    assert_eq!(dupes, 1, "cursor must not re-emit the same row");
    assert!(
        !seen.contains(&dead_public),
        "soft-deleted org's target must stay hidden in the paginated walk"
    );
    assert!(
        seen.contains(&private_target),
        "private target now opens incidents too — must appear in the walk"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// A target bound to several enabled pages is still one `(org, target)` row in
/// the writer walk — the walk reads each `targets` row directly, so page
/// membership never fans a monitor into duplicate incident work.
#[tokio::test]
#[ignore]
async fn target_on_multiple_pages_emitted_once() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    let (org, _private) = seed_org_with_target(&pool, &format!("multi-{suffix}")).await;
    // seed_public_target binds onto one page; add a second enabled page for it.
    let target = seed_public_target(&pool, org).await;
    bind_to_new_page(&pool, org, target).await;

    let repo = AdminRepo::new(pool.clone(), None, "test");
    let mut seen: Vec<Uuid> = Vec::new();
    let mut cursor: Option<PublicTargetCursor> = None;
    let mut steps = 0;
    loop {
        steps += 1;
        assert!(steps <= 1000, "pagination must terminate");
        let p = repo.next_enabled_target_page(cursor, 1).await.unwrap();
        if p.is_empty() {
            break;
        }
        let (o, t) = p.into_iter().next().unwrap();
        seen.push(t.id);
        cursor = Some(PublicTargetCursor {
            org_id: o,
            target_id: t.id,
        });
    }
    assert_eq!(
        seen.iter().filter(|id| **id == target).count(),
        1,
        "target on two enabled pages must appear exactly once in the walk"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// One target with an unparseable `check_spec` must not sink the whole
/// cross-tenant load — the scheduler skips it and still returns every other
/// target, so a single bad row can't blind the fleet.
#[tokio::test]
#[ignore]
async fn undecodable_target_is_skipped_not_fatal() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let slug = format!("admlen-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let (org_id, good) = seed_org_with_target(&pool, &slug).await;
    // Empty check_spec → no `type` tag → the decoder rejects this row.
    let (bad,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 'bad', '{}'::jsonb, 60, true) RETURNING id",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let repo = AdminRepo::new(pool.clone(), None, "test");
    // The call succeeds despite the bad row (no Err), returning only the good one.
    let ids: Vec<Uuid> = repo
        .list_all_enabled_targets()
        .await
        .unwrap()
        .into_iter()
        .map(|(_, t)| t.id)
        .collect();
    assert!(ids.contains(&good), "the decodable target must still load");
    assert!(
        !ids.contains(&bad),
        "the undecodable target is skipped, not returned"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// When *every* enabled target fails to decode (a systemic cipher/schema fault,
/// not one bad row), the load must error — not return an empty list that the
/// scheduler/incident-writer would read as "no targets" and silently go dark.
#[tokio::test]
#[ignore]
async fn all_targets_undecodable_errors_not_empty() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_repo").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let slug = format!("admsys-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let (org_id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 's', a.id FROM a RETURNING id",
    )
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 'bad', '{}'::jsonb, 60, true)",
    )
    .bind(org_id)
    .execute(&pool)
    .await
    .unwrap();

    let repo = AdminRepo::new(pool.clone(), None, "test");
    assert!(
        repo.list_all_enabled_targets().await.is_err(),
        "all-undecodable must fail loud, not return an empty list"
    );

    pool.close().await;
    common::drop_test_db(&db_name).await;
}
