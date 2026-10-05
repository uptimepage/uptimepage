//! Live-Postgres contract for `org_variables`: CRUD, the real `UNIQUE
//! (org_id, key)` constraint, secret seal/open round-trip through the KEK,
//! plaintext fallback when no cipher is configured, and per-org isolation on
//! every method.
//!
//! The in-memory store backs the no-DB harnesses; this suite exercises the real
//! SQL, the at-rest envelope, and the CASCADE FK.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations auto-apply on first
//! connect — point it at a throwaway DB to also validate migration 030.

use crate::common;

use uptimepage::domain::{NewVariable, OrgId, UserId, VariableId};
use uptimepage::storage::{
    CreateVariableOutcome, InMemoryVariableStore, PgVariableStore, VariableStore,
    create_org_with_owner,
};

use common::{make_user, pg_pool_from_env, test_cipher, unique_slug};

fn plain(key: &str, value: &str) -> NewVariable {
    NewVariable {
        key: key.into(),
        is_secret: false,
        value: value.into(),
    }
}

fn secret(key: &str, value: &str) -> NewVariable {
    NewVariable {
        key: key.into(),
        is_secret: true,
        value: value.into(),
    }
}

fn created(o: CreateVariableOutcome) -> uptimepage::domain::Variable {
    match o {
        CreateVariableOutcome::Created(v) => v,
        CreateVariableOutcome::DuplicateKey => panic!("expected Created, got DuplicateKey"),
    }
}

async fn two_orgs(pool: &sqlx::PgPool, tag: &str) -> (OrgId, OrgId, UserId) {
    let user_a = make_user(pool, tag).await;
    let user_b = make_user(pool, tag).await;
    let org_a = create_org_with_owner(pool, user_a, &unique_slug(tag), "A")
        .await
        .unwrap()
        .expect("org a")
        .id;
    let org_b = create_org_with_owner(pool, user_b, &unique_slug(tag), "B")
        .await
        .unwrap()
        .expect("org b")
        .id;
    (org_a, org_b, user_a)
}

