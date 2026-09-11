//! Cross-tenant and write-guard regressions for the MCP front door. Two orgs
//! share one router and one set of Postgres-backed stores; the connector holds
//! an org-bound token for A. Org B's monitor and incident ids must read as
//! `not_found` through every tool, and no write may act on them. Write guards
//! that run ahead of the confirmation gate are covered here too.
//!
//! The other cross-tenant suites drive `/api/v1` and the operator HTML. This
//! one drives JSON-RPC `tools/call`, because MCP resolves its org from the
//! token rather than from a session or a path.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` with `DATABASE_URL`
//! set. The harness auto-applies migrations on first connect.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use common::{build_saas_router_with_pg_cfg, default_http_check, make_user, unique_slug};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
use uptimepage::auth::scope::ScopeSet;
use uptimepage::domain::target::{MAX_TAG_LEN, MAX_TAGS_PER_TARGET};
use uptimepage::domain::{CheckSpec, ExpectedStatus, NewTarget, OrgId, UserId, WriteSource};
use uptimepage::storage::{PostgresTargetStore, TargetStore, create_org_with_owner};
use url::Url;
use uuid::Uuid;

/// A client that declares no elicitation capability: every write tool refuses
/// at the confirmation gate, which is exactly what makes the contrast in
/// `a_write_finds_nothing_to_publish_in_another_org` legible. The org checks
/// run before that gate, so a foreign id still has to fail as `not_found`.
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"tenancy-probe","version":"0"}}}"#;

struct Connector {
    app: Router,
    token: String,
    session: String,
}

impl Connector {
    async fn call(&self, tool: &str, arguments: Value) -> Value {
        let body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        }))
        .unwrap();
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("host", "localhost")
                    .header("accept", "application/json, text/event-stream")
                    .header("authorization", format!("Bearer {}", self.token))
                    .header("mcp-session-id", &self.session)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{tool}: {}", resp.status());
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        let frame = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_else(|| panic!("no data frame for {tool}: {text:?}"));
        serde_json::from_str(frame).expect("tool result json")
    }
}

/// The `{code, message, retryable}` a tool-execution error carries, or `None`
/// when the call succeeded.
fn error_code(result: &Value) -> Option<String> {
    let payload = &result["result"];
    if payload["isError"] != Value::Bool(true) {
        return None;
    }
    payload["structuredContent"]["error"]["code"]
        .as_str()
        .map(str::to_string)
}

