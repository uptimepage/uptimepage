//! The alert sources API: create, read, change, rotate and delete; which
//! scopes see the ingest URL; the plan gate and cap; the owner rule for a
//! policy; holds on a smaller plan; the account's event budget; and what an
//! alert incident shows on the incidents API.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::common;
use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::DurationRound;
use common::{body_json, make_user, unique_slug};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::auth::api_tokens;
use uptimepage::auth::scope::ScopeSet;
use uptimepage::domain::{AccountId, NotificationReason, OrgId, UserId};
use uptimepage::escalation::IncidentSignal;
use uptimepage::quotas::holds::reconcile_after_change;
use uptimepage::storage::alert_ingest::Status;
use uptimepage::storage::create_org_with_owner;
use uptimepage::storage::traits::{
    AlertEventCounts, AlertEventHour, AlertEventLog, AlertEventView,
};
use uuid::Uuid;

struct Fx {
    pool: PgPool,
    org: OrgId,
    slug: String,
    account: AccountId,
    owner: UserId,
    /// A full-access token of the org's owner.
    token: String,
    channel: Uuid,
    state: AppState,
    router: Router,
    signals: mpsc::Receiver<IncidentSignal>,
}

/// A plan like Team with `patch` applied, which the org's account moves to.
/// A new account starts on Free, which has no alert sources.
async fn plan_like_team(pool: &PgPool, account: AccountId, mut patch: Value) {
    let id = unique_slug("plan");
    let fields = patch.as_object_mut().expect("a patch is an object");
    fields.insert("id".into(), json!(id));
    fields.insert("is_listed".into(), json!(false));
    sqlx::query(
        "INSERT INTO plans SELECT (jsonb_populate_record(NULL::plans, to_jsonb(p) || $1)).* \
         FROM plans p WHERE p.id = 'team'",
    )
    .bind(patch)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE accounts SET plan_id = $1 WHERE id = $2")
        .bind(&id)
        .bind(account.0)
        .execute(pool)
        .await
        .unwrap();
}

