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

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use common::{
    build_saas_router_with_pg_cfg, default_http_check, make_user, saas_mcp_host, unique_slug,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
use uptimepage::auth::scope::ScopeSet;
use uptimepage::domain::target::{MAX_TAG_LEN, MAX_TAGS_PER_TARGET};
use uptimepage::domain::{
    CheckSpec, ExpectedStatus, NewMaintenanceWindow, NewStatusPage, NewStatusPageComponent,
    NewTarget, OrgId, StatusPageId, UserId, WriteSource,
};
use uptimepage::storage::{
    MaintenanceStore, PgMaintenanceStore, PgStatusPageStore, PostgresTargetStore, StatusPageStore,
    TargetStore, create_org_with_owner,
};
use url::Url;
use uuid::Uuid;

/// No elicitation capability: a write on the org's own row goes ahead
/// unconfirmed, while a foreign id still fails as `not_found` first.
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
                    .header("host", saas_mcp_host())
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

async fn latest_audit_detail(pool: &PgPool, org: OrgId, tool: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT detail FROM mcp_audit WHERE org_id = $1 AND tool = $2 \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(org.0)
    .bind(tool)
    .fetch_one(pool)
    .await
    .expect("audit row")
}

const UNCONFIRMED: &str = "unconfirmed:no_elicitation";

/// The `{code, message, retryable}` a tool-execution error carries, or `None`
/// when the call succeeded.
fn error_code(result: &Value) -> Option<String> {
    tool_error(result)?["code"].as_str().map(str::to_string)
}

/// The message a tool-execution error carries; empty when the call succeeded.
fn error_message(result: &Value) -> &str {
    tool_error(result)
        .and_then(|e| e["message"].as_str())
        .unwrap_or_default()
}

fn tool_error(result: &Value) -> Option<&Value> {
    let payload = &result["result"];
    (payload["isError"] == Value::Bool(true)).then(|| &payload["structuredContent"]["error"])
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
        recovery_period_secs: None,
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
                acknowledge_button: true,
                resolve_button: false,
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
    let app = mcp_app(pool).await;
    let (org_a, user_a) = seed_org(pool, "mcpa").await;
    let (org_b, _) = seed_org(pool, "mcpb").await;
    let mcp = connector(app, pool, user_a, org_a, &["full_access"]).await;
    (mcp, org_a, org_b)
}

async fn mcp_app(pool: &PgPool) -> Router {
    build_saas_router_with_pg_cfg(pool.clone(), |cfg| {
        cfg.mcp.enabled = true;
    })
    .await
}

/// A session for `user`'s token bound to `org` with `scopes`.
async fn connector(
    app: Router,
    pool: &PgPool,
    user: UserId,
    org: OrgId,
    scopes: &[&str],
) -> Connector {
    let scope_set = ScopeSet::from_strs(scopes.iter().copied());
    assert_eq!(
        scope_set.to_strings().len(),
        scopes.len(),
        "unknown scope in {scopes:?}"
    );
    let created = uptimepage::auth::api_tokens::create(
        pool,
        user,
        "tenancy-probe",
        &scope_set,
        Some(org),
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
                .header("host", saas_mcp_host())
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

    Connector {
        app,
        token: created.token,
        session,
    }
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

#[tokio::test]
#[ignore]
async fn a_write_finds_nothing_to_publish_in_another_org() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, org_b) = connect(&pool).await;
    let (_, a_incident) = seed_monitor_with_incident(&pool, org_a).await;
    let (b_target, b_incident) = seed_monitor_with_incident(&pool, org_b).await;

    let published = mcp
        .call("publish_incident", json!({ "id": a_incident }))
        .await;
    assert_eq!(error_code(&published), None, "{published}");
    assert_eq!(
        latest_audit_detail(&pool, org_a, "publish_incident")
            .await
            .as_deref(),
        Some(UNCONFIRMED)
    );
    for (tool, args) in [
        ("publish_incident", json!({ "id": b_incident })),
        ("unpublish_incident", json!({ "id": b_incident })),
        ("pause_monitor", json!({ "id": b_target })),
        ("resume_monitor", json!({ "id": b_target })),
        (
            "set_monitor_state",
            json!({ "id": b_target, "state": "down" }),
        ),
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

    let editable = store
        .create(org_a, secret_monitor(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("insert ui target")
        .id;
    let retuned = mcp
        .call(
            "update_monitor",
            json!({ "id": editable, "interval_secs": 300 }),
        )
        .await;
    assert_eq!(error_code(&retuned), None, "{retuned}");
    assert_eq!(
        store.get(org_a, editable).await.unwrap().unwrap().interval,
        Duration::from_secs(300)
    );
    assert_eq!(
        latest_audit_detail(&pool, org_a, "update_monitor")
            .await
            .as_deref(),
        Some(UNCONFIRMED)
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

    assert_eq!(
        store.list(org_a, Default::default()).await.unwrap().len(),
        before,
        "no refusal may leave a monitor behind"
    );

    // A heartbeat needs no probe, so no dependence on a live agent.
    let created = mcp
        .call(
            "create_monitor",
            json!({ "name": "nightly", "check": { "type": "heartbeat", "period_secs": 86_400, "grace_secs": 3_600 } }),
        )
        .await;
    assert_eq!(error_code(&created), None, "{created}");
    assert_eq!(
        store.list(org_a, Default::default()).await.unwrap().len(),
        before + 1
    );
    assert_eq!(
        latest_audit_detail(&pool, org_a, "create_monitor")
            .await
            .as_deref(),
        Some(UNCONFIRMED)
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

    // Without a binding that lands, every refusal above could be a broken diff.
    let bound = mcp
        .call(
            "update_monitor",
            json!({ "id": id, "channel_ids": [a_channel] }),
        )
        .await;
    assert_eq!(error_code(&bound), None, "{bound}");
    let after = store.get(org_a, id).await.unwrap().expect("still there");
    assert_eq!(
        after
            .alerts
            .iter()
            .map(|a| a.channel_id)
            .collect::<Vec<_>>(),
        vec![a_channel]
    );
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

async fn seed_monitor(pool: &PgPool, org: OrgId) -> Uuid {
    PostgresTargetStore::from_pool(pool.clone(), None)
        .create(org, secret_monitor(), WriteSource::Ui, i64::MAX, i64::MAX)
        .await
        .expect("insert target")
        .id
}

async fn seed_window(pool: &PgPool, org: OrgId, target: Uuid) -> Uuid {
    let now = chrono::Utc::now();
    PgMaintenanceStore::new(pool.clone())
        .create(
            org,
            NewMaintenanceWindow {
                title: "secret-window".into(),
                description: None,
                starts_at: now + chrono::Duration::hours(1),
                ends_at: now + chrono::Duration::hours(2),
                component_ids: vec![target],
                suppress_alerts: true,
            },
            WriteSource::Ui,
            None,
        )
        .await
        .expect("insert window")
        .id
}

fn window_body(title: &str, from_mins: i64, to_mins: i64, target: Uuid) -> Value {
    let now = chrono::Utc::now();
    json!({
        "title": title,
        "starts_at": (now + chrono::Duration::minutes(from_mins)).to_rfc3339(),
        "ends_at": (now + chrono::Duration::minutes(to_mins)).to_rfc3339(),
        "monitor_ids": [target],
    })
}

#[tokio::test]
#[ignore]
async fn maintenance_tools_cannot_reach_another_orgs_window_or_monitor() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, org_b) = connect(&pool).await;
    let a_target = seed_monitor(&pool, org_a).await;
    let b_target = seed_monitor(&pool, org_b).await;
    let a_window = seed_window(&pool, org_a, a_target).await;
    let b_window = seed_window(&pool, org_b, b_target).await;

    assert_eq!(
        error_code(&mcp.call("get_maintenance", json!({ "id": a_window })).await),
        None
    );
    for (tool, args) in [
        ("get_maintenance", json!({ "id": b_window })),
        (
            "update_maintenance",
            json!({ "id": b_window, "title": "taken" }),
        ),
        ("cancel_maintenance", json!({ "id": b_window })),
    ] {
        assert_eq!(
            error_code(&mcp.call(tool, args).await).as_deref(),
            Some("not_found"),
            "{tool} must not confirm another org's window exists"
        );
    }
    let refused = mcp
        .call("create_maintenance", window_body("x", 60, 120, b_target))
        .await;
    assert_eq!(
        error_code(&refused).as_deref(),
        Some("invalid_argument"),
        "{refused}"
    );

    let listed = mcp.call("list_maintenance", json!({})).await;
    let ids: Vec<&str> = listed["result"]["structuredContent"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&a_window.to_string().as_str()));
    assert!(!ids.contains(&b_window.to_string().as_str()));

    let b = PgMaintenanceStore::new(pool.clone())
        .get(org_b, b_window)
        .await
        .unwrap()
        .expect("still there");
    assert_eq!(b.title, "secret-window");
    assert!(b.deleted_at.is_none());
}

#[tokio::test]
#[ignore]
async fn a_window_is_scheduled_edited_ended_and_cancelled() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, _) = connect(&pool).await;
    let target = seed_monitor(&pool, org_a).await;

    let created = mcp
        .call(
            "create_maintenance",
            window_body("DB upgrade", -5, 60, target),
        )
        .await;
    assert_eq!(error_code(&created), None, "{created}");
    let window = &created["result"]["structuredContent"];
    assert_eq!(window["phase"], "active");
    assert_eq!(window["suppress_alerts"], true);
    assert_eq!(window["monitors"][0]["id"], target.to_string());
    let id = window["id"].as_str().expect("id").to_string();
    assert_eq!(
        latest_audit_detail(&pool, org_a, "create_maintenance")
            .await
            .as_deref(),
        Some(UNCONFIRMED)
    );

    let retitled = mcp
        .call(
            "update_maintenance",
            json!({ "id": id, "title": "DB upgrade, part 2" }),
        )
        .await;
    assert_eq!(error_code(&retitled), None, "{retitled}");
    assert_eq!(
        retitled["result"]["structuredContent"]["changes"][0]["field"],
        "title"
    );

    let ended = mcp
        .call("update_maintenance", json!({ "id": id, "end_now": true }))
        .await;
    assert_eq!(error_code(&ended), None, "{ended}");
    assert_eq!(
        ended["result"]["structuredContent"]["window"]["phase"],
        "completed"
    );
    for (tool, args) in [
        (
            "update_maintenance",
            json!({ "id": id, "title": "too late" }),
        ),
        ("cancel_maintenance", json!({ "id": id })),
    ] {
        assert_eq!(
            error_code(&mcp.call(tool, args).await).as_deref(),
            Some("invalid_argument"),
            "{tool} must refuse a completed window"
        );
    }

    let upcoming = mcp
        .call(
            "create_maintenance",
            window_body("Cache flush", 60, 120, target),
        )
        .await;
    let upcoming_id = upcoming["result"]["structuredContent"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("not created: {upcoming}"))
        .to_string();
    let early = mcp
        .call(
            "update_maintenance",
            json!({ "id": upcoming_id, "end_now": true }),
        )
        .await;
    assert_eq!(error_code(&early).as_deref(), Some("invalid_argument"));

    let cancelled = mcp
        .call("cancel_maintenance", json!({ "id": upcoming_id }))
        .await;
    assert_eq!(error_code(&cancelled), None, "{cancelled}");
    assert_eq!(
        cancelled["result"]["structuredContent"]["phase"],
        "cancelled"
    );
    let row = PgMaintenanceStore::new(pool.clone())
        .get(org_a, upcoming_id.parse().unwrap())
        .await
        .unwrap()
        .expect("kept as history");
    assert!(row.deleted_at.is_some());
    assert_eq!(
        latest_audit_detail(&pool, org_a, "cancel_maintenance")
            .await
            .as_deref(),
        Some(UNCONFIRMED)
    );
}

const SEEDED_PAGE: &str = "Secret Status";

async fn seed_page(pool: &PgPool, org: OrgId, slug: &str) -> StatusPageId {
    PgStatusPageStore::new(pool.clone())
        .create(
            org,
            NewStatusPage {
                slug: slug.into(),
                name: SEEDED_PAGE.into(),
                enabled: true,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("insert page")
        .expect("page created")
        .id
}

/// `(name, enabled)` of the page at `slug`, whichever org holds it.
async fn page_row(pool: &PgPool, slug: &str) -> Option<(String, bool)> {
    sqlx::query_as("SELECT name, enabled FROM status_pages WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// `(monitor, public_name)` of every component on the page at `slug`, in
/// display order.
async fn components(pool: &PgPool, slug: &str) -> Vec<(Uuid, Option<String>)> {
    sqlx::query_as(
        "SELECT c.target_id, c.public_name FROM status_page_components c \
         JOIN status_pages p ON p.id = c.status_page_id \
         WHERE p.slug = $1 ORDER BY c.sort_order",
    )
    .bind(slug)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn status_page_tools_cannot_reach_another_orgs_page_or_monitor() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, org_b) = connect(&pool).await;
    let a_target = seed_monitor(&pool, org_a).await;
    let b_target = seed_monitor(&pool, org_b).await;
    let b_slug = unique_slug("mcpbpage");
    let b_page = seed_page(&pool, org_b, &b_slug).await;
    PgStatusPageStore::new(pool.clone())
        .add_component(
            org_b,
            b_page,
            NewStatusPageComponent {
                target_id: b_target,
                public_name: None,
                public_description: None,
                public_group: None,
                sort_order: 0,
                detail_link_enabled: false,
            },
            i64::MAX,
            None,
        )
        .await
        .expect("component on B's page");

    let taken = mcp
        .call(
            "create_status_page",
            json!({ "slug": b_slug, "name": "Taken" }),
        )
        .await;
    assert_eq!(
        error_code(&taken).as_deref(),
        Some("invalid_argument"),
        "{taken}"
    );
    assert!(error_message(&taken).contains("already taken"), "{taken}");

    for (tool, args) in [
        (
            "update_status_page",
            json!({ "slug": b_slug, "name": "Taken", "enabled": false }),
        ),
        (
            "add_status_page_components",
            json!({ "slug": b_slug, "components": [{ "monitor_id": a_target }] }),
        ),
        (
            "update_status_page_component",
            json!({ "slug": b_slug, "monitor_id": b_target, "public_name": "Taken" }),
        ),
    ] {
        let refused = mcp.call(tool, args).await;
        assert_eq!(
            error_code(&refused).as_deref(),
            Some("not_found"),
            "{tool}: {refused}"
        );
        assert_eq!(
            error_message(&refused),
            "status page not found",
            "{tool} must not resolve another org's page"
        );
    }

    let a_slug = unique_slug("mcpapage");
    let created = mcp
        .call(
            "create_status_page",
            json!({ "slug": a_slug, "name": "Acme" }),
        )
        .await;
    assert_eq!(error_code(&created), None, "{created}");
    let moved = mcp
        .call(
            "update_status_page",
            json!({ "slug": a_slug, "new_slug": b_slug }),
        )
        .await;
    assert_eq!(
        error_code(&moved).as_deref(),
        Some("invalid_argument"),
        "{moved}"
    );
    assert!(error_message(&moved).contains("already taken"), "{moved}");
    assert_eq!(page_row(&pool, &a_slug).await, Some(("Acme".into(), false)));

    let borrowed = mcp
        .call(
            "add_status_page_components",
            json!({ "slug": a_slug, "components": [
                { "monitor_id": b_target },
                { "monitor_id": Uuid::now_v7() },
            ] }),
        )
        .await;
    assert_eq!(error_code(&borrowed), None, "{borrowed}");
    let outcome = &borrowed["result"]["structuredContent"];
    assert_eq!(outcome["added"], 0);
    let results = &outcome["results"];
    assert_eq!(results[0]["outcome"], "failed");
    assert!(results[0]["error"].is_string(), "{borrowed}");
    assert_eq!(
        results[0]["error"], results[1]["error"],
        "another org's monitor must read exactly like one that does not exist"
    );
    assert!(
        components(&pool, &a_slug).await.is_empty(),
        "another org's monitor must never be published on this page"
    );
    let [foreign, missing] = [b_target, Uuid::now_v7()]
        .map(|id| json!({ "slug": a_slug, "monitor_id": id, "public_name": "Taken" }));
    let foreign = mcp.call("update_status_page_component", foreign).await;
    let missing = mcp.call("update_status_page_component", missing).await;
    assert_eq!(
        error_code(&foreign).as_deref(),
        Some("not_found"),
        "{foreign}"
    );
    assert_eq!(
        (error_code(&foreign), error_message(&foreign)),
        (error_code(&missing), error_message(&missing)),
        "another org's monitor must read exactly like one that does not exist"
    );

    assert_eq!(
        page_row(&pool, &b_slug).await,
        Some((SEEDED_PAGE.into(), true))
    );
    assert_eq!(components(&pool, &b_slug).await, vec![(b_target, None)]);
}

#[tokio::test]
#[ignore]
async fn status_pages_are_written_only_by_an_owner_with_the_scope() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let app = mcp_app(&pool).await;
    let (org_a, owner) = seed_org(&pool, "mcpa").await;
    let target = seed_monitor(&pool, org_a).await;
    let slug = unique_slug("mcpown");
    seed_page(&pool, org_a, &slug).await;

    let member = make_user(&pool, "mcpmember").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(member.0)
        .bind(org_a.0)
        .execute(&pool)
        .await
        .expect("seed member");
    let as_member = connector(app.clone(), &pool, member, org_a, &["full_access"]).await;
    let read_only = connector(app, &pool, owner, org_a, &["status_page:read"]).await;

    let new_slug = unique_slug("mcpnew");
    for (mcp, refusal) in [
        (&as_member, "owner-managed"),
        (&read_only, "`status_page:write`"),
    ] {
        for (tool, args) in [
            (
                "create_status_page",
                json!({ "slug": new_slug, "name": "Shadow" }),
            ),
            (
                "update_status_page",
                json!({ "slug": slug, "new_slug": new_slug }),
            ),
            (
                "add_status_page_components",
                json!({ "slug": slug, "components": [{ "monitor_id": target }] }),
            ),
            (
                "update_status_page_component",
                json!({ "slug": slug, "monitor_id": target, "public_name": "Shadow" }),
            ),
        ] {
            let refused = mcp.call(tool, args).await;
            assert_eq!(
                error_code(&refused).as_deref(),
                Some("insufficient_scope"),
                "{tool}"
            );
            assert!(
                error_message(&refused).contains(refusal),
                "{tool}: {refused}"
            );
        }
    }

    assert!(page_row(&pool, &new_slug).await.is_none());
    assert_eq!(
        page_row(&pool, &slug).await,
        Some((SEEDED_PAGE.into(), true))
    );
    assert!(components(&pool, &slug).await.is_empty());
}

#[tokio::test]
#[ignore]
async fn a_page_is_created_curated_and_published() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, _) = connect(&pool).await;
    let [api, web, worker] = [
        seed_monitor(&pool, org_a).await,
        seed_monitor(&pool, org_a).await,
        seed_monitor(&pool, org_a).await,
    ];
    let slug = unique_slug("mcppage");

    for (tool, args) in [
        (
            "create_status_page",
            json!({ "slug": "Not A Slug!", "name": "Acme" }),
        ),
        ("create_status_page", json!({ "slug": slug, "name": "   " })),
    ] {
        let refused = mcp.call(tool, args).await;
        assert_eq!(
            error_code(&refused).as_deref(),
            Some("invalid_argument"),
            "{refused}"
        );
    }

    let created = mcp
        .call(
            "create_status_page",
            json!({ "slug": slug.to_uppercase(), "name": "Acme" }),
        )
        .await;
    assert_eq!(error_code(&created), None, "{created}");
    let page = &created["result"]["structuredContent"];
    assert_eq!(page["slug"], slug.as_str(), "slugs are stored lowercase");
    assert_eq!(page["enabled"], false, "a new page starts unpublished");
    assert!(
        page["public_url"]
            .as_str()
            .unwrap()
            .contains(&format!("{slug}.{}", common::SAAS_BASE_DOMAIN)),
        "{page}"
    );
    assert_eq!(
        latest_audit_detail(&pool, org_a, "create_status_page")
            .await
            .as_deref(),
        Some(UNCONFIRMED)
    );

    for args in [
        json!({ "slug": slug, "components": [] }),
        json!({ "slug": slug, "components": [{ "monitor_id": "not-a-uuid" }] }),
        json!({ "slug": slug, "components": [
            { "monitor_id": api },
            { "monitor_id": web, "public_name": "x".repeat(81) },
        ] }),
    ] {
        let refused = mcp.call("add_status_page_components", args).await;
        assert_eq!(
            error_code(&refused).as_deref(),
            Some("invalid_argument"),
            "{refused}"
        );
    }
    assert!(
        components(&pool, &slug).await.is_empty(),
        "a bad entry anywhere in the batch applies none of it"
    );

    let added = mcp
        .call(
            "add_status_page_components",
            json!({ "slug": slug, "components": [
                { "monitor_id": api, "public_name": "API" },
                { "monitor_id": web },
            ] }),
        )
        .await;
    assert_eq!(error_code(&added), None, "{added}");
    assert_eq!(added["result"]["structuredContent"]["added"], 2);
    let again = mcp
        .call(
            "add_status_page_components",
            json!({ "slug": slug, "components": [
                { "monitor_id": api },
                { "monitor_id": worker },
            ] }),
        )
        .await;
    assert_eq!(error_code(&again), None, "{again}");
    let results = &again["result"]["structuredContent"]["results"];
    assert_eq!(results[0]["outcome"], "already_on_page");
    assert_eq!(results[1]["outcome"], "added");
    assert_eq!(
        components(&pool, &slug).await,
        vec![(api, Some("API".into())), (web, None), (worker, None)],
        "a later batch lands after the components already shown"
    );

    let renamed = mcp
        .call(
            "update_status_page_component",
            json!({ "slug": slug, "monitor_id": web, "public_name": "Website" }),
        )
        .await;
    assert_eq!(error_code(&renamed), None, "{renamed}");
    assert_eq!(
        components(&pool, &slug).await[1].1.as_deref(),
        Some("Website")
    );
    let stranger = seed_monitor(&pool, org_a).await;
    for args in [
        json!({ "slug": slug, "monitor_id": stranger, "public_name": "Nope" }),
        json!({ "slug": unique_slug("nopage"), "monitor_id": web, "public_name": "Nope" }),
    ] {
        let refused = mcp.call("update_status_page_component", args).await;
        assert_eq!(
            error_code(&refused).as_deref(),
            Some("not_found"),
            "{refused}"
        );
    }
    assert_eq!(
        error_code(
            &mcp.call(
                "update_status_page_component",
                json!({ "slug": slug, "monitor_id": web })
            )
            .await
        )
        .as_deref(),
        Some("invalid_argument")
    );

    assert_eq!(
        error_code(
            &mcp.call("update_status_page", json!({ "slug": slug }))
                .await
        )
        .as_deref(),
        Some("invalid_argument"),
        "an update that changes nothing is refused"
    );
    let moved = unique_slug("mcpmoved");
    let published = mcp
        .call(
            "update_status_page",
            json!({ "slug": slug, "name": "Acme Status", "new_slug": moved, "enabled": true }),
        )
        .await;
    assert_eq!(error_code(&published), None, "{published}");
    let page = &published["result"]["structuredContent"];
    assert_eq!(page["slug"], moved.as_str());
    assert_eq!(page["enabled"], true);
    assert_eq!(
        page_row(&pool, &moved).await,
        Some(("Acme Status".into(), true))
    );
    assert!(
        page_row(&pool, &slug).await.is_none(),
        "the old slug is gone"
    );
    assert_eq!(
        error_code(
            &mcp.call(
                "update_status_page",
                json!({ "slug": slug, "enabled": false })
            )
            .await
        )
        .as_deref(),
        Some("not_found")
    );
    assert_eq!(components(&pool, &moved).await.len(), 3);
}

#[tokio::test]
#[ignore]
async fn a_connector_cannot_create_past_the_plans_page_cap() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (mcp, org_a, _) = connect(&pool).await;
    let max = common::plan_for(&pool, org_a).await.max_status_pages;
    for _ in 0..max {
        seed_page(&pool, org_a, &unique_slug("mcpcap")).await;
    }

    let over = unique_slug("mcpover");
    let refused = mcp
        .call(
            "create_status_page",
            json!({ "slug": over, "name": "One more" }),
        )
        .await;

    assert_eq!(
        error_code(&refused).as_deref(),
        Some("invalid_argument"),
        "{refused}"
    );
    assert!(
        error_message(&refused).contains("limit reached"),
        "{refused}"
    );
    assert!(page_row(&pool, &over).await.is_none());
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM status_pages WHERE org_id = $1")
        .bind(org_a.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, i64::from(max));
}
