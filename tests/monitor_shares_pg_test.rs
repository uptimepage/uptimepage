//! Live-Postgres contract for `monitor_shares`: token resolution (active /
//! expired / revoked / deleted-target / deleted-org → None), per-org isolation
//! of create / list / revoke, and the org-match trigger.
//!
//! The in-memory store backs the no-DB harnesses; this suite exercises the real
//! SQL, the unique token_hash, the CASCADE FKs, and the org-match trigger.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Migrations auto-apply on first
//! connect — point it at a throwaway DB to also validate migration 014.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use tower::ServiceExt;
use uptimepage::domain::{
    CheckSpec, CreatedShare, ExpectedStatus, NewMonitorShare, NewStatusPage,
    NewStatusPageComponent, NewTarget, OrgId, UserId, WriteSource,
};
use uptimepage::storage::{
    CreateShareOutcome, MonitorShareStore, PgMonitorShareStore, PgStatusPageStore,
    PostgresTargetStore, StatusPageStore, TargetStore, create_org_with_owner,
};
use uuid::Uuid;

use common::{
    build_saas_router_with_pg_targets, default_http_check, make_user, pg_pool_from_env,
    test_cipher, unique_slug, with_session,
};

fn share(label: Option<&str>) -> NewMonitorShare {
    NewMonitorShare {
        label: label.map(str::to_string),
        expires_at: None,
    }
}

/// Mint a share with generous caps and unwrap the `Created` outcome (the cap
/// tests pass explicit limits instead).
async fn mk_share(
    store: &PgMonitorShareStore,
    org: OrgId,
    target: Uuid,
    new: NewMonitorShare,
) -> CreatedShare {
    match store
        .create(org, target, new, None, Some(i64::MAX), Some(i64::MAX))
        .await
        .unwrap()
    {
        CreateShareOutcome::Created(c) => c,
        o => panic!("expected Created, got {o:?}"),
    }
}

async fn two_orgs(pool: &sqlx::PgPool, tag: &str) -> (OrgId, OrgId, UserId, UserId) {
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
    (org_a, org_b, user_a, user_b)
}

