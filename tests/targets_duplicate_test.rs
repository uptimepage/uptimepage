//! `POST /api/v1/targets/{id}/duplicate` against live Postgres: a copy takes
//! the stored monitor whole, credentials and regions included, and a batch is
//! created entirely or not at all.

use crate::common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uptimepage::domain::{OrgId, UserId};
use uptimepage::storage::create_org_with_owner;
use uuid::Uuid;

struct Rig {
    pool: PgPool,
    db_name: String,
    router: axum::Router,
    user: UserId,
    org: OrgId,
}

impl Rig {
    async fn new(prefix: &str) -> Option<Self> {
        Self::with(prefix, |_| {}).await
    }

    async fn with(
        prefix: &str,
        mutate: impl FnOnce(&mut uptimepage::config::AppConfig),
    ) -> Option<Self> {
        let (db_url, db_name) = common::fresh_test_db(prefix).await?;
        let pool = common::open_test_pool(&db_url).await;
        for region in ["default", "dup-eu", "dup-us"] {
            sqlx::query("INSERT INTO regions (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
                .bind(region)
                .execute(&pool)
                .await
                .unwrap();
        }
        let user = common::make_user(&pool, prefix).await;
        let org = create_org_with_owner(&pool, user, &common::unique_slug("dup"), "Dup")
            .await
            .unwrap()
            .expect("org")
            .id;
        let router = common::with_session(
            common::build_saas_router_with_pg_cfg(pool.clone(), mutate).await,
            user,
            Some(org),
            Some("dup-test"),
        );
        Some(Self {
            pool,
            db_name,
            router,
            user,
            org,
        })
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("X-Requested-With", "uptimepage")
            .body(axum::body::Body::from(
                body.map(|b| b.to_string()).unwrap_or_default(),
            ))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn create(&self, body: Value) -> String {
        let (status, v) = self.send("POST", "/api/v1/targets", Some(body)).await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().to_owned()
    }

    async fn duplicate(&self, id: &str, copies: Value) -> (StatusCode, Value) {
        self.send(
            "POST",
            &format!("/api/v1/targets/{id}/duplicate"),
            Some(json!({ "copies": copies })),
        )
        .await
    }

    async fn regions(&self, id: &str) -> Vec<String> {
        let (status, v) = self
            .send("GET", &format!("/api/v1/targets/{id}/regions"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        serde_json::from_value(v["regions"].clone()).unwrap()
    }

    async fn stored(&self, id: &str) -> (Value, Option<Uuid>) {
        sqlx::query_as::<_, (sqlx::types::Json<Value>, Option<Uuid>)>(
            "SELECT check_spec, escalation_policy_id FROM targets WHERE id = $1",
        )
        .bind(Uuid::parse_str(id).unwrap())
        .fetch_one(&self.pool)
        .await
        .map(|(spec, policy)| (spec.0, policy))
        .unwrap()
    }

    async fn ping_url(&self, id: &str) -> String {
        let (status, v) = self
            .send("GET", &format!("/api/v1/targets/{id}/heartbeat"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        v["ping_url"].as_str().expect("ping url").to_owned()
    }

    async fn monitor_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM targets WHERE org_id = $1")
            .bind(self.org.0)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.pool.close().await;
        common::drop_test_db(&self.db_name).await;
    }
}

fn http_check(url: &str) -> Value {
    json!({
        "type": "http",
        "url": url,
        "method": "POST",
        "timeout": 4000,
        "follow_redirects": true,
        "max_redirects": 3,
        "expected_status": {"kind": "range", "value": {"min": 200, "max": 299}},
        "headers": {"x-api-key": "k-123"},
        "body": "{\"ping\":true}",
        "verify_tls": true,
        "bearer_token": "tok-456",
    })
}

#[tokio::test]
#[ignore]
async fn a_copy_carries_every_setting_and_belongs_to_whoever_made_it() {
    let Some(rig) = Rig::new("dup_carry").await else {
        return;
    };
    let (status, channel) = rig
        .send(
            "POST",
            "/api/v1/notification-channels",
            Some(json!({ "name": "ops", "config": {"type": "webhook", "url": "https://example.com/hook"} })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{channel}");
    let source = rig
        .create(json!({
            "name": "api a",
            "check": http_check("https://a.example.com/health"),
            "interval": 600,
            "enabled": false,
            "tags": ["prod", "api"],
            "group_name": "edge",
            "alert_confirmations": 3,
            "renotify_interval_secs": 600,
            "recovery_period_secs": 120,
            "region_policy": "all",
            "owner_user_id": null,
            "regions": ["dup-eu", "dup-us"],
            "alerts": [{"channel_id": channel["id"]}],
        }))
        .await;
    let (policy,): (Uuid,) = sqlx::query_as(
        "INSERT INTO escalation_policies (org_id, name) VALUES ($1, 'ladder') RETURNING id",
    )
    .bind(rig.org.0)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE targets SET escalation_policy_id = $1 WHERE id = $2")
        .bind(policy)
        .bind(Uuid::parse_str(&source).unwrap())
        .execute(&rig.pool)
        .await
        .unwrap();

    let (status, v) = rig
        .duplicate(
            &source,
            json!([
                {"name": "api b", "address": "https://b.example.com/health"},
                {"name": "api a again"},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let copies = v.as_array().unwrap();
    assert_eq!(copies.len(), 2);
    assert_eq!(copies[0]["name"], "api b");
    assert_eq!(copies[0]["check"]["url"], "https://b.example.com/health");
    assert_eq!(copies[1]["check"]["url"], "https://a.example.com/health");
    assert_eq!(
        copies[0]["check"]["bearer_token"], "***",
        "the response stays redacted"
    );

    for copy in copies {
        let id = copy["id"].as_str().unwrap();
        assert_eq!(
            copy["enabled"], false,
            "a paused source gives paused copies"
        );
        assert_eq!(copy["interval"], 600);
        assert_eq!(copy["tags"], json!(["prod", "api"]));
        assert_eq!(copy["group_name"], "edge");
        assert_eq!(copy["alert_confirmations"], 3);
        assert_eq!(copy["renotify_interval_secs"], 600);
        assert_eq!(copy["recovery_period_secs"], 120);
        assert_eq!(copy["region_policy"], "all");
        assert_eq!(copy["alerts"], json!([{"channel_id": channel["id"]}]));
        assert_eq!(
            copy["owner_user_id"],
            json!(rig.user.0),
            "the copy is the caller's"
        );
        assert_eq!(
            rig.regions(id).await,
            ["dup-eu", "dup-us"].map(String::from)
        );
        let (spec, escalation) = rig.stored(id).await;
        assert_eq!(spec["bearer_token"], "tok-456");
        assert_eq!(spec["headers"]["x-api-key"], "k-123");
        assert_eq!(spec["body"], "{\"ping\":true}");
        assert_eq!(spec["method"], "POST");
        assert_eq!(escalation, Some(policy));
    }
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn copies_are_all_or_nothing_at_the_plan_cap() {
    let Some(rig) = Rig::new("dup_cap").await else {
        return;
    };
    common::set_account_targets_cap(&rig.pool, rig.user, 2).await;
    let source = rig
        .create(
            json!({"name": "api", "check": http_check("https://a.example.com/"), "interval": 300}),
        )
        .await;
    let before = rig.monitor_count().await;

    let (status, v) = rig
        .duplicate(
            &source,
            json!([
                {"name": "b", "address": "https://b.example.com/"},
                {"name": "c", "address": "https://c.example.com/"},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"]["details"]["quota"], "max_targets");
    assert_eq!(rig.monitor_count().await, before);
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_refused_copy_is_named_and_nothing_is_created() {
    let Some(rig) = Rig::new("dup_refused").await else {
        return;
    };
    let source = rig
        .create(
            json!({"name": "api", "check": http_check("https://a.example.com/"), "interval": 300}),
        )
        .await;
    let before = rig.monitor_count().await;

    let (status, v) = rig
        .duplicate(
            &source,
            json!([
                {"name": "b", "address": "https://b.example.com/"},
                {"name": "c", "address": "c.example.com"},
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ADDRESS");
    assert_eq!(v["error"]["field"], "copies[1].address");

    let (status, v) = rig
        .duplicate(
            &source,
            json!([{"name": "  ", "address": "https://b.example.com/"}]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["field"], "copies[0].name");

    let (status, v) = rig
        .duplicate(
            &source,
            json!([{"name": "d", "address": "ftp://d.example.com/"}]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["field"], "copies[0].check.url");

    let (status, v) = rig.duplicate(&source, json!([])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(rig.monitor_count().await, before);
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn another_orgs_monitor_cannot_be_copied() {
    let Some(rig) = Rig::new("dup_tenant").await else {
        return;
    };
    let source = rig
        .create(
            json!({"name": "api", "check": http_check("https://a.example.com/"), "interval": 300}),
        )
        .await;
    let stranger = common::make_user(&rig.pool, "dup_stranger").await;
    let their_org = create_org_with_owner(&rig.pool, stranger, &common::unique_slug("dupx"), "X")
        .await
        .unwrap()
        .expect("org")
        .id;
    let theirs = Rig {
        router: common::with_session(
            common::build_saas_router_with_pg_targets(rig.pool.clone()).await,
            stranger,
            Some(their_org),
            Some("dup-stranger"),
        ),
        pool: rig.pool.clone(),
        db_name: String::new(),
        user: stranger,
        org: their_org,
    };

    let (status, v) = theirs
        .duplicate(&source, json!([{"name": "mine now"}]))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(theirs.monitor_count().await, 0);
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_heartbeat_copy_gets_its_own_ping_url() {
    let Some(rig) = Rig::new("dup_heartbeat").await else {
        return;
    };
    let source = rig
        .create(json!({
            "name": "nightly",
            "check": {"type": "heartbeat", "period": 3_600_000, "grace": 300_000},
            "interval": 180,
        }))
        .await;

    let (status, v) = rig
        .duplicate(
            &source,
            json!([{"name": "nightly b", "address": "x.example.com"}]),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["field"], "copies[0].address");

    let (status, v) = rig.duplicate(&source, json!([{"name": "nightly b"}])).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let copy = v[0]["id"].as_str().unwrap();
    assert_ne!(rig.ping_url(&source).await, rig.ping_url(copy).await);
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn without_on_call_the_copies_alert_their_channels_only() {
    // The hosted service sells on-call by plan; a free account has none.
    let Some(rig) = Rig::with("dup_oncall", |cfg| cfg.quotas.enforce_plan_features = true).await
    else {
        return;
    };
    let source = rig
        .create(
            json!({"name": "api", "check": http_check("https://a.example.com/"), "interval": 300}),
        )
        .await;
    let (policy,): (Uuid,) = sqlx::query_as(
        "INSERT INTO escalation_policies (org_id, name) VALUES ($1, 'ladder') RETURNING id",
    )
    .bind(rig.org.0)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE targets SET escalation_policy_id = $1 WHERE id = $2")
        .bind(policy)
        .bind(Uuid::parse_str(&source).unwrap())
        .execute(&rig.pool)
        .await
        .unwrap();

    let (status, v) = rig
        .duplicate(
            &source,
            json!([{"name": "b", "address": "https://b.example.com/"}]),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (_, escalation) = rig.stored(v[0]["id"].as_str().unwrap()).await;
    assert_eq!(escalation, None);
    let (_, kept) = rig.stored(&source).await;
    assert_eq!(kept, Some(policy), "the source stays on its ladder");
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn a_flow_longer_than_the_plan_allows_is_not_copied() {
    let Some(rig) = Rig::new("dup_flow").await else {
        return;
    };
    sqlx::query("UPDATE plans SET max_flow_checks = 5, max_flow_steps = 2")
        .execute(&rig.pool)
        .await
        .unwrap();
    let flow = json!({
        "type": "flow",
        "start_url": "https://a.example.com/login",
        "steps": [
            {"op": "goto", "url": "https://a.example.com/login"},
            {"op": "click", "selector": "#sign-in"},
            {"op": "wait_for", "selector": "#home"},
        ],
        "timeout": 30000,
        "step_timeout": 5000,
        "verify_tls": true,
    });
    let (source,): (Uuid,) = sqlx::query_as(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
         VALUES ($1, 'login', $2::jsonb, 300, true) RETURNING id",
    )
    .bind(rig.org.0)
    .bind(flow)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    let before = rig.monitor_count().await;

    let (status, v) = rig
        .duplicate(&source.to_string(), json!([{"name": "login b"}]))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_FLOW_PARAMS");
    assert_eq!(rig.monitor_count().await, before);
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn more_than_a_hundred_copies_is_refused() {
    let Some(rig) = Rig::new("dup_max").await else {
        return;
    };
    let source = rig
        .create(
            json!({"name": "api", "check": http_check("https://a.example.com/"), "interval": 300}),
        )
        .await;
    let copies: Vec<Value> = (0..101)
        .map(|i| json!({"name": format!("api {i}")}))
        .collect();
    let before = rig.monitor_count().await;

    let (status, v) = rig.duplicate(&source, json!(copies)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["error"]["code"], "BULK_TOO_LARGE");
    assert_eq!(rig.monitor_count().await, before);
    rig.finish().await;
}

#[tokio::test]
#[ignore]
async fn only_the_owner_puts_copies_on_a_ladder() {
    let Some(rig) = Rig::new("dup_member").await else {
        return;
    };
    let source = rig
        .create(
            json!({"name": "api", "check": http_check("https://a.example.com/"), "interval": 300}),
        )
        .await;
    let (policy,): (Uuid,) = sqlx::query_as(
        "INSERT INTO escalation_policies (org_id, name) VALUES ($1, 'ladder') RETURNING id",
    )
    .bind(rig.org.0)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE targets SET escalation_policy_id = $1 WHERE id = $2")
        .bind(policy)
        .bind(Uuid::parse_str(&source).unwrap())
        .execute(&rig.pool)
        .await
        .unwrap();
    let member = common::make_user(&rig.pool, "dup_member").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(member.0)
        .bind(rig.org.0)
        .execute(&rig.pool)
        .await
        .unwrap();
    let as_member = Rig {
        router: common::with_session(
            common::build_saas_router_with_pg_targets(rig.pool.clone()).await,
            member,
            Some(rig.org),
            Some("dup-member"),
        ),
        pool: rig.pool.clone(),
        db_name: String::new(),
        user: member,
        org: rig.org,
    };

    let (status, v) = as_member
        .duplicate(
            &source,
            json!([{"name": "b", "address": "https://b.example.com/"}]),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (_, escalation) = rig.stored(v[0]["id"].as_str().unwrap()).await;
    assert_eq!(escalation, None, "a member cannot bind a ladder");

    let (status, v) = rig
        .duplicate(
            &source,
            json!([{"name": "c", "address": "https://c.example.com/"}]),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    let (_, escalation) = rig.stored(v[0]["id"].as_str().unwrap()).await;
    assert_eq!(escalation, Some(policy), "the owner can");
    rig.finish().await;
}
