//! Who may open a status page, through the owner's REST API on live Postgres:
//! the plan gate, one PATCH carrying the page and its access together, the
//! validation that keeps a private page from opening for nobody, and the
//! viewer list with block, unblock and history.
//!
//! Live-PG ignored: needs `DATABASE_URL`.

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::{OrgId, PageRef, StatusPageId};
use uptimepage::storage::page_access::viewers;
use uuid::Uuid;

use common::{build_test_app_with_pg_store, pg_pool_from_env, unique_slug};

struct Rig {
    app: Router,
    pool: PgPool,
    org: OrgId,
}

/// An owner on `plan`, in SaaS mode, where the plan decides.
async fn rig(plan: &str) -> Option<Rig> {
    let pool = pg_pool_from_env().await?;
    let (app, org) = build_test_app_with_pg_store(pool.clone(), |cfg| {
        cfg.quotas.enforce_plan_features = true;
    })
    .await;
    sqlx::query(
        "UPDATE accounts SET plan_id = $2 \
         WHERE id = (SELECT account_id FROM organizations WHERE id = $1)",
    )
    .bind(org.0)
    .bind(plan)
    .execute(&pool)
    .await
    .unwrap();
    Some(Rig { app, pool, org })
}

impl Rig {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let text = common::body_text(resp).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn create(&self, access: Option<Value>) -> (StatusCode, Value) {
        let mut body =
            json!({ "slug": unique_slug("acc"), "name": "Autohaus Weber", "enabled": true });
        if let Some(a) = access {
            body["access"] = a;
        }
        self.call(Method::POST, "/api/v1/status-pages", Some(body))
            .await
    }

    async fn cleanup(self) {
        let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(self.org.0)
            .execute(&self.pool)
            .await;
    }
}

fn private(rules: Value) -> Value {
    json!({ "protected": true, "rules": rules })
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_plan_without_private_pages_cannot_make_one() {
    let Some(rig) = rig("free").await else { return };
    let (status, body) = rig
        .create(Some(private(
            json!([{ "kind": "domain", "value": "dealer.example" }]),
        )))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "PRIVATE_PAGES_DISABLED");

    let (status, page) = rig.create(None).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(page["access"], Value::Null);
    let path = format!("/api/v1/status-pages/{}", page["id"].as_str().unwrap());
    let (status, body) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "access": private(json!([{ "kind": "domain", "value": "dealer.example" }])) })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // A public page may still keep a list for later; nothing about it is sold.
    let (status, body) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "access": { "protected": false, "rules": [{ "kind": "domain", "value": "dealer.example" }] } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    rig.cleanup().await;
}

