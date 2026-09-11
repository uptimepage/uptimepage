//! PG-backed integration tests. Provisioned per-test via `#[sqlx::test]` which
//! requires `DATABASE_URL` to be set. Tests are skipped at the cargo level when
//! the env var is absent (sqlx::test refuses to run without it).

mod common;

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use uptimepage::domain::{
    CheckSpec, ExpectedStatus, NewTarget, NewTargetWithRegions, RegionIncidentPolicy, WriteSource,
};
use uptimepage::security::Cipher;
use uptimepage::storage::{PostgresTargetStore, TargetStore};
use url::Url;
use uuid::Uuid;

use crate::common::{default_http_check, test_cipher};

/// Provision a fresh org with a unique slug and return a store paired with
/// the new org's id. Each test gets its own org so parallel test runs don't
/// step on each other's `targets` rows.
async fn store_with_default_org(
    pool: PgPool,
    cipher: Option<Arc<Cipher>>,
) -> (PostgresTargetStore, uptimepage::domain::OrgId) {
    let slug = format!("bulk-{}", Uuid::now_v7().simple());
    let slug = &slug[..slug.len().min(30)];
    let (id,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Bulk Test', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(&pool)
    .await
    .expect("insert bulk-test org");
    let org_id = uptimepage::domain::OrgId(id);
    let store = PostgresTargetStore::from_pool(pool, cipher);
    (store, org_id)
}

fn unplaced(items: Vec<NewTarget>) -> Vec<NewTargetWithRegions> {
    items
        .into_iter()
        .map(|target| NewTargetWithRegions {
            target,
            regions: Vec::new(),
        })
        .collect()
}

fn make(name: &str, tags: Vec<String>) -> NewTarget {
    let url = Url::parse("https://example.com/").unwrap();
    NewTarget {
        name: name.into(),
        check: CheckSpec::Http(default_http_check(url, ExpectedStatus::Exact(200))),
        interval: Duration::from_secs(30),
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
    }
}

fn make_flow(name: &str) -> NewTarget {
    use uptimepage::domain::{FlowCheck, FlowStep};
    NewTarget {
        name: name.into(),
        check: CheckSpec::Flow(FlowCheck {
            start_url: Url::parse("https://example.com/login").unwrap(),
            steps: vec![FlowStep::AssertUrl {
                contains: "/x".into(),
            }],
            timeout: Duration::from_secs(30),
            step_timeout: Duration::from_secs(5),
            verify_tls: true,
        }),
        interval: Duration::from_secs(300),
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

#[sqlx::test(migrations = "./migrations/postgres")]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn flow_sub_cap_enforced_atomically(pool: PgPool) {
    let (store, org) = store_with_default_org(pool, None).await;
    // Generous overall cap, flow sub-cap of 1: the first flow lands, the second
    // is rejected by the in-INSERT guard even though max_targets has room.
    store
        .create(org, make_flow("f1"), WriteSource::Ui, 100, 1)
        .await
        .expect("first flow within sub-cap");
    let err = store
        .create(org, make_flow("f2"), WriteSource::Ui, 100, 1)
        .await
        .expect_err("second flow over sub-cap");
    match err {
        uptimepage::error::AppError::QuotaExceeded { quota, .. } => {
            assert_eq!(quota, "max_flow_checks");
        }
        other => panic!("expected max_flow_checks quota error, got {other:?}"),
    }
    // A non-flow target is unaffected by the flow sub-cap.
    store
        .create(org, make("http-ok", vec![]), WriteSource::Ui, 100, 1)
        .await
        .expect("http target unaffected by flow cap");
}

#[sqlx::test(migrations = "./migrations/postgres")]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn bulk_create_with_ragged_tags(pool: PgPool) {
    let (store, org) = store_with_default_org(pool, None).await;
    let items = vec![
        make("t1", vec!["a".into(), "b".into()]),
        make("t2", vec![]),
        make("t3", vec!["only".into()]),
    ];

    let created = store
        .bulk_create(org, unplaced(items), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("bulk_create succeeds");

    assert_eq!(created.len(), 3);
    assert_eq!(created[0].name, "t1");
    assert_eq!(created[0].tags, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(created[1].name, "t2");
    assert!(created[1].tags.is_empty());
    assert_eq!(created[2].name, "t3");
    assert_eq!(created[2].tags, vec!["only".to_string()]);
}

/// Regions and the policy land with the rows, so a bulk never leaves a
/// monitor half-placed.
#[sqlx::test(migrations = "./migrations/postgres")]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn bulk_create_places_each_row_in_one_write(pool: PgPool) {
    let (store, org) = store_with_default_org(pool.clone(), None).await;
    for region in ["bulk-eu", "bulk-us"] {
        sqlx::query("INSERT INTO regions (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
            .bind(region)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut counted = make("counted", vec![]);
    counted.region_policy = Some(RegionIncidentPolicy::Count(2));
    let items = vec![
        NewTargetWithRegions {
            target: counted,
            regions: vec!["bulk-eu".into(), "bulk-us".into()],
        },
        NewTargetWithRegions {
            target: make("single", vec![]),
            regions: vec!["bulk-us".into()],
        },
    ];

    let created = store
        .bulk_create(org, items, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("bulk_create succeeds");

    assert_eq!(created[0].region_policy, RegionIncidentPolicy::Count(2));
    assert_eq!(created[1].region_policy, RegionIncidentPolicy::Majority);
    assert_eq!(
        store.regions_for_target(org, created[0].id).await.unwrap(),
        Some(vec!["bulk-eu".to_string(), "bulk-us".to_string()])
    );
    assert_eq!(
        store.regions_for_target(org, created[1].id).await.unwrap(),
        Some(vec!["bulk-us".to_string()])
    );
}

#[sqlx::test(migrations = "./migrations/postgres")]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn bulk_create_empty_is_noop(pool: PgPool) {
    let (store, org) = store_with_default_org(pool, None).await;
    let result = store
        .bulk_create(org, vec![], WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("empty bulk ok");
    assert!(result.is_empty());
}

#[sqlx::test(migrations = "./migrations/postgres")]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn credentials_stored_as_ciphertext_envelope(pool: PgPool) {
    let (store, org) = store_with_default_org(pool.clone(), Some(test_cipher())).await;
    let url = Url::parse("https://example.com/").unwrap();
    let mut http = default_http_check(url, ExpectedStatus::Exact(200));
    http.basic_auth = Some(("alice".into(), "s3cret".into()));
    http.bearer_token = Some("tok.en.value".into());
    let new = NewTarget {
        name: "with-secrets".into(),
        check: CheckSpec::Http(http),
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
    };

    let created = store
        .create(org, new, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("create");

    // Round-trip via the store decrypts.
    let fetched = store
        .get(org, created.id)
        .await
        .expect("get")
        .expect("present");
    match fetched.check {
        CheckSpec::Http(h) => {
            assert_eq!(h.basic_auth, Some(("alice".into(), "s3cret".into())));
            assert_eq!(h.bearer_token.as_deref(), Some("tok.en.value"));
        }
        _ => panic!("expected http check"),
    }

    // Raw row holds ciphertext envelope, not plaintext.
    let raw: (serde_json::Value,) = sqlx::query_as("SELECT check_spec FROM targets WHERE id = $1")
        .bind(created.id)
        .fetch_one(&pool)
        .await
        .expect("raw select");
    let raw_str = raw.0.to_string();
    assert!(!raw_str.contains("alice"), "plaintext leaked: {raw_str}");
    assert!(!raw_str.contains("s3cret"), "plaintext leaked: {raw_str}");
    assert!(
        !raw_str.contains("tok.en.value"),
        "plaintext leaked: {raw_str}"
    );
    assert!(
        raw_str.contains("\"$enc\":\"v1:"),
        "envelope missing: {raw_str}"
    );
}

#[sqlx::test(migrations = "./migrations/postgres")]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn no_credentials_no_envelope(pool: PgPool) {
    let (store, org) = store_with_default_org(pool.clone(), Some(test_cipher())).await;
    let new = make("plain", vec![]);
    let created = store
        .create(org, new, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("create");

    let raw: (serde_json::Value,) = sqlx::query_as("SELECT check_spec FROM targets WHERE id = $1")
        .bind(created.id)
        .fetch_one(&pool)
        .await
        .expect("raw select");
    let raw_str = raw.0.to_string();
    assert!(!raw_str.contains("$enc"), "unexpected envelope: {raw_str}");
}

#[sqlx::test(migrations = "./migrations/postgres")]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn legacy_plaintext_row_decrypts_passthrough(pool: PgPool) {
    // Simulate a row written before encryption was enabled: insert plaintext
    // basic_auth directly, then read via a cipher-enabled store.
    let check_json = serde_json::json!({
        "type": "http",
        "url": "https://example.com/",
        "method": "GET",
        "timeout": 3000,
        "follow_redirects": false,
        "max_redirects": 0,
        "expected_status": { "kind": "exact", "value": 200 },
        "expected_body_contains": null,
        "headers": {},
        "body": null,
        "verify_tls": true,
        "basic_auth": ["legacy", "old-pass"],
        "bearer_token": null
    });
    let slug = format!("bulk-{}", Uuid::now_v7().simple());
    let slug = &slug[..slug.len().min(30)];
    let (org_uuid,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Bulk Test', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(&pool)
    .await
    .expect("insert bulk-test org");
    let org_id = uptimepage::domain::OrgId(org_uuid);
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO targets (id, org_id, name, check_spec, interval_secs, enabled, tags) \
         VALUES ($1, $2, 'legacy', $3, 30, true, ARRAY[]::text[])",
    )
    .bind(id)
    .bind(org_id.0)
    .bind(check_json)
    .execute(&pool)
    .await
    .expect("insert legacy row");

    let store = PostgresTargetStore::from_pool(pool, Some(test_cipher()));
    let fetched = store.get(org_id, id).await.expect("get").expect("present");
    match fetched.check {
        CheckSpec::Http(h) => {
            assert_eq!(h.basic_auth, Some(("legacy".into(), "old-pass".into())));
        }
        _ => panic!("expected http check"),
    }
}