fn secret_monitor() -> NewTarget {
    let url = Url::parse("https://example.com/").unwrap();
    NewTarget {
        name: "secret-monitor".into(),
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

async fn seed_org(pool: &PgPool, prefix: &str) -> (OrgId, UserId) {
    let user = make_user(pool, prefix).await;
    let org = create_org_with_owner(pool, user, &unique_slug(prefix), "svc")
        .await
        .expect("create org")
        .expect("org created")
        .id;
    (org, user)
}

async fn seed_monitor_with_incident(pool: &PgPool, org: OrgId) -> (Uuid, Uuid) {
    // Through the store, so the row carries a `check_spec` the read path can
    // actually decode.
    let target_id = PostgresTargetStore::from_pool(pool.clone(), None)
        .create(org, secret_monitor(), WriteSource::Api, i64::MAX, i64::MAX)
        .await
        .expect("insert target")
        .id;
    let incident_id: Uuid = sqlx::query_scalar(
        "INSERT INTO incidents (org_id, target_id, started_at, status_at_start, check_count, \
                                state, visibility, origin) \
         VALUES ($1, $2, now() - interval '10 minute', 'down', 1, 'triggered', 'internal', \
                 'monitor') RETURNING id",
    )
    .bind(org.0)
    .bind(target_id)
    .fetch_one(pool)
    .await
    .expect("insert incident");
    (target_id, incident_id)
}

async fn seed_tagged_monitor(pool: &PgPool, org: OrgId, tag: &str) {
    let mut target = secret_monitor();
    target.name = format!("tagged-{tag}");
    target.tags = vec![tag.to_string()];
    PostgresTargetStore::from_pool(pool.clone(), None)
        .create(org, target, WriteSource::Api, i64::MAX, i64::MAX)
        .await
        .expect("insert tagged target");
}

async fn seed_channel(pool: &PgPool, org: OrgId, name: &str) -> Uuid {
    use uptimepage::domain::notification_channel::{
        ChannelConfig, NewNotificationChannel, SlackConfig,
    };
    use uptimepage::storage::NotificationChannelStore;
    uptimepage::storage::PgNotificationChannelStore::new(pool.clone(), None)
        .create(
            org,
            NewNotificationChannel {
                name: name.to_string(),
                config: ChannelConfig::Slack(SlackConfig {
                    webhook_url: "https://hooks.slack.example/T/B/x".into(),
                    mention: None,
                }),
                enabled: true,
                auto_bind_tags: Vec::new(),
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("insert channel")
        .id
}

/// Router with `/mcp` mounted, two orgs, and a connector bound to the first.
async fn connect(pool: &PgPool) -> (Connector, OrgId, OrgId) {
    let app = build_saas_router_with_pg_cfg(pool.clone(), |cfg| {
        cfg.mcp.enabled = true;
    })
    .await;
    let (org_a, user_a) = seed_org(pool, "mcpa").await;
    let (org_b, _) = seed_org(pool, "mcpb").await;

    let created = uptimepage::auth::api_tokens::create(
        pool,
        user_a,
        "tenancy-probe",
        &ScopeSet::from_strs(["full_access"]),
        Some(org_a),
        None,
        16,
        10,
    )
    .await
    .expect("mint token");

    let init = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("host", "localhost")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", format!("Bearer {}", created.token))
                .body(Body::from(INITIALIZE))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(init.status().is_success(), "initialize: {}", init.status());
    let session = init
        .headers()
        .get("mcp-session-id")
        .expect("session id")
        .to_str()
        .unwrap()
        .to_string();

    (
        Connector {
            app,
            token: created.token,
            session,
        },
        org_a,
        org_b,
    )
}

#[tokio::test]
#[ignore]
async fn reads_cannot_reach_another_orgs_monitor_or_incident() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, org_b) = connect(&pool).await;
    let (a_target, a_incident) = seed_monitor_with_incident(&pool, org_a).await;
    let (b_target, b_incident) = seed_monitor_with_incident(&pool, org_b).await;

    // The connector's own rows are readable, so a `not_found` below is tenancy
    // and not a broken fixture.
    assert_eq!(
        error_code(&mcp.call("get_monitor", json!({ "id": a_target })).await),
        None
    );
    assert_eq!(
        error_code(&mcp.call("get_incident", json!({ "id": a_incident })).await),
        None
    );

    for (tool, args) in [
        ("get_monitor", json!({ "id": b_target })),
        (
            "get_monitor_history",
            json!({ "id": b_target, "window": "24h" }),
        ),
        ("get_flow_runs", json!({ "id": b_target, "window": "24h" })),
        ("get_incident", json!({ "id": b_incident })),
    ] {
        assert_eq!(
            error_code(&mcp.call(tool, args).await).as_deref(),
            Some("not_found"),
            "{tool} must not confirm another org's row exists"
        );
    }
}

#[tokio::test]
#[ignore]
async fn listings_carry_only_the_tokens_own_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, org_b) = connect(&pool).await;
    let (a_target, a_incident) = seed_monitor_with_incident(&pool, org_a).await;
    let (b_target, b_incident) = seed_monitor_with_incident(&pool, org_b).await;

    let monitors = mcp.call("list_monitors", json!({})).await;
    let ids: Vec<&str> = monitors["result"]["structuredContent"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&a_target.to_string().as_str()));
    assert!(!ids.contains(&b_target.to_string().as_str()));

    // `state: all` widens the window, which is the read that reaches furthest
    // back; it must not reach sideways.
    let incidents = mcp.call("list_incidents", json!({ "state": "all" })).await;
    let ids: Vec<&str> = incidents["result"]["structuredContent"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&a_incident.to_string().as_str()));
    assert!(!ids.contains(&b_incident.to_string().as_str()));

    let health = mcp.call("get_org_health", json!({})).await;
    let worst = &health["result"]["structuredContent"]["worst"];
    assert!(
        !worst.to_string().contains(&b_target.to_string()),
        "org health leaked a foreign monitor: {worst}"
    );

    // The tag inventory aggregates across the whole org and takes no id to
    // scope it, so it is a tenancy surface of its own.
    seed_tagged_monitor(&pool, org_a, "mcp-tenancy-a").await;
    seed_tagged_monitor(&pool, org_b, "mcp-tenancy-b").await;
    let tags = mcp.call("list_tags", json!({})).await;
    let names: Vec<&str> = tags["result"]["structuredContent"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"mcp-tenancy-a"), "{names:?}");
    assert!(!names.contains(&"mcp-tenancy-b"), "{names:?}");
}