async fn make_target(pool: &sqlx::PgPool, org: OrgId, name: &str) -> Uuid {
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let url = url::Url::parse("https://example.com/").unwrap();
    let nt = NewTarget {
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
    };
    store
        .create(org, nt, WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("create target")
        .id
}

async fn cleanup(pool: &sqlx::PgPool, orgs: &[OrgId], users: &[UserId]) {
    let _ = sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(orgs.iter().map(|o| o.0).collect::<Vec<_>>())
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(users.iter().map(|u| u.0).collect::<Vec<_>>())
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn create_resolve_revoke_roundtrip_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "ms-rt").await;
    let store = PgMonitorShareStore::new(pool.clone(), None);
    let target = make_target(&pool, org_a, "svc").await;

    let created = mk_share(&store, org_a, target, share(Some("slack link"))).await;

    // Resolves to the same monitor + org.
    let resolved = store
        .resolve_active(&created.token)
        .await
        .unwrap()
        .expect("active");
    assert_eq!(resolved.target_id, target);
    assert_eq!(resolved.org, org_a);
    assert_eq!(resolved.share_id, created.share.id);

    // Lists for the monitor (newest first).
    let listed = store.list_for_target(org_a, target).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.share.id);
    assert_eq!(listed[0].label.as_deref(), Some("slack link"));
    assert_eq!(listed[0].view_count, 0);
    assert!(listed[0].last_viewed_at.is_none());
    // Re-copyable: the list hands back the same raw token (no cipher → plaintext).
    assert_eq!(listed[0].token.as_deref(), Some(created.token.as_str()));

    // A recorded view bumps the counter + stamps last_viewed_at.
    store.record_view(created.share.id).await.unwrap();
    store.record_view(created.share.id).await.unwrap();
    let viewed = store.list_for_target(org_a, target).await.unwrap();
    assert_eq!(viewed[0].view_count, 2);
    assert!(viewed[0].last_viewed_at.is_some());

    // A foreign org cannot revoke it…
    assert!(
        !store
            .revoke(org_b, target, created.share.id, None)
            .await
            .unwrap()
    );
    assert!(
        store
            .resolve_active(&created.token)
            .await
            .unwrap()
            .is_some()
    );
    // …nor can a revoke addressed at the wrong monitor in the right org.
    let other = make_target(&pool, org_a, "other svc").await;
    assert!(
        !store
            .revoke(org_a, other, created.share.id, None)
            .await
            .unwrap()
    );
    assert!(
        store
            .resolve_active(&created.token)
            .await
            .unwrap()
            .is_some()
    );

    // …the owner can; after which it resolves to nothing and drops off the list.
    assert!(
        store
            .revoke(org_a, target, created.share.id, None)
            .await
            .unwrap()
    );
    assert!(
        store
            .resolve_active(&created.token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .list_for_target(org_a, target)
            .await
            .unwrap()
            .is_empty()
    );
    // Double-revoke is a clean miss.
    assert!(
        !store
            .revoke(org_a, target, created.share.id, None)
            .await
            .unwrap()
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}

/// A share a status page minted is not one the operator spent quota on, so it
/// must not consume a slot in either cap.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn page_minted_shares_do_not_consume_the_operator_caps_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "ms-pgcap").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("ms-pgcap"), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    let store = PgMonitorShareStore::new(pool.clone(), None);
    let pages = PgStatusPageStore::new(pool.clone());
    let t = make_target(&pool, org, "a").await;

    let page = pages
        .create(
            org,
            NewStatusPage {
                slug: unique_slug("pgcap"),
                name: "P".into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    pages
        .add_component(
            org,
            page.id,
            NewStatusPageComponent {
                target_id: t,
                public_name: None,
                public_description: None,
                public_group: None,
                sort_order: 0,
                detail_link_enabled: true,
            },
            i64::MAX,
            None,
        )
        .await
        .unwrap();

    let CreateShareOutcome::Created(minted) = store
        .create(org, t, share(None), Some(user), None, None)
        .await
        .unwrap()
    else {
        panic!("page mint refused");
    };
    pages
        .attach_share(org, page.id, t, None, minted.share.id)
        .await
        .unwrap();

    // Free-plan shape. The page's link is attached, so the operator still has
    // their own slot on this monitor and on the org.
    assert!(matches!(
        store
            .create(org, t, share(None), Some(user), Some(1), Some(2))
            .await
            .unwrap(),
        CreateShareOutcome::Created(_)
    ));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn create_enforces_per_monitor_and_per_org_caps_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "ms-cap").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("ms-cap"), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    let store = PgMonitorShareStore::new(pool.clone(), None);
    let (a, b, c) = (
        make_target(&pool, org, "a").await,
        make_target(&pool, org, "b").await,
        make_target(&pool, org, "c").await,
    );
    // Free-plan shape: 1 link per monitor, 2 shared monitors per org.
    let mk = |t| store.create(org, t, share(None), Some(user), Some(1), Some(2));

    // Monitor A's first link fits; a second hits the per-monitor cap.
    let first_a = match mk(a).await.unwrap() {
        CreateShareOutcome::Created(c) => c,
        o => panic!("expected Created, got {o:?}"),
    };
    assert!(matches!(
        mk(a).await.unwrap(),
        CreateShareOutcome::PerMonitorLimit
    ));
    // B is the second distinct shared monitor — allowed.
    assert!(matches!(
        mk(b).await.unwrap(),
        CreateShareOutcome::Created(_)
    ));
    // C would be a third — the per-org shared-monitor cap bites.
    assert!(matches!(
        mk(c).await.unwrap(),
        CreateShareOutcome::OrgMonitorLimit
    ));
    // Revoking A's only link drops A as a shared monitor, freeing the org slot.
    assert!(store.revoke(org, a, first_a.share.id, None).await.unwrap());
    assert!(matches!(
        mk(c).await.unwrap(),
        CreateShareOutcome::Created(_)
    ));

    cleanup(&pool, &[org], &[user]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn token_is_encrypted_at_rest_and_recopyable_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "ms-enc").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("ms-enc"), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    let store = PgMonitorShareStore::new(pool.clone(), Some(test_cipher()));
    let target = make_target(&pool, org, "svc").await;

    let created = mk_share(&store, org, target, share(None)).await;

    // The stored column is a Cipher envelope, never the raw token.
    let (token_enc,): (String,) =
        sqlx::query_as("SELECT token_enc FROM monitor_shares WHERE id = $1")
            .bind(created.share.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(uptimepage::security::is_envelope(&token_enc));
    assert!(!token_enc.contains(&created.token));

    // The owner's list decrypts it back to the same raw token (re-copyable).
    let listed = store.list_for_target(org, target).await.unwrap();
    assert_eq!(listed[0].token.as_deref(), Some(created.token.as_str()));
    // The capability still resolves by hash, unchanged by encryption.
    assert!(
        store
            .resolve_active(&created.token)
            .await
            .unwrap()
            .is_some()
    );

    cleanup(&pool, &[org], &[user]).await;
}

/// End-to-end HTTP contract for the operator shares API: create → list →
/// revoke → list through the real router (routing, extractors, CSRF, auth,
/// PG stores). Guards the path the UI drives.
#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn shares_api_create_list_revoke_http_round_trip_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "ms-http").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("ms-http"), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    let target = make_target(&pool, org, "svc").await;
    let router = with_session(
        build_saas_router_with_pg_targets(pool.clone()).await,
        user,
        Some(org),
        Some("ms-http-session"),
    );

    let base = format!("/api/v1/targets/{target}/shares");
    let post = |body: String| {
        Request::post(&base)
            .header("content-type", "application/json")
            .header("X-Requested-With", "uptimepage")
            .body(Body::from(body))
            .unwrap()
    };

    // Create.
    let resp = router.clone().oneshot(post("{}".into())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: serde_json::Value = {
        let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&b).unwrap()
    };
    let share_id = created["id"].as_str().unwrap().to_string();
    assert!(created["token"].as_str().is_some_and(|t| !t.is_empty()));

    // List shows it.
    let listed: serde_json::Value = get_json(&router, &base).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // Free plan allows one link per monitor — a second is a 422.
    let second = router.clone().oneshot(post("{}".into())).await.unwrap();
    assert_eq!(second.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Revoke returns 204 (the bug surfaced as a masked 404).
    let revoke = router
        .clone()
        .oneshot(
            Request::delete(format!("{base}/{share_id}"))
                .header("X-Requested-With", "uptimepage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT, "revoke must 204");

    // List is now empty.
    let after: serde_json::Value = get_json(&router, &base).await;
    assert!(
        after.as_array().unwrap().is_empty(),
        "revoked link must drop off"
    );

    cleanup(&pool, &[org], &[user]).await;
}

async fn get_json(router: &axum::Router, path: &str) -> serde_json::Value {
    let resp = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {path}");
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&b).unwrap()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn expired_and_unknown_tokens_resolve_to_none_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "ms-exp").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("ms-exp"), "O")
        .await
        .unwrap()
        .expect("org")
        .id;
    let store = PgMonitorShareStore::new(pool.clone(), None);
    let target = make_target(&pool, org, "svc").await;

    let expired = NewMonitorShare {
        label: None,
        expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
    };
    let created = mk_share(&store, org, target, expired).await;
    assert!(
        store
            .resolve_active(&created.token)
            .await
            .unwrap()
            .is_none(),
        "expired token must not resolve"
    );
    // A garbage token is indistinguishable (uniform None).
    assert!(
        store
            .resolve_active("not-a-real-token")
            .await
            .unwrap()
            .is_none()
    );

    cleanup(&pool, &[org], &[user]).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn deleted_target_cascades_and_cross_org_create_is_rejected_live_pg() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (org_a, org_b, user_a, user_b) = two_orgs(&pool, "ms-cas").await;
    let store = PgMonitorShareStore::new(pool.clone(), None);
    let target_a = make_target(&pool, org_a, "svc").await;

    // org_b cannot mint a share for org_a's monitor.
    assert!(
        matches!(
            store
                .create(
                    org_b,
                    target_a,
                    share(None),
                    Some(user_b),
                    Some(i64::MAX),
                    Some(i64::MAX)
                )
                .await
                .unwrap(),
            CreateShareOutcome::TargetNotFound
        ),
        "foreign monitor must not yield a cross-tenant share"
    );

    // Deleting the monitor cascades the share away (token then 404s).
    let created = mk_share(&store, org_a, target_a, share(None)).await;
    assert!(
        store
            .resolve_active(&created.token)
            .await
            .unwrap()
            .is_some()
    );
    let target_store = PostgresTargetStore::from_pool(pool.clone(), None);
    assert!(target_store.delete(org_a, target_a, None).await.unwrap());
    assert!(
        store
            .resolve_active(&created.token)
            .await
            .unwrap()
            .is_none(),
        "share must die with its monitor"
    );

    cleanup(&pool, &[org_a, org_b], &[user_a, user_b]).await;
}
