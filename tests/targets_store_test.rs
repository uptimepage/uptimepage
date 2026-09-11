//! Live-PG coverage for the targets store: the `kind` filter + `count_by_kind`
//! pushdown (the generated `kind` column drives the SQL filter, and chip
//! tallies are org-wide, not page-scoped), who an update credits, and the tag
//! cap on a server-side merge.

mod common;

use std::time::Duration;

use uptimepage::domain::target::MAX_TAGS_PER_TARGET;
use uptimepage::domain::{CheckSpec, ExpectedStatus, NewTarget, OrgId, TcpCheck, WriteSource};
use uptimepage::storage::{PostgresTargetStore, TargetFilter, TargetStore, create_org_with_owner};
use url::Url;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

async fn seed(
    store: &PostgresTargetStore,
    org: OrgId,
    name: &str,
    check: CheckSpec,
    group: Option<&str>,
) -> Uuid {
    let nt = NewTarget {
        name: name.into(),
        check,
        interval: Duration::from_secs(30),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: group.map(str::to_owned),
        owner_user_id: None,
        regions: None,
    };
    store
        .create(org, nt, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .unwrap()
        .id
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn kind_filter_and_counts_are_org_wide() {
    let Some((db, name)) = common::fresh_test_db("targets_kind").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "k").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("k"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    let http = || {
        CheckSpec::Http(common::default_http_check(
            Url::parse("https://example.com/").unwrap(),
            ExpectedStatus::Exact(200),
        ))
    };
    let tcp = || {
        CheckSpec::Tcp(TcpCheck {
            host: "db.example.com".into(),
            port: 5432,
            timeout: Duration::from_secs(3),
        })
    };
    seed(&store, org.id, "a", http(), None).await;
    seed(&store, org.id, "b", http(), None).await;
    seed(&store, org.id, "c", tcp(), None).await;

    // Org-wide tally keyed by the check_spec type tag.
    let counts = store
        .count_by_kind(org.id, TargetFilter::default())
        .await
        .unwrap();
    assert_eq!(counts.get("http").copied(), Some(2));
    assert_eq!(counts.get("tcp").copied(), Some(1));

    // The generated `kind` column drives the SQL filter.
    let only_tcp = store
        .list(
            org.id,
            TargetFilter {
                kind: Some("tcp".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(only_tcp.len(), 1);
    assert_eq!(only_tcp[0].name, "c");

    let only_http = store
        .list(
            org.id,
            TargetFilter {
                kind: Some("http".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(only_http.len(), 2);

    common::drop_test_db(&name).await;
}

/// The subject lives under a different JSON key per kind, and the answer must
/// not cross tenants.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn hosts_by_kind_reads_each_kinds_subject_and_stays_org_scoped() {
    let Some((db, name)) = common::fresh_test_db("targets_hosts").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user_a = common::make_user(&pool, "h").await;
    let user_b = common::make_user(&pool, "h").await;
    let org_a = create_org_with_owner(&pool, user_a, &common::unique_slug("ha"), "A")
        .await
        .unwrap()
        .unwrap();
    let org_b = create_org_with_owner(&pool, user_b, &common::unique_slug("hb"), "B")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    // `host` for tls_cert, `domain` for dns and domain_expiry.
    let tls: CheckSpec = serde_json::from_str(
        r#"{"type":"tls_cert","host":"acme.com","port":443,"warn_days":30,"critical_days":7,"timeout":5000}"#,
    )
    .unwrap();
    let dns: CheckSpec = serde_json::from_str(
        r#"{"type":"dns","domain":"api.acme.com","record_type":"A","timeout":3000}"#,
    )
    .unwrap();
    let expiry: CheckSpec = serde_json::from_str(
        r#"{"type":"domain_expiry","domain":"acme.com","warn_days":30,"critical_days":7,"timeout":5000}"#,
    )
    .unwrap();
    let http = CheckSpec::Http(common::default_http_check(
        Url::parse("https://acme.com/").unwrap(),
        ExpectedStatus::Exact(200),
    ));
    seed(&store, org_a.id, "cert", tls, None).await;
    seed(&store, org_a.id, "record", dns, None).await;
    seed(&store, org_a.id, "registration", expiry, None).await;
    // Out of scope for the filter even though it shares the host.
    seed(&store, org_a.id, "site", http.clone(), None).await;
    // Another tenant watching the same host must not leak in.
    seed(&store, org_b.id, "b-site", http, None).await;

    let mut got = store
        .hosts_by_kind(org_a.id, &["tls_cert", "domain_expiry", "dns"])
        .await
        .unwrap();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("dns".to_string(), "api.acme.com".to_string()),
            ("domain_expiry".to_string(), "acme.com".to_string()),
            ("tls_cert".to_string(), "acme.com".to_string()),
        ]
    );
    assert!(
        store
            .hosts_by_kind(org_b.id, &["tls_cert", "domain_expiry", "dns"])
            .await
            .unwrap()
            .is_empty(),
        "B runs no coverage-shaped monitors, so it must see none of A's"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn region_filter_and_distinct_groups_are_org_wide() {
    let Some((db, name)) = common::fresh_test_db("targets_region").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "r").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("r"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    sqlx::query(
        "INSERT INTO regions (id, name) VALUES ('eu-west','eu-west'),('us-east','us-east')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let http = || {
        CheckSpec::Http(common::default_http_check(
            Url::parse("https://example.com/").unwrap(),
            ExpectedStatus::Exact(200),
        ))
    };
    let a = seed(&store, org.id, "a", http(), Some("alpha")).await;
    let b = seed(&store, org.id, "b", http(), Some("beta")).await;
    seed(&store, org.id, "c", http(), None).await;
    store
        .set_target_regions(org.id, a, &["eu-west".into()])
        .await
        .unwrap();
    store
        .set_target_regions(org.id, b, &["us-east".into()])
        .await
        .unwrap();

    // Region filter lists only targets that run there.
    let eu = store
        .list(
            org.id,
            TargetFilter {
                region: Some("eu-west".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(eu.len(), 1);
    assert_eq!(eu[0].name, "a");

    // Group options are org-wide, sorted, deduped — not page-scoped.
    let groups = store.distinct_groups(org.id).await.unwrap();
    assert_eq!(groups, vec!["alpha".to_string(), "beta".to_string()]);

    common::drop_test_db(&name).await;
}

/// A `None` source leaves the `write_source` marker for the next writer to read.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_update_can_decline_to_claim_authorship() {
    let Some((db, name)) = common::fresh_test_db("targets_write_source").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "ws").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("ws"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    let declared = NewTarget {
        name: "declared-in-tf".into(),
        check: CheckSpec::Http(common::default_http_check(
            Url::parse("https://example.com/").unwrap(),
            ExpectedStatus::Exact(200),
        )),
        interval: Duration::from_secs(60),
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
    };
    let id = store
        .create(org.id, declared, WriteSource::Terraform, i64::MAX, i64::MAX)
        .await
        .unwrap()
        .id;

    let updated = store
        .update(
            org.id,
            id,
            uptimepage::domain::TargetUpdate {
                interval: Some(Duration::from_secs(120)),
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap()
        .expect("target exists");
    assert_eq!(updated.interval, Duration::from_secs(120), "edit applied");
    assert_eq!(
        updated.write_source,
        WriteSource::Terraform,
        "an unattributed write must not repaint the marker"
    );

    let updated = store
        .update(
            org.id,
            id,
            uptimepage::domain::TargetUpdate {
                interval: Some(Duration::from_secs(180)),
                ..Default::default()
            },
            Some(WriteSource::Api),
            None,
        )
        .await
        .unwrap()
        .expect("target exists");
    assert_eq!(updated.write_source, WriteSource::Api, "named writer wins");

    common::drop_test_db(&name).await;
}

/// A bulk add merges server-side, so the cap lands on a list no request carried.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_bulk_tag_add_stops_at_the_cap_and_says_which_monitor_was_full() {
    let Some((db, name)) = common::fresh_test_db("targets_tag_cap").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "cap").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("cap"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);

    let make = |tags: Vec<String>, label: &str| NewTarget {
        name: label.to_string(),
        check: CheckSpec::Http(common::default_http_check(
            Url::parse("https://example.com/").unwrap(),
            ExpectedStatus::Exact(200),
        )),
        interval: Duration::from_secs(60),
        enabled: true,
        tags,
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        regions: None,
    };

    let full: Vec<String> = (0..MAX_TAGS_PER_TARGET).map(|i| format!("t{i}")).collect();
    let crowded = store
        .create(
            org.id,
            make(full, "crowded"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap()
        .id;
    let roomy = store
        .create(
            org.id,
            make(vec!["prod".into()], "roomy"),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap()
        .id;

    let outcome = store
        .add_tags(org.id, &[crowded, roomy], &["fresh".to_string()])
        .await
        .unwrap();
    assert_eq!(outcome.updated, vec![roomy]);
    assert_eq!(outcome.over_cap, vec![crowded]);

    let untouched = store.get(org.id, crowded).await.unwrap().unwrap();
    assert_eq!(untouched.tags.len(), MAX_TAGS_PER_TARGET);
    assert!(!untouched.tags.iter().any(|t| t == "fresh"));

    // A tag already present is not a new one, so a full monitor still accepts it.
    let outcome = store
        .add_tags(org.id, &[crowded], &["t0".to_string()])
        .await
        .unwrap();
    assert_eq!(outcome.updated, vec![crowded]);

    common::drop_test_db(&name).await;
}
