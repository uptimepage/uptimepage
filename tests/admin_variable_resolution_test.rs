//! Live-PG contract for the config-pull serving layer: a region's targets are
//! served with their `{{var}}` references already resolved (so an agent needs
//! no database), and editing a variable bumps the region pull etag so agents
//! re-pull the new values.

use crate::common;

use sqlx::PgPool;
use uptimepage::domain::{CheckSpec, NewVariable, OrgId};
use uptimepage::storage::{AdminRepo, CreateVariableOutcome, PgVariableStore, VariableStore};
use uuid::Uuid;

use common::test_cipher;

use crate::common::MIGRATOR;

fn var_check_spec() -> serde_json::Value {
    serde_json::json!({
        "type": "http",
        "url": "https://{{base}}/health",
        "method": "GET",
        "timeout": 5000,
        "follow_redirects": false,
        "max_redirects": 0,
        "expected_status": {"kind": "exact", "value": 200},
        "headers": {"x-api-key": "{{api_key}}"},
        "verify_tls": true,
    })
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

fn http(check: &CheckSpec) -> &uptimepage::domain::HttpCheck {
    match check {
        CheckSpec::Http(h) => h,
        _ => panic!("expected http check"),
    }
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn region_pull_resolves_variables_and_etag_tracks_them() {
    let Some((db_url, db_name)) = common::fresh_test_db("admin_var").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    MIGRATOR.run(&pool).await.unwrap();

    let region = "eu-var";
    sqlx::query("INSERT INTO regions (id, name) VALUES ($1, $1)")
        .bind(region)
        .execute(&pool)
        .await
        .unwrap();
    let org = seed_org(
        &pool,
        &format!("avar-{}", &Uuid::new_v4().simple().to_string()[..8]),
    )
    .await;

    let store = PgVariableStore::new(pool.clone(), Some(test_cipher()));
    store
        .create(
            OrgId(org),
            NewVariable {
                key: "base".into(),
                is_secret: false,
                value: "api.example.com".into(),
            },
            None,
        )
        .await
        .unwrap();
    let secret = match store
        .create(
            OrgId(org),
            NewVariable {
                key: "api_key".into(),
                is_secret: true,
                value: "sk-live".into(),
            },
            None,
        )
        .await
        .unwrap()
    {
        CreateVariableOutcome::Created(v) => v,
        CreateVariableOutcome::DuplicateKey => panic!("unexpected duplicate"),
    };

    let (target,): (Uuid,) = sqlx::query_as(
        r#"INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled)
           VALUES ($1, 't', $2::jsonb, 60, true) RETURNING id"#,
    )
    .bind(org)
    .bind(var_check_spec())
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
        .bind(target)
        .bind(region)
        .execute(&pool)
        .await
        .unwrap();

    let repo = AdminRepo::new(pool.clone(), Some(test_cipher()), "test");

    // Served spec carries resolved values, not the `{{ }}` literals.
    let served = repo
        .list_enabled_targets_for_region(region, true, &Default::default())
        .await
        .unwrap();
    assert_eq!(served.len(), 1);
    let h = http(&served[0].1.check);
    assert_eq!(h.url.as_str(), "https://api.example.com/health");
    assert_eq!(h.headers["x-api-key"], "sk-live");

    // A variable edit changes the etag so agents re-pull.
    let etag1 = repo
        .region_pull_etag(region, &Default::default(), "")
        .await
        .unwrap();
    store
        .update_value(OrgId(org), secret.id, "sk-rotated", None)
        .await
        .unwrap();
    let etag2 = repo
        .region_pull_etag(region, &Default::default(), "")
        .await
        .unwrap();
    assert_ne!(etag1, etag2, "variable edit must bump the region etag");

    // Re-pull serves the rotated secret.
    let served2 = repo
        .list_enabled_targets_for_region(region, true, &Default::default())
        .await
        .unwrap();
    assert_eq!(http(&served2[0].1.check).headers["x-api-key"], "sk-rotated");

    pool.close().await;
    common::drop_test_db(&db_name).await;
}
