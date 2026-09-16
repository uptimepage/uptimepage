//! A create that leaves `owner_user_id` out lands owned by the caller, whoever
//! the caller is: a plain API token and a Terraform-badged one alike. Live-PG
//! ignored: the token path needs a pool.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_saas_router_with_pg_targets, make_user, unique_slug};
use serde_json::{Value, json};
use tower::ServiceExt;
use uptimepage::auth::api_tokens;
use uptimepage::auth::scope::ScopeSet;
use uptimepage::storage::create_org_with_owner;

async fn create_as(router: &axum::Router, token: &str, org: &str, user_agent: &str) -> Value {
    let body = json!({
        "name": "db",
        "check": {"type": "tcp", "host": "db.example.com", "port": 5432, "timeout": 1000},
        "interval": 180
    });
    let resp = router
        .clone()
        .oneshot(
            Request::post("/api/v1/targets")
                .header("authorization", format!("Bearer {token}"))
                .header("x-uptimepage-org", org)
                .header("user-agent", user_agent)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
#[ignore]
async fn token_creates_are_owned_by_the_token_user_whatever_the_client() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "owner").await;
    let slug = unique_slug("owner");
    create_org_with_owner(&pool, user, &slug, "Owner")
        .await
        .unwrap()
        .expect("org");
    let token = api_tokens::create(
        &pool,
        user,
        "ci",
        &ScopeSet::full_access(),
        None,
        None,
        16,
        1000,
    )
    .await
    .unwrap()
    .token;
    let router = build_saas_router_with_pg_targets(pool).await;

    for ua in ["curl/8.0", "terraform-provider-uptimepage/0.11.0"] {
        let created = create_as(&router, &token, &slug, ua).await;
        assert_eq!(
            created["owner_user_id"],
            json!(user.0.to_string()),
            "client {ua}"
        );
    }
}