/// The org lookup runs before the confirmation gate, so the two failures are
/// distinguishable: A's own incident gets as far as asking the user (and this
/// client can't answer), while B's never resolves to a row at all.
#[tokio::test]
#[ignore]
async fn a_write_finds_nothing_to_publish_in_another_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, org_b) = connect(&pool).await;
    let (_, a_incident) = seed_monitor_with_incident(&pool, org_a).await;
    let (b_target, b_incident) = seed_monitor_with_incident(&pool, org_b).await;

    assert_eq!(
        error_code(
            &mcp.call("publish_incident", json!({ "id": a_incident }))
                .await
        )
        .as_deref(),
        Some("elicitation_unsupported"),
    );
    for (tool, args) in [
        ("publish_incident", json!({ "id": b_incident })),
        ("unpublish_incident", json!({ "id": b_incident })),
        ("pause_monitor", json!({ "id": b_target })),
        ("resume_monitor", json!({ "id": b_target })),
    ] {
        assert_eq!(
            error_code(&mcp.call(tool, args).await).as_deref(),
            Some("not_found"),
            "{tool} must not act on another org's row"
        );
    }

    // Nothing above may have changed B's row on its way to failing.
    let (visibility, enabled): (String, bool) = sqlx::query_as(
        "SELECT i.visibility::text, t.enabled FROM incidents i \
         JOIN targets t ON t.id = i.target_id WHERE i.id = $1",
    )
    .bind(b_incident)
    .fetch_one(&pool)
    .await
    .expect("read back org B");
    assert_eq!(visibility, "internal");
    assert!(enabled);
}

/// The Terraform refusal runs before the confirmation gate, so a client that
/// cannot confirm still hits it.
#[tokio::test]
#[ignore]
async fn no_write_touches_a_terraform_managed_monitor() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, _) = connect(&pool).await;
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let declared = store
        .create(
            org_a,
            secret_monitor(),
            WriteSource::Terraform,
            i64::MAX,
            i64::MAX,
        )
        .await
        .expect("insert terraform-managed target")
        .id;

    for (tool, args) in [
        (
            "update_monitor",
            json!({ "id": declared, "interval_secs": 300 }),
        ),
        ("pause_monitor", json!({ "id": declared })),
        ("resume_monitor", json!({ "id": declared })),
    ] {
        assert_eq!(
            error_code(&mcp.call(tool, args).await).as_deref(),
            Some("managed_externally"),
            "{tool} must refuse a Terraform-managed monitor"
        );
    }

    // An editable monitor gets as far as asking, which this client cannot answer.
    let editable = store
        .create(org_a, secret_monitor(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("insert ui target")
        .id;
    assert_eq!(
        error_code(
            &mcp.call(
                "update_monitor",
                json!({ "id": editable, "interval_secs": 300 })
            )
            .await
        )
        .as_deref(),
        Some("elicitation_unsupported"),
    );

    // Nothing above may have changed the declared row on its way to failing.
    let after = store
        .get(org_a, declared)
        .await
        .unwrap()
        .expect("still there");
    assert_eq!(after.interval, Duration::from_secs(30));
    assert!(after.enabled);
    assert_eq!(after.write_source, WriteSource::Terraform);
}

/// A field outside the allowlist must fail loudly, not be dropped by serde.
#[tokio::test]
#[ignore]
async fn an_uneditable_field_is_refused_rather_than_ignored() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, _) = connect(&pool).await;
    let id = PostgresTargetStore::from_pool(pool.clone(), None)
        .create(org_a, secret_monitor(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("insert target")
        .id;

    let refused = mcp
        .call(
            "update_monitor",
            json!({ "id": id, "name": "renamed", "interval_secs": 300 }),
        )
        .await;
    assert!(
        refused["result"]["isError"] == Value::Bool(true) || refused["error"] != Value::Null,
        "an unknown field must not be dropped: {refused}"
    );
    let unchanged = PostgresTargetStore::from_pool(pool.clone(), None)
        .get(org_a, id)
        .await
        .unwrap()
        .expect("still there");
    assert_eq!(unchanged.name, "secret-monitor");
    assert_eq!(unchanged.interval, Duration::from_secs(30));
}

/// Creation is vetted and then tried before it can persist, so both gates run
/// ahead of the confirmation and none of them leaves a monitor behind.
#[tokio::test]
#[ignore]
async fn a_monitor_is_not_created_when_the_check_is_refused_or_cannot_be_tried() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, _) = connect(&pool).await;
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let before = store.list(org_a, Default::default()).await.unwrap().len();

    for (case, args) in [
        (
            "a url that does not parse",
            json!({ "name": "bad url", "check": { "type": "http", "url": "not-a-url" } }),
        ),
        (
            "credentials in the url",
            json!({ "name": "creds", "check": { "type": "http", "url": "https://u:p@example.com/" } }),
        ),
        (
            "a second count too wide for the column",
            json!({ "name": "wide", "check": { "type": "http", "url": "https://example.com/" }, "interval_secs": 4_294_967_356u64 }),
        ),
        (
            "a region the fleet does not serve",
            json!({ "name": "nowhere", "check": { "type": "http", "url": "https://example.com/" }, "regions": ["atlantis"] }),
        ),
        (
            "an empty region set",
            json!({ "name": "empty regions", "check": { "type": "http", "url": "https://example.com/" }, "regions": [] }),
        ),
        (
            "regions on a monitor that is pinged rather than probed",
            json!({ "name": "nightly", "check": { "type": "heartbeat", "period_secs": 86_400, "grace_secs": 3_600 }, "regions": ["eu-helsinki"] }),
        ),
    ] {
        let refused = mcp.call("create_monitor", args).await;
        assert_eq!(
            error_code(&refused).as_deref(),
            Some("invalid_argument"),
            "{case}: {refused}"
        );
    }

    // This harness never negotiated elicitation. The refusal must come from
    // that, before any probe is dispatched at a caller-supplied address —
    // asserting `probe_unavailable` here would pin the suite to whether a live
    // agent happens to serve the region.
    let untried = mcp
        .call(
            "create_monitor",
            json!({ "name": "shop", "check": { "type": "http", "url": "https://example.com/" } }),
        )
        .await;
    assert_eq!(
        error_code(&untried).as_deref(),
        Some("elicitation_unsupported"),
        "{untried}"
    );

    assert_eq!(
        store.list(org_a, Default::default()).await.unwrap().len(),
        before,
        "no refusal may leave a monitor behind"
    );
}

