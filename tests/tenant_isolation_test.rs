//! Storage-layer tenant-isolation suite (acceptance #6). Provisions two orgs
//! with distinct owners, fills each with the same kinds of data, then asserts
//! every per-org store backed by Postgres or ClickHouse only sees its own
//! org's rows.
//!
//! Live-PG + live-CH. Ignored at the `cargo test` default; runs under
//! `--include-ignored` once `DATABASE_URL` and `CLICKHOUSE_URL` are set.

mod common;

use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use uptimepage::domain::{
    CheckResult, CheckSpec, CheckStatus, ExpectedStatus, NewMaintenanceWindow, NewStatusPage,
    NewStatusPageComponent, NewTarget, OrgId, UserId, WriteSource,
};
use uptimepage::storage::traits::{ClampedRange, TimeRange};
use uptimepage::storage::{
    ClickhouseResultSink, ClickhouseResultsStore, MaintenanceStore, PgMaintenanceStore,
    PgStatusPageStore, PostgresTargetStore, ResultSink, ResultsStore, StatusPageStore,
    TargetFilter, TargetStore, create_org_with_owner, is_active_member,
};
use url::Url;
use uuid::Uuid;

use crate::common::{default_http_check, make_user, unique_slug};

fn target_named(name: &str) -> NewTarget {
    let url = Url::parse("https://example.com/").unwrap();
    NewTarget {
        name: name.into(),
        check: CheckSpec::Http(default_http_check(url, ExpectedStatus::Exact(200))),
        interval: Duration::from_secs(30),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        regions: None,
    }
}

fn ok_result(target_id: Uuid, org_id: Uuid) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: Utc::now(),
        status: CheckStatus::Up,
        duration_ms: 42,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: None,
        diagnostic: None,
        error: None,
    }
}

fn time_range_around_now() -> TimeRange {
    let now = Utc::now();
    TimeRange {
        from: now - chrono::Duration::minutes(5),
        to: now + chrono::Duration::minutes(5),
    }
}

struct Tenant {
    user: UserId,
    org: OrgId,
}

async fn provision_tenant(pool: &PgPool, label: &str) -> Tenant {
    let user = make_user(pool, "isol").await;
    let org = create_org_with_owner(pool, user, &unique_slug(label), label)
        .await
        .unwrap()
        .expect("provision org");
    Tenant { user, org: org.id }
}

async fn teardown(pool: &PgPool, ch: &clickhouse::Client, t: &Tenant) {
    // ALTER TABLE ... DELETE on ClickHouse is async server-side but
    // idempotent. Issue it before the PG cascade so the rows are scheduled
    // for removal even if the test only lives in CH for a few minutes.
    let _ = ch
        .query("ALTER TABLE check_results DELETE WHERE org_id = ?")
        .bind(t.org.0)
        .execute()
        .await;
    let _ = ch
        .query("ALTER TABLE check_results_1m DELETE WHERE org_id = ?")
        .bind(t.org.0)
        .execute()
        .await;

    // ON DELETE CASCADE on tenant tables wipes targets, maintenance,
    // memberships, audit rows. The user row goes last; its FK to memberships
    // cascades the other direction.
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(t.org.0)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(t.user.0)
        .execute(pool)
        .await;
}