/// A page held because its plan lost private pages must stay editable by
/// clients that send its access back unchanged, as Terraform and MCP do.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_held_private_page_stays_editable() {
    let Some(rig) = rig("pro").await else { return };
    let (_, held) = rig.create(None).await;
    let (_, open) = rig.create(None).await;
    // Made private while the plan still had private pages, then held.
    sqlx::query(
        "WITH a AS (INSERT INTO status_page_access (status_page_id, org_id, protected) \
                    VALUES ($1, $2, true)), \
              h AS (UPDATE status_pages SET plan_hold_at = now() WHERE id = $1) \
         INSERT INTO status_page_access_rules (status_page_id, org_id, kind, value) \
         VALUES ($1, $2, 'domain', 'dealer.example')",
    )
    .bind(Uuid::parse_str(held["id"].as_str().unwrap()).unwrap())
    .bind(rig.org.0)
    .execute(&rig.pool)
    .await
    .unwrap();
    let access = private(json!([{ "kind": "domain", "value": "dealer.example" }]));

    let held = format!("/api/v1/status-pages/{}", held["id"].as_str().unwrap());
    let (status, body) = rig
        .call(
            Method::PATCH,
            &held,
            Some(json!({ "name": "renamed", "access": access.clone() })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "renamed");
    assert_eq!(body["access"]["protected"], true);

    let open = format!("/api/v1/status-pages/{}", open["id"].as_str().unwrap());
    let (status, body) = rig
        .call(Method::PATCH, &open, Some(json!({ "access": access })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "PRIVATE_PAGES_DISABLED");
    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_private_page_is_written_in_one_request_and_reads_back() {
    let Some(rig) = rig("team").await else { return };
    let (status, page) = rig
        .create(Some(json!({
            "protected": true,
            "session_days": 30,
            "email": { "passkeys_enabled": false, "recheck_days": 90 },
            "rules": [
                { "kind": "domain", "value": "DEALER.example" },
                { "kind": "address", "value": "K.Lindner@Example.org" },
                { "kind": "domain", "value": "dealer.example" }
            ]
        })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    assert_eq!(
        page["access"],
        json!({
            "protected": true,
            "session_days": 30,
            "email": { "enabled": true, "passkeys_enabled": false, "recheck_days": 90 },
            "password": { "enabled": false },
            "rules": [
                { "kind": "address", "value": "k.lindner@example.org" },
                { "kind": "domain", "value": "dealer.example" }
            ]
        })
    );
    let id = page["id"].as_str().unwrap();
    let path = format!("/api/v1/status-pages/{id}");

    for (access, field) in [
        (json!({ "protected": true, "rules": [] }), "access.rules"),
        (
            json!({ "protected": true, "rules": [{ "kind": "address", "value": "dealer.example" }] }),
            "access.rules[0]",
        ),
        (
            json!({ "protected": true, "email": { "recheck_days": 14 },
                    "rules": [{ "kind": "domain", "value": "dealer.example" }] }),
            "access.email.recheck_days",
        ),
        (
            json!({ "protected": true, "email": { "enabled": false },
                    "rules": [{ "kind": "domain", "value": "dealer.example" }] }),
            "access.email.enabled",
        ),
        (
            json!({ "protected": true, "session_days": 14,
                    "rules": [{ "kind": "domain", "value": "dealer.example" }] }),
            "access.session_days",
        ),
    ] {
        let (status, body) = rig
            .call(
                Method::PATCH,
                &path,
                Some(json!({ "name": "renamed", "access": access })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "ACCESS_INVALID");
        assert_eq!(body["error"]["field"], field);
    }
    // A refused access write takes the rest of the edit down with it.
    let (_, read) = rig.call(Method::GET, &path, None).await;
    assert_eq!(read["name"], "Autohaus Weber");

    let (status, body) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "access": { "protected": true, "rules": [], "extra": 1 } })),
        )
        .await;
    assert!(status.is_client_error(), "{body}");
    // The session length moved out of `email`; the old spelling is refused.
    let (status, body) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "access": { "protected": true, "email": { "session_days": 30 },
                                     "rules": [{ "kind": "domain", "value": "dealer.example" }] } })),
        )
        .await;
    assert!(status.is_client_error(), "{body}");

    // Public again: the list stays for the next time.
    let (status, body) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "name": "public now", "access": {
                "protected": false, "rules": [{ "kind": "domain", "value": "dealer.example" }] } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "public now");
    assert_eq!(body["access"]["protected"], false);
    assert_eq!(body["access"]["rules"][0]["value"], "dealer.example");

    let (audited,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM org_audit_log \
         WHERE org_id = $1 AND action = 'status_page.access_updated'",
    )
    .bind(rig.org.0)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert_eq!(audited, 2, "the create and the switch back");

    let (_, list) = rig.call(Method::GET, "/api/v1/status-pages", None).await;
    let listed = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .unwrap();
    assert_eq!(listed["access"]["protected"], false);
    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn viewers_are_listed_blocked_unblocked_and_their_history_read() {
    let Some(rig) = rig("team").await else { return };
    let (_, page) = rig
        .create(Some(private(
            json!([{ "kind": "domain", "value": "dealer.example" }]),
        )))
        .await;
    let page_id = Uuid::parse_str(page["id"].as_str().unwrap()).unwrap();
    let page_ref = PageRef {
        page: StatusPageId(page_id),
        org: rig.org,
    };
    let viewer = viewers::record_verified(&rig.pool, page_ref, "anna@dealer.example")
        .await
        .unwrap()
        .unwrap();
    let base = format!("/api/v1/status-pages/{page_id}/access/viewers");

    let (status, list) = rig.call(Method::GET, &base, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["email"], "anna@dealer.example");
    assert_eq!(list[0]["blocked_at"], Value::Null);

    let block = format!("{base}/{}/block", viewer.id);
    assert_eq!(
        rig.call(Method::POST, &block, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        rig.call(Method::POST, &block, None).await.0,
        StatusCode::NO_CONTENT,
        "blocking twice is the same"
    );
    let (_, list) = rig.call(Method::GET, &base, None).await;
    assert_ne!(list[0]["blocked_at"], Value::Null);

    let (status, history) = rig
        .call(Method::GET, &format!("{base}/{}/history", viewer.id), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["events"][0]["kind"], "blocked");
    assert!(history["events"][0]["actor_email"].as_str().is_some());

    let unblock = format!("{base}/{}/unblock", viewer.id);
    assert_eq!(
        rig.call(Method::POST, &unblock, None).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, body) = rig
        .call(
            Method::POST,
            &format!("{base}/{}/block", Uuid::now_v7()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "VIEWER_NOT_FOUND");
    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn another_orgs_viewer_is_not_found() {
    let Some(rig) = rig("team").await else { return };
    let Some(other) = self::rig("team").await else {
        return;
    };
    let (_, theirs) = other
        .create(Some(private(
            json!([{ "kind": "domain", "value": "dealer.example" }]),
        )))
        .await;
    let their_page = Uuid::parse_str(theirs["id"].as_str().unwrap()).unwrap();
    let viewer = viewers::record_verified(
        &other.pool,
        PageRef {
            page: StatusPageId(their_page),
            org: other.org,
        },
        "anna@dealer.example",
    )
    .await
    .unwrap()
    .unwrap();

    let (_, mine) = rig.create(None).await;
    let mine = mine["id"].as_str().unwrap();
    for path in [
        format!("/api/v1/status-pages/{their_page}/access/viewers"),
        format!(
            "/api/v1/status-pages/{mine}/access/viewers/{}/history",
            viewer.id
        ),
    ] {
        assert_eq!(
            rig.call(Method::GET, &path, None).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        rig.call(
            Method::POST,
            &format!(
                "/api/v1/status-pages/{mine}/access/viewers/{}/block",
                viewer.id
            ),
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    other.cleanup().await;
    rig.cleanup().await;
}

fn password_only(value: Option<&str>) -> Value {
    let mut access = json!({
        "protected": true,
        "email": { "enabled": false },
        "password": { "enabled": true },
    });
    if let Some(v) = value {
        access["password"]["value"] = json!(v);
    }
    access
}

async fn stored_password(pool: &PgPool, page: &str) -> (Option<String>, bool) {
    sqlx::query_as(
        "SELECT password_hash, password_set_by IS NOT NULL FROM status_page_access \
         WHERE status_page_id = $1",
    )
    .bind(Uuid::parse_str(page).unwrap())
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_page_password_is_written_once_and_never_read_back() {
    let Some(rig) = rig("team").await else { return };
    let secret = "hunter2 but longer";
    let (status, page) = rig.create(Some(password_only(Some(secret)))).await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    assert_eq!(page["access"]["password"], json!({ "enabled": true }));
    assert!(!page.to_string().contains(secret));
    let id = page["id"].as_str().unwrap().to_owned();
    let path = format!("/api/v1/status-pages/{id}");
    let (hash, set_by) = stored_password(&rig.pool, &id).await;
    let hash = hash.unwrap();
    assert!(hash.starts_with("$argon2id$"), "{hash}");
    assert!(set_by);

    // A GET then a PATCH of what it said keeps the password.
    let (_, read) = rig.call(Method::GET, &path, None).await;
    assert!(!read.to_string().contains(secret));
    let (status, body) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "access": read["access"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stored_password(&rig.pool, &id).await.0.as_deref(),
        Some(hash.as_str())
    );

    // Off keeps it; on again without a value brings it back.
    let mut off = read["access"].clone();
    off["password"]["enabled"] = json!(false);
    off["email"]["enabled"] = json!(true);
    off["rules"] = json!([{ "kind": "domain", "value": "dealer.example" }]);
    let (status, body) = rig
        .call(Method::PATCH, &path, Some(json!({ "access": off })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["access"]["password"]["enabled"], false);
    assert_eq!(
        stored_password(&rig.pool, &id).await.0.as_deref(),
        Some(hash.as_str())
    );
    let (status, body) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "access": password_only(None) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stored_password(&rig.pool, &id).await.0.as_deref(),
        Some(hash.as_str())
    );

    // A one-character password is the owner's call; empty and too long are not.
    let (status, _) = rig
        .call(
            Method::PATCH,
            &path,
            Some(json!({ "access": password_only(Some("1")) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(stored_password(&rig.pool, &id).await.0.unwrap(), hash);
    for bad in [String::new(), "a".repeat(129)] {
        let (status, body) = rig
            .call(
                Method::PATCH,
                &path,
                Some(json!({ "access": password_only(Some(&bad)) })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "INVALID_PAGE_PASSWORD");
        assert_eq!(body["error"]["field"], "access.password.value");
    }

    // The trail says a password was set and changed, never what it is.
    let audit: Vec<(Value,)> = sqlx::query_as(
        "SELECT metadata FROM org_audit_log \
         WHERE org_id = $1 AND action = 'status_page.access_updated' ORDER BY occurred_at, id",
    )
    .bind(rig.org.0)
    .fetch_all(&rig.pool)
    .await
    .unwrap();
    let said: Vec<&Value> = audit.iter().map(|(m,)| &m["password"]).collect();
    assert_eq!(said.first(), Some(&&json!("set")));
    assert_eq!(said.last(), Some(&&json!("changed")));
    for (m,) in &audit {
        assert!(!m.to_string().contains(secret), "{m}");
    }
    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn turning_the_password_on_needs_one_sent_or_stored() {
    let Some(rig) = rig("team").await else { return };
    let (status, body) = rig.create(Some(password_only(None))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "PASSWORD_REQUIRED");
    assert_eq!(body["error"]["field"], "access.password.value");

    // Stored while off, and the way in stays off.
    let (status, page) = rig
        .create(Some(json!({
            "protected": false,
            "password": { "enabled": false, "value": "for later" },
        })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{page}");
    assert_eq!(page["access"]["password"]["enabled"], false);
    let id = page["id"].as_str().unwrap();
    assert!(stored_password(&rig.pool, id).await.0.is_some());
    let (status, body) = rig
        .call(
            Method::PATCH,
            &format!("/api/v1/status-pages/{id}"),
            Some(json!({ "access": password_only(None) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_password_line_counts_its_use_and_signs_everyone_out() {
    let Some(rig) = rig("team").await else { return };
    let (_, page) = rig.create(Some(password_only(Some("shared one")))).await;
    let id = page["id"].as_str().unwrap();
    let page_id = Uuid::parse_str(id).unwrap();
    for (method, ip) in [("password", "a"), ("password", "b")] {
        sqlx::query(
            "INSERT INTO status_page_sessions (id_hash, status_page_id, org_id, method, ip_hash) \
             VALUES (gen_random_uuid()::text, $1, $2, $3, $4)",
        )
        .bind(page_id)
        .bind(rig.org.0)
        .bind(method)
        .bind(ip)
        .execute(&rig.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO status_page_access_events (status_page_id, org_id, kind, method) \
         VALUES ($1, $2, 'password_failed', 'password'), ($1, $2, 'signed_in', 'password')",
    )
    .bind(page_id)
    .bind(rig.org.0)
    .execute(&rig.pool)
    .await
    .unwrap();
    let base = format!("/api/v1/status-pages/{id}/access/password");
    let (status, line) = rig.call(Method::GET, &base, None).await;
    assert_eq!(status, StatusCode::OK, "{line}");
    assert_eq!(line["enabled"], true);
    assert_eq!(line["sessions"], 2);
    assert_eq!(line["failed_last_24h"], 1);
    assert_eq!(line["subscriptions"], 0);
    assert!(line["set_at"].is_string());
    assert!(line["set_by"].is_string());
    assert!(line["last_sign_in_at"].is_string());

    let (status, _) = rig
        .call(Method::POST, &format!("{base}/sign-out"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, line) = rig.call(Method::GET, &base, None).await;
    assert_eq!(line["sessions"], 0);
    // The password still works, so it is not changed.
    assert!(stored_password(&rig.pool, id).await.0.is_some());
    let (ended,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM org_audit_log \
         WHERE org_id = $1 AND action = 'status_page.password_sessions_ended'",
    )
    .bind(rig.org.0)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert_eq!(ended, 1);
    rig.cleanup().await;
}
