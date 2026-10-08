//! Live-PG coverage for the targets store: the `kind` filter + `count_by_kind`
//! pushdown (the generated `kind` column drives the SQL filter, and chip
//! tallies are org-wide, not page-scoped), who an update credits, the tag
//! cap on a server-side merge, and the bulk interval and channel edits.

use crate::common;

use std::time::Duration;

use uptimepage::domain::target::MAX_TAGS_PER_TARGET;
use uptimepage::domain::{CheckSpec, ExpectedStatus, NewTarget, OrgId, TcpCheck, WriteSource};
use uptimepage::storage::traits::ChannelEdit;
use uptimepage::storage::{PostgresTargetStore, TargetFilter, TargetStore, create_org_with_owner};
use url::Url;
use uuid::Uuid;

use crate::common::MIGRATOR;

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
        recovery_period_secs: 0,
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
        recovery_period_secs: 0,
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

/// The recovery hold survives every write path and every read back, the
/// cross-tenant one the incident writer walks included.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_recovery_period_round_trips_through_every_write() {
    use uptimepage::domain::NewTargetWithRegions;
    use uptimepage::storage::AdminRepo;

    let Some((db, name)) = common::fresh_test_db("targets_recovery").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "rp").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("rp"), "Co")
        .await
        .unwrap()
        .unwrap();
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let held = |name: &str, secs: u32| NewTarget {
        name: name.into(),
        check: CheckSpec::Tcp(TcpCheck {
            host: "db.example.com".into(),
            port: 5432,
            timeout: Duration::from_secs(1),
        }),
        interval: Duration::from_secs(60),
        enabled: true,
        tags: vec![],
        alerts: Default::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        recovery_period_secs: secs,
        group_name: None,
        owner_user_id: None,
        regions: None,
    };

    let single = store
        .create(
            org.id,
            held("single", 600),
            WriteSource::Ui,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
    assert_eq!(single.recovery_period_secs, 600);
    let bulk = store
        .bulk_create(
            org.id,
            vec![
                NewTargetWithRegions {
                    target: held("bulk-a", 120),
                    regions: vec![],
                },
                NewTargetWithRegions {
                    target: held("bulk-b", 0),
                    regions: vec![],
                },
            ],
            WriteSource::Api,
            i64::MAX,
            i64::MAX,
        )
        .await
        .unwrap();
    assert_eq!(
        bulk.iter()
            .map(|t| t.recovery_period_secs)
            .collect::<Vec<_>>(),
        [120, 0]
    );

    let updated = store
        .update(
            org.id,
            single.id,
            uptimepage::domain::TargetUpdate {
                recovery_period_secs: Some(1800),
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap()
        .expect("target exists");
    assert_eq!(updated.recovery_period_secs, 1800);
    let untouched = store
        .update(
            org.id,
            single.id,
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
    assert_eq!(
        untouched.recovery_period_secs, 1800,
        "an omitted field is kept"
    );
    assert_eq!(
        store
            .get(org.id, single.id)
            .await
            .unwrap()
            .unwrap()
            .recovery_period_secs,
        1800
    );

    let page = AdminRepo::new(pool.clone(), None, "recovery_test")
        .next_enabled_target_page(None, 100)
        .await
        .unwrap();
    let walked = page
        .iter()
        .find(|(_, t)| t.id == single.id)
        .expect("the writer sees the monitor");
    assert_eq!(walked.1.recovery_period_secs, 1800);

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
        recovery_period_secs: 0,
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

/// The skip list is applied in the statement, so a skipped kind keeps its
/// interval and comes back named; a foreign org's id is neither.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_bulk_interval_skips_the_named_kinds() {
    let Some((db, name)) = common::fresh_test_db("targets_bulk_interval").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "iv").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("iv"), "Co")
        .await
        .unwrap()
        .unwrap();
    let stranger = common::make_user(&pool, "iv2").await;
    let other = create_org_with_owner(&pool, stranger, &common::unique_slug("iv2"), "Other")
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
    let tcp = CheckSpec::Tcp(TcpCheck {
        host: "db.example.com".into(),
        port: 5432,
        timeout: Duration::from_secs(3),
    });
    let web = seed(&store, org.id, "web", http(), None).await;
    let db_port = seed(&store, org.id, "db", tcp, None).await;
    let foreign = seed(&store, other.id, "foreign", http(), None).await;

    let outcome = store
        .set_interval(
            org.id,
            &[web, db_port, foreign],
            Duration::from_secs(300),
            &["tcp"],
        )
        .await
        .unwrap();
    assert_eq!(outcome.updated, vec![web]);
    assert_eq!(outcome.skipped, vec![(db_port, "tcp".to_string())]);

    let interval = |org: OrgId, id: Uuid| {
        let store = &store;
        async move { store.get(org, id).await.unwrap().unwrap().interval }
    };
    assert_eq!(interval(org.id, web).await, Duration::from_secs(300));
    assert_eq!(interval(org.id, db_port).await, Duration::from_secs(30));
    assert_eq!(interval(other.id, foreign).await, Duration::from_secs(30));

    common::drop_test_db(&name).await;
}

/// Add keeps what is bound and binds a channel once however often it is
/// named, remove leaves the rest in order, and replace with nothing unbinds
/// every channel.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_bulk_channel_edit_merges_into_each_monitors_bindings() {
    let Some((db, name)) = common::fresh_test_db("targets_bulk_channels").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let user = common::make_user(&pool, "ch").await;
    let org = create_org_with_owner(&pool, user, &common::unique_slug("ch"), "Co")
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
    let [slack, pager, email] = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
    let bound = seed(&store, org.id, "bound", http(), None).await;
    let bare = seed(&store, org.id, "bare", http(), None).await;
    store
        .edit_channels(org.id, &[bound], ChannelEdit::Replace(&[slack]))
        .await
        .unwrap();

    let channels = |id: Uuid| {
        let store = &store;
        async move {
            store
                .get(org.id, id)
                .await
                .unwrap()
                .unwrap()
                .alerts
                .iter()
                .map(|b| b.channel_id)
                .collect::<Vec<_>>()
        }
    };

    let hit = store
        .edit_channels(org.id, &[bound, bare], ChannelEdit::Add(&[slack, pager]))
        .await
        .unwrap();
    assert_eq!(hit.len(), 2);
    assert_eq!(channels(bound).await, vec![slack, pager]);
    assert_eq!(channels(bare).await, vec![slack, pager]);

    store
        .edit_channels(org.id, &[bound], ChannelEdit::Add(&[email, email]))
        .await
        .unwrap();
    store
        .edit_channels(org.id, &[bound, bare], ChannelEdit::Remove(&[pager]))
        .await
        .unwrap();
    assert_eq!(channels(bound).await, vec![slack, email]);
    assert_eq!(channels(bare).await, vec![slack]);

    let stranger = common::make_user(&pool, "ch2").await;
    let other = create_org_with_owner(&pool, stranger, &common::unique_slug("ch2"), "Other")
        .await
        .unwrap()
        .unwrap();
    let foreign = seed(&store, other.id, "foreign", http(), None).await;
    store
        .edit_channels(other.id, &[foreign], ChannelEdit::Replace(&[slack]))
        .await
        .unwrap();

    let hit = store
        .edit_channels(org.id, &[bound, bare, foreign], ChannelEdit::Replace(&[]))
        .await
        .unwrap();
    assert_eq!(
        hit.len(),
        2,
        "a foreign org's monitor is not this org's to edit"
    );
    assert!(channels(bound).await.is_empty());
    assert!(channels(bare).await.is_empty());
    let untouched = store.get(other.id, foreign).await.unwrap().unwrap();
    assert_eq!(
        untouched
            .alerts
            .iter()
            .map(|b| b.channel_id)
            .collect::<Vec<_>>(),
        vec![slack]
    );

    common::drop_test_db(&name).await;
}
