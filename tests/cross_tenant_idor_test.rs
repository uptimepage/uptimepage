//! Cross-tenant IDOR / BOLA regression (acceptance #9). Two SaaS orgs, one
//! target each, one shared `PostgresTargetStore`. A logged-in operator whose
//! active org is A must not be able to read org B's target — by config, by
//! results, by the operator HTML pages — even with B's exact UUID. Every
//! foreign lookup is a 404 (cloak: never confirm the row exists).
//!
//! Live-PG ignored: needs a Postgres pool. The results sink is in-memory, but
//! the per-target results endpoints gate on `target_store.get(org, id)` first,
//! so a foreign UUID is 404 regardless of the results backend.

mod common;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    build_saas_router_with_pg_targets, default_http_check, make_user, unique_slug, with_session,
};
use tower::ServiceExt;
use uptimepage::domain::{CheckSpec, ExpectedStatus, NewStatusPage, NewTarget, WriteSource};
use uptimepage::storage::{
    PgStatusPageStore, PostgresTargetStore, StatusPageStore, TargetStore, create_org_with_owner,
};
use url::Url;

fn a_target() -> NewTarget {
    let url = Url::parse("https://example.com/").unwrap();
    NewTarget {
        name: "secret-target".into(),
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

async fn status_of(router: &Router, path: &str) -> StatusCode {
    router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

/// Issue `method path` with an optional JSON body and return the status. Used
/// to probe the write/delete IDOR surfaces a bare GET can't reach.
async fn method_status(
    router: &Router,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> StatusCode {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header("X-Requested-With", "uptimepage");
    let req = match body {
        Some(json) => builder
            .header("content-type", "application/json")
            .body(Body::from(json.to_owned()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    router.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn logged_in_operator_cannot_read_another_orgs_target() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user_a = make_user(&pool, "idor").await;
    let user_b = make_user(&pool, "idor").await;
    let org_a = create_org_with_owner(&pool, user_a, &unique_slug("idor-a"), "A")
        .await
        .unwrap()
        .expect("org a")
        .id;
    let org_b = create_org_with_owner(&pool, user_b, &unique_slug("idor-b"), "B")
        .await
        .unwrap()
        .expect("org b")
        .id;

    // One shared store; isolation comes from the org argument, exactly as in
    // production (`AppState` holds one store, the request supplies the org).
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let t_a = store
        .create(org_a, a_target(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("create A's target");
    let t_b = store
        .create(org_b, a_target(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("create B's target");

    let router = build_saas_router_with_pg_targets(pool.clone()).await;
    let a = with_session(router.clone(), user_a, Some(org_a), Some("idor-test"));
    let b = with_session(router, user_b, Some(org_b), Some("idor-test"));

    // ── A may read its own target on every per-target surface ────────────
    for path in [
        format!("/api/v1/targets/{}", t_a.id),
        format!("/api/v1/targets/{}/results", t_a.id),
        format!("/api/v1/targets/{}/uptime", t_a.id),
        format!("/api/v1/targets/{}/incidents", t_a.id),
        format!("/targets/{}", t_a.id),
        format!("/targets/{}/edit", t_a.id),
    ] {
        assert_eq!(
            status_of(&a, &path).await,
            StatusCode::OK,
            "A must see its own target at {path}"
        );
    }

    // ── A must NOT read B's target on any surface — 404, not 403 ─────────
    for path in [
        format!("/api/v1/targets/{}", t_b.id),
        format!("/api/v1/targets/{}/results", t_b.id),
        format!("/api/v1/targets/{}/uptime", t_b.id),
        format!("/api/v1/targets/{}/incidents", t_b.id),
        format!("/targets/{}", t_b.id),
        format!("/targets/{}/edit", t_b.id),
    ] {
        assert_eq!(
            status_of(&a, &path).await,
            StatusCode::NOT_FOUND,
            "A must get 404 for B's target at {path}"
        );
    }

    // ── Symmetry: B cannot read A's target either ────────────────────────
    assert_eq!(
        status_of(&b, &format!("/api/v1/targets/{}", t_a.id)).await,
        StatusCode::NOT_FOUND,
        "B must get 404 for A's target"
    );
    assert_eq!(
        status_of(&b, &format!("/api/v1/targets/{}", t_b.id)).await,
        StatusCode::OK,
        "B still sees its own target"
    );

    // The list endpoint must not enumerate the other org's rows.
    let a_list = a
        .clone()
        .oneshot(Request::get("/api/v1/targets").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(a_list.status(), StatusCode::OK);
    let body = axum::body::to_bytes(a_list.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains(&t_a.id.to_string()),
        "A's list includes A's target"
    );
    assert!(
        !body.contains(&t_b.id.to_string()),
        "A's list must not leak B's target id"
    );

    // Teardown: ON DELETE CASCADE wipes targets + memberships.
    let _ = sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(vec![org_a.0, org_b.0])
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(vec![user_a.0, user_b.0])
        .execute(&pool)
        .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn logged_in_operator_cannot_touch_another_orgs_status_page() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let user_a = make_user(&pool, "sp-idor").await;
    let user_b = make_user(&pool, "sp-idor").await;
    let org_a = create_org_with_owner(&pool, user_a, &unique_slug("sp-idor-a"), "A")
        .await
        .unwrap()
        .expect("org a")
        .id;
    let org_b = create_org_with_owner(&pool, user_b, &unique_slug("sp-idor-b"), "B")
        .await
        .unwrap()
        .expect("org b")
        .id;

    // A's page lives in the same pool the router reads from.
    let store = PgStatusPageStore::new(pool.clone());
    let page_a = store
        .create(
            org_a,
            NewStatusPage {
                slug: unique_slug("apage"),
                name: "A page".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create A's page")
        .expect("within page cap");
    let id = page_a.id;

    let router = build_saas_router_with_pg_targets(pool.clone()).await;
    let a = with_session(router.clone(), user_a, Some(org_a), Some("idor-test"));
    let b = with_session(router, user_b, Some(org_b), Some("idor-test"));

    let page_path = format!("/api/v1/status-pages/{id}");
    let comp_path = format!("/api/v1/status-pages/{id}/components");

    // A reaches its own page; B is cloaked with a 404 on every verb.
    assert_eq!(status_of(&a, &page_path).await, StatusCode::OK);
    assert_eq!(
        status_of(&b, &page_path).await,
        StatusCode::NOT_FOUND,
        "B must not READ A's page"
    );
    assert_eq!(
        method_status(&b, "PATCH", &page_path, Some(r#"{"enabled":false}"#)).await,
        StatusCode::NOT_FOUND,
        "B must not UPDATE A's page"
    );
    assert_eq!(
        method_status(&b, "DELETE", &page_path, None).await,
        StatusCode::NOT_FOUND,
        "B must not DELETE A's page"
    );
    // …nor curate it: adding a component to a foreign page is a 404, not a leak.
    assert_eq!(
        method_status(
            &b,
            "POST",
            &comp_path,
            Some(&format!(r#"{{"target_id":"{}"}}"#, uuid::Uuid::new_v4())),
        )
        .await,
        StatusCode::NOT_FOUND,
        "B must not add a component to A's page"
    );

    // The list endpoint must not enumerate A's page for B.
    let b_list = b
        .clone()
        .oneshot(
            Request::get("/api/v1/status-pages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(b_list.status(), StatusCode::OK);
    let body = axum::body::to_bytes(b_list.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&body).contains(&id.to_string()),
        "B's page list must not leak A's page id"
    );

    let _ = sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(vec![org_a.0, org_b.0])
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(vec![user_a.0, user_b.0])
        .execute(&pool)
        .await;
}

/// A non-owner member of an org may READ its status pages but never mutate them
/// — the public brand surface is owner-only. Guards the `OwnerAuthorized` gate.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn non_owner_member_cannot_mutate_status_pages() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };

    let owner = make_user(&pool, "sp-owner").await;
    let member = make_user(&pool, "sp-member").await;
    let org = create_org_with_owner(&pool, owner, &unique_slug("sp-owner"), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    // An active, non-owner membership for the same org.
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(member.0)
        .bind(org.0)
        .execute(&pool)
        .await
        .expect("seed member");

    let page = PgStatusPageStore::new(pool.clone())
        .create(
            org,
            NewStatusPage {
                slug: unique_slug("opage"),
                name: "page".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create page")
        .expect("within page cap");
    let id = page.id;

    let router = build_saas_router_with_pg_targets(pool.clone()).await;
    let m = with_session(router, member, Some(org), Some("sp-member"));
    let page_path = format!("/api/v1/status-pages/{id}");
    let comp_path = format!("/api/v1/status-pages/{id}/components");

    // Read is allowed for any active member.
    assert_eq!(status_of(&m, &page_path).await, StatusCode::OK);
    // Every mutation is owner-only → 403 (not 404 — the member can see it exists).
    assert_eq!(
        method_status(&m, "PATCH", &page_path, Some(r#"{"enabled":false}"#)).await,
        StatusCode::FORBIDDEN,
        "member must not UPDATE the page"
    );
    assert_eq!(
        method_status(&m, "DELETE", &page_path, None).await,
        StatusCode::FORBIDDEN,
        "member must not DELETE the page"
    );
    assert_eq!(
        method_status(
            &m,
            "POST",
            &comp_path,
            Some(&format!(r#"{{"target_id":"{}"}}"#, uuid::Uuid::new_v4())),
        )
        .await,
        StatusCode::FORBIDDEN,
        "member must not curate the page"
    );

    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(vec![owner.0, member.0])
        .execute(&pool)
        .await;
}