/// A channel id is only bindable by the org that owns it, and a missing one is
/// refused rather than silently dropped from the binding set.
#[tokio::test]
#[ignore]
async fn a_channel_from_another_org_cannot_be_bound() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, org_b) = connect(&pool).await;
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let id = store
        .create(org_a, secret_monitor(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("insert target")
        .id;
    let b_channel = seed_channel(&pool, org_b, "b-slack").await;

    let a_channel = seed_channel(&pool, org_a, "a-slack").await;

    for (case, ids) in [
        ("another org's channel", vec![b_channel]),
        ("a channel that does not exist", vec![Uuid::now_v7()]),
        ("the same channel twice", vec![a_channel, a_channel]),
    ] {
        let refused = mcp
            .call("update_monitor", json!({ "id": id, "channel_ids": ids }))
            .await;
        assert_eq!(
            error_code(&refused).as_deref(),
            Some("invalid_argument"),
            "{case}: {refused}"
        );
    }

    // The org's own channel gets as far as the confirmation this client cannot
    // answer. Without this the suite would pass on a binding that can never
    // succeed, since every refusal above looks the same as a broken diff.
    let asked = mcp
        .call(
            "update_monitor",
            json!({ "id": id, "channel_ids": [a_channel] }),
        )
        .await;
    assert_eq!(
        error_code(&asked).as_deref(),
        Some("elicitation_unsupported"),
        "{asked}"
    );

    let unchanged = store.get(org_a, id).await.unwrap().expect("still there");
    assert!(unchanged.alerts.is_empty(), "{:?}", unchanged.alerts);
}

/// The tag bounds hold at the MCP door, ahead of the confirmation gate.
#[tokio::test]
#[ignore]
async fn a_tag_the_shared_validator_rejects_never_reaches_the_prompt() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, _) = connect(&pool).await;
    let store = PostgresTargetStore::from_pool(pool.clone(), None);
    let id = store
        .create(org_a, secret_monitor(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("insert target")
        .id;

    let over_cap: Vec<String> = (0..=MAX_TAGS_PER_TARGET).map(|i| format!("t{i}")).collect();
    for tags in [
        json!(over_cap),
        json!(["x".repeat(MAX_TAG_LEN + 1)]),
        json!(["   "]),
        json!(["prod\u{202e}ignore"]),
    ] {
        let refused = mcp
            .call("update_monitor", json!({ "id": id, "tags": tags }))
            .await;
        assert_eq!(
            error_code(&refused).as_deref(),
            Some("invalid_argument"),
            "{refused}"
        );
    }

    let unchanged = store.get(org_a, id).await.unwrap().expect("still there");
    assert!(unchanged.tags.is_empty(), "{:?}", unchanged.tags);
}