async fn fx(prefix: &str, plan: Option<Value>) -> Option<Fx> {
    let pool = common::pg_pool_from_env().await?;
    let owner = make_user(&pool, prefix).await;
    let slug = unique_slug(prefix);
    let org = create_org_with_owner(&pool, owner, &slug, "n")
        .await
        .unwrap()
        .unwrap()
        .id;
    let account = AccountId(
        sqlx::query_scalar("SELECT account_id FROM organizations WHERE id = $1")
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap(),
    );
    plan_like_team(&pool, account, plan.unwrap_or_else(|| json!({}))).await;
    let channel: Uuid = sqlx::query_scalar(
        "INSERT INTO notification_channels (org_id, name, kind, config, enabled) \
         VALUES ($1, 'ops', 'webhook', '{\"webhook\":{\"url\":\"http://x/\",\"headers\":{}}}'::jsonb, true) \
         RETURNING id",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (_, _, state) = common::build_test_app_with_pg_state(pool.clone(), |_| {}).await;
    let (tx, signals) = mpsc::channel(64);
    let state = state.with_incident_signals(tx);
    let router = uptimepage::build_app_router(state.clone(), CancellationToken::new());
    let token = token_for(&pool, owner, org, &ScopeSet::full_access()).await;
    Some(Fx {
        pool,
        org,
        slug,
        account,
        owner,
        token,
        channel,
        state,
        router,
        signals,
    })
}

async fn token_for(pool: &PgPool, user: UserId, org: OrgId, scopes: &ScopeSet) -> String {
    api_tokens::create(pool, user, "t", scopes, Some(org), None, 16, 1000)
        .await
        .unwrap()
        .token
}

impl Fx {
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("x-uptimepage-org", &self.slug);
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    async fn create(&self, name: &str) -> Value {
        let (status, body) = self
            .call(
                "POST",
                "/api/v1/alert-sources",
                &self.token,
                Some(json!({"name": name, "kind": "generic", "alerts": [{"channel_id": self.channel}]})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body
    }

    async fn hook(
        &self,
        token: &str,
        action: &str,
        key: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let body = json!({
            "event_action": action,
            "dedup_key": key,
            "payload": {"summary": "Checkout error rate above 5%", "severity": "critical"}
        });
        let req = Request::post(format!("/hooks/alerts/{token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        (status, headers, body_json(res).await)
    }
}

fn token_of(source: &Value) -> String {
    source["ingest_token"]
        .as_str()
        .expect("a writer sees the token")
        .to_owned()
}

#[tokio::test]
#[ignore]
async fn a_source_is_created_changed_rotated_and_deleted_through_the_api() {
    let Some(mut fx) = fx("srcapi-life", None).await else {
        return;
    };
    let created = fx.create("Prod checks").await;
    let id = created["id"].as_str().unwrap().to_owned();
    let token = token_of(&created);
    assert!(
        created["ingest_url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/hooks/alerts/{token}"))
    );
    assert_eq!(created["routes_to_nobody"], false);

    let (status, listed) = fx
        .call("GET", "/api/v1/alert-sources", &fx.token, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let (status, paused) = fx
        .call(
            "PATCH",
            &format!("/api/v1/alert-sources/{id}"),
            &fx.token,
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(
        (status, &paused["enabled"]),
        (StatusCode::OK, &json!(false))
    );
    let (status, _) = fx
        .call(
            "PATCH",
            &format!("/api/v1/alert-sources/{id}"),
            &fx.token,
            Some(json!({"kind": "grafana"})),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "kind is fixed at create"
    );
    fx.call(
        "PATCH",
        &format!("/api/v1/alert-sources/{id}"),
        &fx.token,
        Some(json!({"enabled": true})),
    )
    .await;

    let (status, rotated) = fx
        .call(
            "POST",
            &format!("/api/v1/alert-sources/{id}/rotate"),
            &fx.token,
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(token_of(&rotated), token);
    assert!(rotated["previous_token_expires_at"].is_string());
    let (status, _) = fx
        .call(
            "DELETE",
            &format!("/api/v1/alert-sources/{id}/previous-token"),
            &fx.token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, after) = fx
        .call(
            "GET",
            &format!("/api/v1/alert-sources/{id}"),
            &fx.token,
            None,
        )
        .await;
    assert!(after["previous_token_expires_at"].is_null());
    let (status, _, _) = fx.hook(&token, "trigger", "k").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the old token is cut off");

    let (status, _, _) = fx.hook(&token_of(&rotated), "trigger", "k").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let opened = fx.signals.try_recv().expect("opened");
    let (status, events) = fx
        .call(
            "GET",
            &format!("/api/v1/alert-sources/{id}/events"),
            &fx.token,
            None,
        )
        .await;
    assert_eq!(
        (status, events),
        (StatusCode::OK, json!({"items": [], "next_cursor": null})),
        "no log without ClickHouse"
    );

    let (status, _) = fx
        .call(
            "DELETE",
            &format!("/api/v1/alert-sources/{id}"),
            &fx.token,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let closed = fx
        .signals
        .try_recv()
        .expect("the delete's close reaches the engine");
    assert_eq!(
        (closed.incident_id, closed.reason),
        (opened.incident_id, NotificationReason::Resolved)
    );
    let (status, gone) = fx
        .call(
            "GET",
            &format!("/api/v1/alert-sources/{id}"),
            &fx.token,
            None,
        )
        .await;
    assert_eq!(
        (status, &gone["error"]["code"]),
        (StatusCode::NOT_FOUND, &json!("ALERT_SOURCE_NOT_FOUND"))
    );
    let (_, incidents) = fx
        .call(
            "GET",
            &format!("/api/v1/incidents?source={id}"),
            &fx.token,
            None,
        )
        .await;
    assert_eq!(
        incidents.as_array().unwrap().len(),
        1,
        "its incidents stay findable"
    );
}

#[tokio::test]
#[ignore]
async fn only_a_writer_sees_the_ingest_url_and_only_a_deleter_deletes() {
    let Some(fx) = fx("srcapi-scope", None).await else {
        return;
    };
    let id = fx.create("Prod checks").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reader = token_for(
        &fx.pool,
        fx.owner,
        fx.org,
        &ScopeSet::from_strs(["alert_sources:read"]),
    )
    .await;
    let writer = token_for(
        &fx.pool,
        fx.owner,
        fx.org,
        &ScopeSet::from_strs(["alert_sources:write"]),
    )
    .await;
    let deleter = token_for(
        &fx.pool,
        fx.owner,
        fx.org,
        &ScopeSet::from_strs(["alert_sources:delete"]),
    )
    .await;

    let (status, read) = fx
        .call("GET", &format!("/api/v1/alert-sources/{id}"), &reader, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(read["ingest_url"].is_null() && read["ingest_token"].is_null());
    let (status, _) = fx
        .call(
            "POST",
            "/api/v1/alert-sources",
            &reader,
            Some(json!({"name": "x", "kind": "generic"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, written) = fx
        .call("GET", &format!("/api/v1/alert-sources/{id}"), &writer, None)
        .await;
    assert!(written["ingest_url"].is_string(), "write reads the URL");
    let (status, _) = fx
        .call(
            "DELETE",
            &format!("/api/v1/alert-sources/{id}"),
            &writer,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = fx
        .call(
            "DELETE",
            &format!("/api/v1/alert-sources/{id}"),
            &deleter,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
#[ignore]
async fn a_plan_without_sources_refuses_one_and_a_cap_stops_the_next() {
    let Some(off) = fx("srcapi-off", Some(json!({"max_alert_sources": 0}))).await else {
        return;
    };
    let (status, body) = off
        .call(
            "POST",
            "/api/v1/alert-sources",
            &off.token,
            Some(json!({"name": "x", "kind": "generic", "alerts": [{"channel_id": off.channel}]})),
        )
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("ALERT_SOURCES_DISABLED"))
    );

    let Some(one) = fx("srcapi-cap", Some(json!({"max_alert_sources": 1}))).await else {
        return;
    };
    one.create("first").await;
    let (status, body) = one
        .call(
            "POST",
            "/api/v1/alert-sources",
            &one.token,
            Some(json!({"name": "second", "kind": "generic", "alerts": [{"channel_id": one.channel}]})),
        )
        .await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("QUOTA_EXCEEDED"))
    );
    let (_, usage) = one
        .call(
            "GET",
            &format!("/api/v1/orgs/{}/usage", one.org.0),
            &one.token,
            None,
        )
        .await;
    assert_eq!(usage["quotas"]["max_alert_sources"]["current"], 1);
    assert_eq!(usage["quotas"]["max_alert_sources"]["limit"], 1);
    assert_eq!(usage["rate_limits"]["alert_events_per_minute"], 1200);
}

#[tokio::test]
#[ignore]
async fn only_the_owner_binds_a_source_to_a_policy() {
    let Some(fx) = fx("srcapi-policy", None).await else {
        return;
    };
    let policy: Uuid = sqlx::query_scalar(
        "INSERT INTO escalation_policies (org_id, name) VALUES ($1, 'ladder') RETURNING id",
    )
    .bind(fx.org.0)
    .fetch_one(&fx.pool)
    .await
    .unwrap();
    let member = make_user(&fx.pool, "srcapi-member").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'member')")
        .bind(member.0)
        .bind(fx.org.0)
        .execute(&fx.pool)
        .await
        .unwrap();
    let as_member = token_for(&fx.pool, member, fx.org, &ScopeSet::full_access()).await;
    let new = json!({"name": "ladder", "kind": "generic", "escalation_policy_id": policy});

    let (status, _) = fx
        .call(
            "POST",
            "/api/v1/alert-sources",
            &as_member,
            Some(new.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, created) = fx
        .call("POST", "/api/v1/alert-sources", &fx.token, Some(new))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap();
    let (status, _) = fx
        .call(
            "PATCH",
            &format!("/api/v1/alert-sources/{id}"),
            &as_member,
            Some(json!({"name": "renamed"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "a member may change the rest");
    let (status, unchanged) = fx
        .call(
            "PATCH",
            &format!("/api/v1/alert-sources/{id}"),
            &as_member,
            Some(json!({"escalation_policy_id": policy, "name": "again"})),
        )
        .await;
    assert_eq!(
        (status, &unchanged["escalation_policy_id"]),
        (StatusCode::OK, &json!(policy)),
        "and may send the policy back unchanged, as Terraform does"
    );
    let (status, _) = fx
        .call(
            "PATCH",
            &format!("/api/v1/alert-sources/{id}"),
            &as_member,
            Some(json!({"escalation_policy_id": null, "alerts": [{"channel_id": fx.channel}]})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "but not who it pages");
}

#[tokio::test]
#[ignore]
async fn a_held_source_refuses_new_firings_still_closes_and_comes_back_with_its_url() {
    let Some(fx) = fx("srcapi-hold", None).await else {
        return;
    };
    let older = fx.create("older").await;
    let newer = fx.create("newer").await;
    let token = token_of(&newer);
    assert_eq!(
        fx.hook(&token, "trigger", "open").await.0,
        StatusCode::ACCEPTED
    );

    plan_like_team(&fx.pool, fx.account, json!({"max_alert_sources": 1})).await;
    reconcile_after_change(&fx.pool, &fx.state.quotas, fx.account, None)
        .await
        .unwrap();

    let (_, holds) = fx
        .call("GET", "/api/v1/account/holds", &fx.token, None)
        .await;
    let held: Vec<&str> = holds["alert_sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    assert_eq!(held, ["newer"], "the newest is held, the oldest kept");
    let (_, kept) = fx
        .call(
            "GET",
            &format!("/api/v1/alert-sources/{}", older["id"].as_str().unwrap()),
            &fx.token,
            None,
        )
        .await;
    assert!(kept["plan_hold_at"].is_null());

    let (status, _, body) = fx.hook(&token, "trigger", "new").await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::FORBIDDEN, &json!("ALERT_SOURCE_HELD"))
    );
    let (status, _, body) = fx.hook(&token, "resolve", "open").await;
    assert_eq!(
        (status, &body["status"]),
        (StatusCode::ACCEPTED, &json!("resolved")),
        "a held source still closes"
    );

    plan_like_team(&fx.pool, fx.account, json!({"max_alert_sources": 5})).await;
    reconcile_after_change(&fx.pool, &fx.state.quotas, fx.account, None)
        .await
        .unwrap();
    let (status, _, _) = fx.hook(&token, "trigger", "new").await;
    assert_eq!(status, StatusCode::ACCEPTED, "released with the same URL");
}

#[tokio::test]
#[ignore]
async fn past_the_accounts_event_budget_a_sender_is_told_to_retry() {
    let Some(fx) = fx("srcapi-budget", Some(json!({"alert_events_per_minute": 2}))).await else {
        return;
    };
    let token = token_of(&fx.create("Prod checks").await);
    for key in ["a", "b"] {
        assert_eq!(
            fx.hook(&token, "trigger", key).await.0,
            StatusCode::ACCEPTED
        );
    }
    let (status, headers, body) = fx.hook(&token, "trigger", "c").await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (StatusCode::SERVICE_UNAVAILABLE, &json!("RATE_LIMITED"))
    );
    assert!(headers.contains_key(header::RETRY_AFTER));
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM incidents WHERE org_id = $1 AND origin = 'alert'")
            .bind(fx.org.0)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(open, 2, "the refused body opened nothing");
}

#[tokio::test]
#[ignore]
async fn an_alert_incident_shows_what_the_tool_said_and_filters_by_source() {
    let Some(fx) = fx("srcapi-incident", None).await else {
        return;
    };
    let one = fx.create("one").await;
    let other = fx.create("other").await;
    fx.hook(&token_of(&one), "trigger", "checkout-5xx").await;

    let (_, listed) = fx
        .call(
            "GET",
            &format!("/api/v1/incidents?source={}", one["id"].as_str().unwrap()),
            &fx.token,
            None,
        )
        .await;
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["origin"], "alert");
    assert_eq!(listed[0]["alert_source"]["name"], "one");
    assert_eq!(listed[0]["dedup_key"], "checkout-5xx");
    let (_, none) = fx
        .call(
            "GET",
            &format!("/api/v1/incidents?source={}", other["id"].as_str().unwrap()),
            &fx.token,
            None,
        )
        .await;
    assert_eq!(none, json!([]));

    let id = listed[0]["id"].as_str().unwrap();
    let (_, detail) = fx
        .call("GET", &format!("/api/v1/incidents/{id}"), &fx.token, None)
        .await;
    assert_eq!(detail["alert_state"]["firing_count"], 1);
    assert_eq!(detail["alert_state"]["reported_severity"], "critical");
}

/// One source's delivery log held in memory, in the order the real one reads.
struct Log {
    source: Uuid,
    rows: Vec<AlertEventView>,
}

#[async_trait]
impl AlertEventLog for Log {
    async fn recent(
        &self,
        _org: OrgId,
        _source: Uuid,
        outcomes: &[Status],
        after: Option<(chrono::DateTime<chrono::Utc>, &str)>,
        limit: usize,
    ) -> uptimepage::error::Result<Vec<AlertEventView>> {
        let mut rows = self.rows.clone();
        rows.sort_by(|a, b| {
            b.received_at
                .cmp(&a.received_at)
                .then(a.dedup_key.cmp(&b.dedup_key))
        });
        Ok(rows
            .into_iter()
            .filter(|e| outcomes.is_empty() || outcomes.contains(&e.outcome))
            .filter(|e| {
                after.is_none_or(|(at, key)| {
                    e.received_at < at || (e.received_at == at && e.dedup_key.as_str() > key)
                })
            })
            .take(limit)
            .collect())
    }

    async fn hourly(
        &self,
        _org: OrgId,
        source: Option<Uuid>,
        since: chrono::DateTime<chrono::Utc>,
    ) -> uptimepage::error::Result<Vec<AlertEventHour>> {
        if source.is_some_and(|s| s != self.source) {
            return Ok(Vec::new());
        }
        let mut hours: BTreeMap<chrono::DateTime<chrono::Utc>, AlertEventCounts> = BTreeMap::new();
        for e in self.rows.iter().filter(|e| e.received_at >= since) {
            let hour = e
                .received_at
                .duration_trunc(chrono::TimeDelta::hours(1))
                .unwrap();
            let slot = hours.entry(hour).or_default();
            *slot = *slot
                + AlertEventCounts {
                    events: 1,
                    not_applied: u64::from(Status::NOT_APPLIED.contains(&e.outcome)),
                };
        }
        Ok(hours
            .into_iter()
            .map(|(hour, counts)| AlertEventHour {
                source: self.source,
                hour,
                counts,
            })
            .collect())
    }
}

#[tokio::test]
#[ignore]
async fn the_delivery_log_pages_through_one_request_without_losing_an_event() {
    let Some(mut fx) = fx("srcapi-log", None).await else {
        return;
    };
    let id = fx.create("Prod checks").await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let at = chrono::Utc::now();
    let rows = ["a", "b", "c"]
        .map(|key| AlertEventView {
            received_at: at,
            outcome: Status::Opened,
            dedup_key: key.into(),
            incident_id: None,
            note: String::new(),
            body: String::new(),
        })
        .to_vec();
    let log = Log {
        source: id.parse().unwrap(),
        rows,
    };
    let state = fx.state.clone().with_alert_event_log(Arc::new(log));
    fx.router = uptimepage::build_app_router(state, CancellationToken::new());

    let path = format!("/api/v1/alert-sources/{id}/events?limit=2");
    let (_, first) = fx.call("GET", &path, &fx.token, None).await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("one more page")
        .to_owned();
    let (_, rest) = fx
        .call("GET", &format!("{path}&cursor={cursor}"), &fx.token, None)
        .await;
    assert_eq!(rest["items"][0]["dedup_key"], "c");
    assert!(rest["next_cursor"].is_null());
    let (status, _) = fx
        .call("GET", &format!("{path}&cursor=nonsense"), &fx.token, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