/// Raw at-rest `value` straight from the row, bypassing the store's redaction —
/// to assert a secret is sealed (or plaintext under the no-KEK fallback).
async fn raw_value(pool: &sqlx::PgPool, id: VariableId) -> String {
    let (v,): (String,) = sqlx::query_as("SELECT value FROM org_variables WHERE id = $1")
        .bind(id.0)
        .fetch_one(pool)
        .await
        .unwrap();
    v
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn create_get_list_roundtrip() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, _b, actor) = two_orgs(&pool, "var-crud").await;
    let store = PgVariableStore::new(pool.clone(), Some(test_cipher()));

    let z = created(
        store
            .create(org, plain("z_url", "https://z"), Some(actor))
            .await
            .unwrap(),
    );
    let a = created(
        store
            .create(org, plain("a_url", "https://a"), Some(actor))
            .await
            .unwrap(),
    );

    let got = store.get(org, z.id).await.unwrap().unwrap();
    assert_eq!(got.value.as_deref(), Some("https://z"));

    let list = store.list(org).await.unwrap();
    let keys: Vec<&str> = list.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(keys, vec!["a_url", "z_url"], "ordered by key");
    assert_eq!(a.value.as_deref(), Some("https://a"));
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn duplicate_key_rejected_per_org() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, actor) = two_orgs(&pool, "var-dup").await;
    let store = PgVariableStore::new(pool.clone(), Some(test_cipher()));

    store
        .create(org_a, plain("api_key", "1"), Some(actor))
        .await
        .unwrap();
    assert!(matches!(
        store
            .create(org_a, plain("api_key", "2"), Some(actor))
            .await
            .unwrap(),
        CreateVariableOutcome::DuplicateKey
    ));
    // Same key, different org is allowed by the (org_id, key) unique scope.
    assert!(matches!(
        store
            .create(org_b, plain("api_key", "2"), Some(actor))
            .await
            .unwrap(),
        CreateVariableOutcome::Created(_)
    ));
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn secret_sealed_at_rest_redacted_in_view_resolves_plaintext() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, _b, actor) = two_orgs(&pool, "var-secret").await;
    let store = PgVariableStore::new(pool.clone(), Some(test_cipher()));

    let v = created(
        store
            .create(org, secret("api_key", "sk-live-123"), Some(actor))
            .await
            .unwrap(),
    );

    // Redacted everywhere the operator can read it.
    assert!(v.value.is_none());
    assert!(store.get(org, v.id).await.unwrap().unwrap().value.is_none());
    assert!(store.list(org).await.unwrap()[0].value.is_none());

    // Sealed at rest, decryptable only via resolve_map.
    assert!(
        raw_value(&pool, v.id).await.starts_with("v1:"),
        "secret must be an envelope"
    );
    let map = store.resolve_map(org).await.unwrap();
    assert_eq!(map["api_key"].value, "sk-live-123");
    assert!(map["api_key"].is_secret);
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn plaintext_fallback_without_kek() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, _b, actor) = two_orgs(&pool, "var-nokek").await;
    let store = PgVariableStore::new(pool.clone(), None);

    let v = created(
        store
            .create(org, secret("api_key", "sk-plain"), Some(actor))
            .await
            .unwrap(),
    );
    assert_eq!(
        raw_value(&pool, v.id).await,
        "sk-plain",
        "no KEK stores plaintext"
    );
    assert_eq!(
        store.resolve_map(org).await.unwrap()["api_key"].value,
        "sk-plain"
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn update_value_reseals_secret() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org, _b, actor) = two_orgs(&pool, "var-rotate").await;
    let store = PgVariableStore::new(pool.clone(), Some(test_cipher()));

    let v = created(
        store
            .create(org, secret("api_key", "old"), Some(actor))
            .await
            .unwrap(),
    );
    let old_env = raw_value(&pool, v.id).await;
    store
        .update_value(org, v.id, "new", Some(actor))
        .await
        .unwrap()
        .unwrap();
    let new_env = raw_value(&pool, v.id).await;

    assert_ne!(old_env, new_env, "rotation rewrites the envelope");
    assert!(new_env.starts_with("v1:"));
    assert_eq!(
        store.resolve_map(org).await.unwrap()["api_key"].value,
        "new"
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn tenant_isolation_on_every_method() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, actor) = two_orgs(&pool, "var-iso").await;
    let store = PgVariableStore::new(pool.clone(), Some(test_cipher()));

    let v = created(
        store
            .create(org_a, secret("api_key", "sk-a"), Some(actor))
            .await
            .unwrap(),
    );

    assert!(store.get(org_b, v.id).await.unwrap().is_none());
    assert!(
        store
            .update_value(org_b, v.id, "x", Some(actor))
            .await
            .unwrap()
            .is_none()
    );
    assert!(!store.delete(org_b, v.id, Some(actor)).await.unwrap());
    assert!(store.resolve_map(org_b).await.unwrap().is_empty());

    // Org A's secret is untouched by the cross-tenant attempts.
    assert_eq!(
        store.resolve_map(org_a).await.unwrap()["api_key"].value,
        "sk-a"
    );
    assert!(store.delete(org_a, v.id, Some(actor)).await.unwrap());
    assert!(store.get(org_a, v.id).await.unwrap().is_none());
}

/// The in-memory store backs no-DB fixtures; this guards its semantics against
/// drift from the PG contract without needing a database.
#[tokio::test]
async fn in_memory_store_matches_contract() {
    let store = InMemoryVariableStore::new();
    let org = OrgId(uuid::Uuid::from_u128(1));
    let v = created(
        store
            .create(org, secret("api_key", "sk"), None)
            .await
            .unwrap(),
    );
    assert!(v.value.is_none());
    assert_eq!(store.resolve_map(org).await.unwrap()["api_key"].value, "sk");
    assert!(matches!(
        store
            .create(org, secret("api_key", "dup"), None)
            .await
            .unwrap(),
        CreateVariableOutcome::DuplicateKey
    ));
}