/// Two-tenant cross-contamination matrix. Each per-tenant store is rebuilt
/// with the other tenant's org id and asserted to see zero of the first
/// tenant's data. Mirrors acceptance #6 at the storage layer — the API layer
/// pulls these stores through `AppState`, so storage-level isolation is the
/// invariant the HTTP routes rest on.
#[tokio::test]
#[ignore]
async fn two_tenants_never_see_each_others_data() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };

    let a = provision_tenant(&pool, "tenant-a").await;
    let b = provision_tenant(&pool, "tenant-b").await;

    // One shared store per backend. Isolation must come from the `org`
    // argument, not from a per-tenant construction — that is exactly the
    // production shape now (`AppState` holds one store; the request supplies
    // the org via `CurrentOrg`).
    let target_store = PostgresTargetStore::from_pool(pool.clone(), None);
    let results_store = ClickhouseResultsStore::from_client(ch.clone());
    let maintenance_store = PgMaintenanceStore::new(pool.clone());
    // ONE shared sink (production shape: `AppState` holds a single sink).
    // Isolation must come from the per-result `org_id`, NOT from a
    // per-tenant sink construction — a per-tenant sink masked the
    // write-path org-stamping bug (results landed under the wrong org).
    let result_sink = ClickhouseResultSink::new(
        ch.clone(),
        "default".into(),
        "default".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );

    // ── Targets ──────────────────────────────────────────────────────────
    let target_a = target_store
        .create(
            a.org,
            target_named(&format!("a-target-{}", Uuid::now_v7())),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create target in a");
    let target_b = target_store
        .create(
            b.org,
            target_named(&format!("b-target-{}", Uuid::now_v7())),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("create target in b");

    let a_list = target_store
        .list(a.org, TargetFilter::default())
        .await
        .unwrap();
    let b_list = target_store
        .list(b.org, TargetFilter::default())
        .await
        .unwrap();
    assert!(
        a_list.iter().any(|t| t.id == target_a.id),
        "tenant a sees its own target"
    );
    assert!(
        !a_list.iter().any(|t| t.id == target_b.id),
        "tenant a must not see tenant b's target"
    );
    assert!(
        b_list.iter().any(|t| t.id == target_b.id),
        "tenant b sees its own target"
    );
    assert!(
        !b_list.iter().any(|t| t.id == target_a.id),
        "tenant b must not see tenant a's target"
    );

    // Get-by-id across tenants resolves to None.
    assert!(
        target_store
            .get(a.org, target_b.id)
            .await
            .unwrap()
            .is_none(),
        "tenant a get of b's target id must be None"
    );
    assert!(
        target_store
            .get(b.org, target_a.id)
            .await
            .unwrap()
            .is_none(),
        "tenant b get of a's target id must be None"
    );

    // ── Maintenance windows ──────────────────────────────────────────────
    let now = Utc::now();
    let mw_a = maintenance_store
        .create(
            a.org,
            NewMaintenanceWindow {
                title: "a-window".into(),
                description: None,
                starts_at: now,
                ends_at: now + chrono::Duration::hours(1),
                component_ids: vec![],
                suppress_alerts: true,
            },
            WriteSource::Ui,
        )
        .await
        .expect("a mw");
    let mw_b = maintenance_store
        .create(
            b.org,
            NewMaintenanceWindow {
                title: "b-window".into(),
                description: None,
                starts_at: now,
                ends_at: now + chrono::Duration::hours(1),
                component_ids: vec![],
                suppress_alerts: true,
            },
            WriteSource::Ui,
        )
        .await
        .expect("b mw");

    assert!(
        maintenance_store
            .get(a.org, mw_b.id)
            .await
            .unwrap()
            .is_none(),
        "tenant a maintenance get of b's id must be None"
    );
    assert!(
        maintenance_store
            .get(b.org, mw_a.id)
            .await
            .unwrap()
            .is_none(),
        "tenant b maintenance get of a's id must be None"
    );

    // A window that silences paging does so only for its own org's monitor,
    // and only while `suppress_alerts` is on.
    let covering = maintenance_store
        .create(
            a.org,
            NewMaintenanceWindow {
                title: "a-covering".into(),
                description: None,
                starts_at: now - chrono::Duration::minutes(5),
                ends_at: now + chrono::Duration::hours(1),
                component_ids: vec![target_a.id],
                suppress_alerts: true,
            },
            WriteSource::Ui,
        )
        .await
        .expect("a covering mw");
    assert!(
        maintenance_store
            .alerts_suppressed(a.org, target_a.id)
            .await
            .unwrap()
    );
    assert!(
        !maintenance_store
            .alerts_suppressed(b.org, target_a.id)
            .await
            .unwrap(),
        "another tenant's window must never silence this org's paging"
    );

    maintenance_store
        .update(
            a.org,
            covering.id,
            uptimepage::domain::MaintenanceWindowUpdate {
                suppress_alerts: Some(false),
                ..Default::default()
            },
            WriteSource::Ui,
        )
        .await
        .expect("turn suppression off");
    assert!(
        !maintenance_store
            .alerts_suppressed(a.org, target_a.id)
            .await
            .unwrap(),
        "a public-only window leaves paging alone"
    );

    // ── Status pages + curated components ─────────────────────────────────
    // i64::MAX bypasses the per-org page cap — this asserts isolation, not quota.
    let page_store = PgStatusPageStore::new(pool.clone());
    let page_a = page_store
        .create(
            a.org,
            NewStatusPage {
                slug: unique_slug("sp-a"),
                name: "a-page".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("a page")
        .expect("within page cap");
    let page_b = page_store
        .create(
            b.org,
            NewStatusPage {
                slug: unique_slug("sp-b"),
                name: "b-page".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("b page")
        .expect("within page cap");

    // Cross-org page get → None; list never enumerates the other tenant's page.
    assert!(
        page_store.get(a.org, page_b.id).await.unwrap().is_none(),
        "tenant a get of b's page id must be None"
    );
    assert!(
        page_store.get(b.org, page_a.id).await.unwrap().is_none(),
        "tenant b get of a's page id must be None"
    );
    let a_pages = page_store.list(a.org).await.unwrap();
    assert!(
        a_pages.iter().any(|p| p.id == page_a.id),
        "tenant a sees its own page"
    );
    assert!(
        !a_pages.iter().any(|p| p.id == page_b.id),
        "tenant a must not see tenant b's page"
    );

    // Curate A's target onto A's page; B must not read that component set even
    // with A's exact page id (the store scopes `list_components` by org).
    page_store
        .add_component(
            a.org,
            page_a.id,
            NewStatusPageComponent {
                target_id: target_a.id,
                public_name: None,
                public_description: None,
                public_group: None,
                sort_order: 0,
                detail_link_enabled: false,
            },
            i64::MAX,
            None,
        )
        .await
        .expect("add a's component");
    let a_comps = page_store.list_components(a.org, page_a.id).await.unwrap();
    assert!(
        a_comps.iter().any(|c| c.target_id == target_a.id),
        "tenant a sees its curated component"
    );
    let b_view_of_a_page = page_store.list_components(b.org, page_a.id).await.unwrap();
    assert!(
        b_view_of_a_page.is_empty(),
        "tenant b must not read components of a's page"
    );

    // ── ClickHouse results ───────────────────────────────────────────────
    // Both written through the SAME shared sink; isolation must come from
    // the per-result org_id alone.
    result_sink
        .write_batch(&[ok_result(target_a.id, a.org.0)])
        .await
        .expect("ch insert a");
    result_sink
        .write_batch(&[ok_result(target_b.id, b.org.0)])
        .await
        .expect("ch insert b");

    let range = time_range_around_now();
    // Queried with org_a → sees a row for target_a.
    let a_results = results_store
        .list_results(
            a.org,
            target_a.id,
            ClampedRange::unclamped(range),
            10,
            0,
            None,
        )
        .await
        .unwrap();
    assert!(
        !a_results.is_empty(),
        "results_store under org A must see A's own row"
    );
    // Regression (the write-path bug): the persisted row must carry the
    // TARGET's org, not a sink-construction default. A shared sink that
    // stamped a default org would put A's result under the wrong tenant
    // and this would fail.
    assert!(
        a_results.iter().any(|r| r.target_id == target_a.id),
        "the row read back under org A must be the one written for target_a \
         (not a stray) — proves the shared sink stamped A's org, not a default"
    );
    assert_eq!(
        a_results[0].org_id, a.org.0,
        "written result must be stamped with the target's own org"
    );
    // Same store, b's target id, but org_a → 0 rows (org_id filter wins over
    // target_id match: CH rows tagged with org_b are invisible under org_a).
    let cross = results_store
        .list_results(
            a.org,
            target_b.id,
            ClampedRange::unclamped(range),
            10,
            0,
            None,
        )
        .await
        .unwrap();
    assert!(
        cross.is_empty(),
        "results_store under org A must not return rows tagged with B's org"
    );
    // list_results above already asserts the cross-org rows are filtered;
    // no separate count check is needed now that count_results is gone.

    // ── Membership ──────────────────────────────────────────────────────
    assert!(
        is_active_member(&pool, a.user, a.org).await.unwrap(),
        "owner is active in own org"
    );
    assert!(
        !is_active_member(&pool, a.user, b.org).await.unwrap(),
        "user A must not be member of org B"
    );
    assert!(
        !is_active_member(&pool, b.user, a.org).await.unwrap(),
        "user B must not be member of org A"
    );

    teardown(&pool, &ch, &a).await;
    teardown(&pool, &ch, &b).await;
}
