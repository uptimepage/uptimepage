//! The alert ingest route over HTTP: the token in the path or a Bearer
//! header, rotation overlap, Grafana's signature, the body bounds, plan hold
//! and pause, the per-token limit, the answer a sender reads, the engine
//! signal after each commit, and the delivery log.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::common;
use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use common::{body_json, make_user, unique_slug};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::domain::{
    AlertBinding, AlertGrouping, AlertSource, AlertSourceKind, AlertSourceUpdate, NewAlertSource,
    NotificationReason, OrgId, TargetAlerts, WriteSource,
};
use uptimepage::escalation::IncidentSignal;
use uptimepage::intake::gate::{IntakeGate, MAX_BODY_BYTES};
use uptimepage::intake::signature::{SIGNATURE_HEADER, TIMESTAMP_HEADER};
use uptimepage::security::mac::hmac_sha256_hex;
use uptimepage::storage::alert_delivery::{Delivery, Status};
use uptimepage::storage::alert_sources::{AlertSourceStore, DeletedSource, TokenMatch};
use uptimepage::storage::create_org_with_owner;
use uptimepage::storage::traits::AlertEventSink;
use uuid::Uuid;

const SECRET: &str = "grafana-signing-secret";

#[derive(Default)]
struct Recorded(Mutex<Vec<Delivery>>);

#[async_trait]
impl AlertEventSink for Recorded {
    async fn write_deliveries(&self, rows: &[Delivery]) {
        self.0.lock().unwrap().extend_from_slice(rows);
    }
}

struct Fx {
    pool: PgPool,
    org: OrgId,
    state: AppState,
    router: Router,
    signals: mpsc::Receiver<IncidentSignal>,
    log: Arc<Recorded>,
}

async fn fx(prefix: &str, tweak: impl FnOnce(&mut AppState)) -> Option<Fx> {
    let pool = common::pg_pool_from_env().await?;
    let user = make_user(&pool, prefix).await;
    let org = create_org_with_owner(&pool, user, &unique_slug(prefix), "n")
        .await
        .unwrap()
        .unwrap()
        .id;
    let (_, _, state) = common::build_test_app_with_pg_state(pool.clone(), |_| {}).await;
    let (tx, signals) = mpsc::channel(64);
    let log = Arc::new(Recorded::default());
    let mut state = state
        .with_incident_signals(tx)
        .with_alert_event_sink(log.clone());
    tweak(&mut state);
    let router = uptimepage::build_app_router(state.clone(), CancellationToken::new());
    Some(Fx {
        pool,
        org,
        state,
        router,
        signals,
        log,
    })
}

impl Fx {
    async fn source(
        &self,
        name: &str,
        kind: AlertSourceKind,
        grouping: AlertGrouping,
        secret: Option<&str>,
    ) -> AlertSource {
        let channel: Uuid = sqlx::query_scalar(
            "INSERT INTO notification_channels (org_id, name, kind, config, enabled) \
             VALUES ($1, $2, 'webhook', '{\"webhook\":{\"url\":\"http://x/\",\"headers\":{}}}'::jsonb, true) \
             RETURNING id",
        )
        .bind(self.org.0)
        .bind(format!("ops {name}"))
        .fetch_one(&self.pool)
        .await
        .unwrap();
        self.state
            .alert_source_store
            .create(
                self.org,
                NewAlertSource {
                    name: name.into(),
                    kind,
                    grouping,
                    escalation_policy_id: None,
                    alerts: TargetAlerts(vec![AlertBinding {
                        channel_id: channel,
                    }]),
                    notify_recovery: true,
                    renotify_interval_secs: 3600,
                    signing_secret: secret.map(Into::into),
                },
                WriteSource::Api,
                None,
                25,
            )
            .await
            .unwrap()
    }

    async fn generic(&self, name: &str) -> (AlertSource, String) {
        let s = self
            .source(name, AlertSourceKind::Generic, AlertGrouping::Group, None)
            .await;
        let token = s.token.clone().expect("a new source shows its token");
        (s, token)
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Value) {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        (status, headers, body_json(res).await)
    }

    async fn post(&self, token: &str, body: impl Into<Body>) -> (StatusCode, Value) {
        let (status, _, json) = self.send(to_path(token, body)).await;
        (status, json)
    }

    async fn open(&self, source: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM incidents \
             WHERE org_id = $1 AND alert_source_id = $2 AND ended_at IS NULL",
        )
        .bind(self.org.0)
        .bind(source)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// The delivery log is written off the response path.
    async fn logged(&self, n: usize) -> Vec<Delivery> {
        for _ in 0..100 {
            let rows = self.log.0.lock().unwrap().clone();
            if rows.len() >= n {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the delivery log never reached {n} rows");
    }
}

fn to_path(token: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post(format!("/hooks/alerts/{token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

fn generic(action: &str, key: &str) -> String {
    json!({
        "event_action": action,
        "dedup_key": key,
        "payload": {"summary": "Checkout error rate above 5%", "severity": "critical"}
    })
    .to_string()
}

fn signed(token: &str, body: &[u8], secret: &str, at: i64) -> Request<Body> {
    let ts = at.to_string();
    let sig = hmac_sha256_hex(secret.as_bytes(), &[ts.as_bytes(), b":", body]);
    Request::post(format!("/hooks/alerts/{token}"))
        .header(TIMESTAMP_HEADER, ts)
        .header(SIGNATURE_HEADER, sig)
        .body(Body::from(body.to_vec()))
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn an_alert_opens_repeats_and_resolves_and_the_engine_hears_each_change() {
    let Some(mut fx) = fx("hook-life", |_| {}).await else {
        return;
    };
    let (source, token) = fx.generic("Prod checks").await;

    let (status, body) = fx.post(&token, generic("trigger", "checkout-5xx")).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        body,
        json!({"status": "opened", "dedup_key": "checkout-5xx"})
    );
    let opened = fx.signals.try_recv().expect("signalled after the commit");
    assert_eq!(opened.reason, NotificationReason::Opened);

    let (_, body) = fx.post(&token, generic("trigger", "checkout-5xx")).await;
    assert_eq!(body["status"], "repeat");
    assert!(fx.signals.try_recv().is_err(), "a repeat pages nobody");

    let (_, body) = fx.post(&token, generic("resolve", "checkout-5xx")).await;
    assert_eq!(body["status"], "resolved");
    let resolved = fx.signals.try_recv().unwrap();
    assert_eq!(
        (resolved.incident_id, resolved.reason),
        (opened.incident_id, NotificationReason::Resolved)
    );
    assert_eq!(fx.open(source.id).await, 0);

    let rows = fx.logged(3).await;
    let statuses: Vec<Status> = rows.iter().map(|r| r.status).collect();
    assert_eq!(statuses, [Status::Opened, Status::Repeat, Status::Resolved]);
    assert!(rows.iter().all(|r| r.source_id == source.id
        && r.dedup_key == "checkout-5xx"
        && r.incident_id == Some(opened.incident_id)));
    assert!(rows[0].body.contains("\"event_action\":\"trigger\""));
    assert!(rows[1].body.is_empty(), "a repeat keeps no body");
    assert!(rows[2].body.contains("\"event_action\":\"resolve\""));
}

#[tokio::test]
#[ignore]
async fn the_bearer_header_carries_the_same_token_and_anything_else_is_unknown() {
    let Some(fx) = fx("hook-bearer", |_| {}).await else {
        return;
    };
    let (source, token) = fx.generic("Bearer checks").await;
    let bearer = |value: Option<String>| {
        let mut req = Request::post("/hooks/alerts");
        if let Some(v) = value {
            req = req.header(header::AUTHORIZATION, v);
        }
        req.body(Body::from(generic("trigger", "disk-full")))
            .unwrap()
    };
    let (status, _, body) = fx.send(bearer(Some(format!("Bearer {token}")))).await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::ACCEPTED, Some("opened"))
    );
    assert_eq!(fx.open(source.id).await, 1);

    for value in [
        None,
        Some("Bearer ".into()),
        Some("Bearer not-a-token".into()),
    ] {
        let (status, _, body) = fx.send(bearer(value.clone())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{value:?}");
        assert_eq!(body["error"]["code"], "ALERT_TOKEN_UNKNOWN");
    }
    let (status, _) = fx.post("not-a-token", generic("trigger", "x")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn the_old_token_works_through_the_overlap_and_not_after_a_revoke() {
    let Some(fx) = fx("hook-rotate", |_| {}).await else {
        return;
    };
    let (source, old) = fx.generic("Rotating").await;
    let rotated = fx
        .state
        .alert_source_store
        .rotate(fx.org, source.id, false, None)
        .await
        .unwrap()
        .unwrap();
    let new = rotated.token.unwrap();
    assert_ne!(new, old);
    assert_eq!(
        fx.post(&new, generic("trigger", "a")).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        fx.post(&old, generic("trigger", "b")).await.0,
        StatusCode::ACCEPTED
    );
    assert!(
        fx.state
            .alert_source_store
            .revoke_previous(fx.org, source.id, None)
            .await
            .unwrap()
    );
    assert_eq!(
        fx.post(&old, generic("trigger", "c")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(fx.open(source.id).await, 2);
}

#[tokio::test]
#[ignore]
async fn a_signed_source_takes_only_a_fresh_valid_signature_over_the_whole_body() {
    let Some(fx) = fx("hook-sign", |_| {}).await else {
        return;
    };
    let source = fx
        .source(
            "Signed Grafana",
            AlertSourceKind::Grafana,
            AlertGrouping::Group,
            Some(SECRET),
        )
        .await;
    let token = source.token.clone().unwrap();
    let body = include_bytes!("fixtures/alerts/grafana_firing.json");
    let now = Utc::now().timestamp();

    let refused = [
        signed(&token, body, "the-wrong-secret-entirely", now),
        signed(&token, body, SECRET, now - 600),
        Request::post(format!("/hooks/alerts/{token}"))
            .body(Body::from(body.to_vec()))
            .unwrap(),
        {
            let mut req = signed(&token, body, SECRET, now);
            req.headers_mut().remove(TIMESTAMP_HEADER);
            req
        },
        signed(&token, b"not json", "the-wrong-secret-entirely", now),
    ];
    for req in refused {
        let (status, _, body) = fx.send(req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "ALERT_SIGNATURE_INVALID");
    }
    assert_eq!(
        fx.open(source.id).await,
        0,
        "nothing opens unsigned, unparsed included"
    );

    let (status, _, reply) = fx.send(signed(&token, body, SECRET, now)).await;
    assert_eq!(
        (status, reply["status"].as_str()),
        (StatusCode::ACCEPTED, Some("opened"))
    );
    let (_, _, reply) = fx.send(signed(&token, b"not json", SECRET, now)).await;
    assert_eq!(
        reply["status"], "unparsed",
        "a signed body nobody can read still opens"
    );
    assert_eq!(fx.open(source.id).await, 2);
}

#[tokio::test]
#[ignore]
async fn any_content_type_is_read_as_the_source_kind() {
    let Some(fx) = fx("hook-ctype", |_| {}).await else {
        return;
    };
    let (_, token) = fx.generic("Plain text").await;
    let req = Request::post(format!("/hooks/alerts/{token}"))
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(generic("trigger", "plain")))
        .unwrap();
    let (status, _, body) = fx.send(req).await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::ACCEPTED, Some("opened"))
    );
}

#[tokio::test]
#[ignore]
async fn a_body_over_the_limit_or_never_finished_opens_nothing() {
    let Some(fx) = fx("hook-body", |state| {
        state.alert_gate = Arc::new(IntakeGate::with_deadline(Duration::from_millis(200)));
    })
    .await
    else {
        return;
    };
    let (source, token) = fx.generic("Big bodies").await;

    let (status, body) = fx.post(&token, vec![b' '; MAX_BODY_BYTES + 1]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["code"], "ALERT_BODY_TOO_LARGE");
    let declared = Request::post(format!("/hooks/alerts/{token}"))
        .header(header::CONTENT_LENGTH, (MAX_BODY_BYTES + 1).to_string())
        .body(Body::empty())
        .unwrap();
    assert_eq!(fx.send(declared).await.0, StatusCode::PAYLOAD_TOO_LARGE);

    let stalled = Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>());
    let (status, headers, body) = fx.send(to_path(&token, stalled)).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a sender retries a 5xx"
    );
    assert_eq!(body["error"]["code"], "ALERT_BODY_STALLED");
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert_eq!(fx.open(source.id).await, 0);
}

#[tokio::test]
#[ignore]
async fn a_flood_on_one_token_answers_503_before_any_lookup() {
    let Some(fx) = fx("hook-flood", |_| {}).await else {
        return;
    };
    let token = "an-unknown-token-sent-in-a-loop";
    let mut throttled = None;
    for _ in 0..400 {
        let (status, headers, body) = fx.send(to_path(token, "{}")).await;
        if status == StatusCode::SERVICE_UNAVAILABLE {
            throttled = Some((headers, body));
            break;
        }
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (headers, body) = throttled.expect("the burst ran out");
    assert_eq!(body["error"]["code"], "RATE_LIMITED");
    assert_eq!(headers[header::RETRY_AFTER], "1");
}

#[tokio::test]
#[ignore]
async fn a_held_source_refuses_a_firing_and_still_closes_and_a_paused_one_accepts() {
    let Some(mut fx) = fx("hook-hold", |_| {}).await else {
        return;
    };
    let (source, token) = fx.generic("Over the plan").await;
    assert_eq!(
        fx.post(&token, generic("trigger", "before")).await.0,
        StatusCode::ACCEPTED
    );
    sqlx::query("UPDATE alert_sources SET plan_hold_at = now() WHERE org_id = $1 AND id = $2")
        .bind(fx.org.0)
        .bind(source.id)
        .execute(&fx.pool)
        .await
        .unwrap();

    let (status, body) = fx.post(&token, generic("trigger", "after")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "ALERT_SOURCE_HELD");
    let (status, body) = fx.post(&token, generic("resolve", "before")).await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::ACCEPTED, Some("resolved"))
    );
    assert_eq!(fx.open(source.id).await, 0);
    while fx.signals.try_recv().is_ok() {}

    let (paused, paused_token) = fx.generic("Paused").await;
    fx.state
        .alert_source_store
        .update(
            fx.org,
            paused.id,
            AlertSourceUpdate {
                enabled: Some(false),
                ..Default::default()
            },
            WriteSource::Api,
            None,
        )
        .await
        .unwrap();
    let (status, body) = fx.post(&paused_token, generic("trigger", "quiet")).await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::ACCEPTED, Some("paused"))
    );
    assert_eq!(fx.open(paused.id).await, 0);
    assert!(fx.signals.try_recv().is_err());
}

#[tokio::test]
#[ignore]
async fn a_request_with_several_events_lists_each_and_logs_the_body_once() {
    let Some(fx) = fx("hook-many", |_| {}).await else {
        return;
    };
    let source = fx
        .source(
            "Per alert",
            AlertSourceKind::Alertmanager,
            AlertGrouping::Alert,
            None,
        )
        .await;
    let token = source.token.clone().unwrap();
    let body = include_str!("fixtures/alerts/alertmanager_partial.json");
    let (status, reply) = fx.post(&token, body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        reply,
        json!({"status": "accepted", "events": [
            {"status": "ignored", "dedup_key": "9f5acf3bffd528ce"},
            {"status": "opened", "dedup_key": "e6183ab088811254"},
            {"status": "opened", "dedup_key": "3881e5bfa4aa691c"},
        ]})
    );
    let rows = fx.logged(3).await;
    assert_eq!(rows.iter().filter(|r| !r.body.is_empty()).count(), 1);
    assert!(rows.iter().all(|r| r.received_at == rows[0].received_at));
    assert_eq!(rows[0].note, "nothing open for this key");
}

#[tokio::test]
#[ignore]
async fn a_held_source_lists_what_else_its_body_did_in_the_refusal() {
    let Some(fx) = fx("hook-held-many", |_| {}).await else {
        return;
    };
    let source = fx
        .source(
            "Held per alert",
            AlertSourceKind::Alertmanager,
            AlertGrouping::Alert,
            None,
        )
        .await;
    let token = source.token.clone().unwrap();
    let mut body: Value =
        serde_json::from_str(include_str!("fixtures/alerts/alertmanager_partial.json")).unwrap();
    body["alerts"][0]["status"] = "firing".into();
    body["alerts"][0]["endsAt"] = "0001-01-01T00:00:00Z".into();
    assert_eq!(
        fx.post(&token, body.to_string()).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(fx.open(source.id).await, 3);
    sqlx::query("UPDATE alert_sources SET plan_hold_at = now() WHERE org_id = $1 AND id = $2")
        .bind(fx.org.0)
        .bind(source.id)
        .execute(&fx.pool)
        .await
        .unwrap();

    let mut body: Value =
        serde_json::from_str(include_str!("fixtures/alerts/alertmanager_partial.json")).unwrap();
    body["alerts"][2]["fingerprint"] = "a-new-alert".into();
    let (status, reply) = fx.post(&token, body.to_string()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(reply["error"]["code"], "ALERT_SOURCE_HELD");
    assert_eq!(
        reply["error"]["details"]["events"],
        json!([
            {"status": "resolved", "dedup_key": "9f5acf3bffd528ce"},
            {"status": "repeat", "dedup_key": "e6183ab088811254"},
            {"status": "held", "dedup_key": "a-new-alert"},
        ])
    );
    assert_eq!(fx.open(source.id).await, 2);
}

#[tokio::test]
#[ignore]
async fn a_notification_naming_no_alert_still_marks_the_source_heard() {
    let Some(fx) = fx("hook-heard", |_| {}).await else {
        return;
    };
    let source = fx
        .source(
            "Prod Datadog",
            AlertSourceKind::Datadog,
            AlertGrouping::Group,
            None,
        )
        .await;
    let token = source.token.clone().unwrap();
    let muted = json!({"transition": "Muted", "cycle_key": "c1", "title": "CPU"});
    let (status, reply) = fx.post(&token, muted.to_string()).await;
    assert_eq!(
        (status, reply),
        (StatusCode::ACCEPTED, json!({"status": "ignored"}))
    );
    let heard: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_event_at FROM alert_sources WHERE org_id = $1 AND id = $2")
            .bind(fx.org.0)
            .bind(source.id)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert!(heard.is_some());
    let rows = fx.logged(1).await;
    assert_eq!(
        (rows[0].status, rows[0].dedup_key.as_str()),
        (Status::Ignored, "")
    );
}

#[tokio::test]
#[ignore]
async fn an_operator_can_keep_the_bodies_of_repeats() {
    let Some(fx) = fx("hook-keep-repeats", |state| {
        Arc::make_mut(&mut state.cfg)
            .alert_intake
            .keep_repeat_bodies = true;
    })
    .await
    else {
        return;
    };
    let (_, token) = fx.generic("Keeps repeats").await;
    for _ in 0..2 {
        assert_eq!(
            fx.post(&token, generic("trigger", "kept")).await.0,
            StatusCode::ACCEPTED
        );
    }
    let rows = fx.logged(2).await;
    assert_eq!(rows[1].status, Status::Repeat);
    assert!(rows[1].body.contains("\"dedup_key\":\"kept\""));
}

/// Reports every token's source under the other grouping, as if the grouping
/// changed between the token lookup and the body being applied.
struct StaleGrouping(Arc<dyn AlertSourceStore>);

#[async_trait]
impl AlertSourceStore for StaleGrouping {
    async fn list(&self, org: OrgId) -> uptimepage::error::Result<Vec<AlertSource>> {
        self.0.list(org).await
    }
    async fn names(
        &self,
        org: OrgId,
    ) -> uptimepage::error::Result<Vec<uptimepage::storage::alert_sources::SourceName>> {
        self.0.names(org).await
    }
    async fn get(&self, org: OrgId, id: Uuid) -> uptimepage::error::Result<Option<AlertSource>> {
        self.0.get(org, id).await
    }
    async fn create(
        &self,
        org: OrgId,
        new: NewAlertSource,
        source: WriteSource,
        actor: Option<uptimepage::domain::UserId>,
        cap: i64,
    ) -> uptimepage::error::Result<AlertSource> {
        self.0.create(org, new, source, actor, cap).await
    }
    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: AlertSourceUpdate,
        source: WriteSource,
        actor: Option<uptimepage::domain::UserId>,
    ) -> uptimepage::error::Result<Option<AlertSource>> {
        self.0.update(org, id, update, source, actor).await
    }
    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Option<uptimepage::domain::UserId>,
    ) -> uptimepage::error::Result<Option<DeletedSource>> {
        self.0.delete(org, id, actor).await
    }
    async fn rotate(
        &self,
        org: OrgId,
        id: Uuid,
        revoke_previous: bool,
        actor: Option<uptimepage::domain::UserId>,
    ) -> uptimepage::error::Result<Option<AlertSource>> {
        self.0.rotate(org, id, revoke_previous, actor).await
    }
    async fn revoke_previous(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Option<uptimepage::domain::UserId>,
    ) -> uptimepage::error::Result<bool> {
        self.0.revoke_previous(org, id, actor).await
    }
    async fn find_by_token(&self, raw: &str) -> uptimepage::error::Result<Option<TokenMatch>> {
        Ok(self.0.find_by_token(raw).await?.map(|mut found| {
            found.source.grouping = match found.source.grouping {
                AlertGrouping::Group => AlertGrouping::Alert,
                AlertGrouping::Alert => AlertGrouping::Group,
            };
            found
        }))
    }
}

/// A body read under a grouping the source has left is parsed again under
/// the live one, so it opens the incident its later events will match,
/// without the sender having to send it again.
#[tokio::test]
#[ignore]
async fn a_body_read_under_an_old_grouping_is_parsed_again_under_the_new_one() {
    let Some(fx) = fx("hook-regroup", |state| {
        state.alert_source_store = Arc::new(StaleGrouping(state.alert_source_store.clone()));
    })
    .await
    else {
        return;
    };
    let source = fx
        .source(
            "Per group",
            AlertSourceKind::Alertmanager,
            AlertGrouping::Group,
            None,
        )
        .await;
    let token = source.token.clone().unwrap();
    let body = include_str!("fixtures/alerts/alertmanager_partial.json");
    let (status, reply) = fx.post(&token, body).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(reply["status"], "opened", "one event, the group: {reply}");
    assert_eq!(fx.open(source.id).await, 1);
    let rows = fx.logged(1).await;
    assert_eq!(rows.len(), 1, "logged once, as the group: {rows:?}");
}

/// A request the database cannot take leaves a line in the delivery log, so
/// an operator sees why the sender keeps retrying.
#[tokio::test]
#[ignore]
async fn a_request_the_database_could_not_take_is_logged_as_unavailable() {
    let Some(fx) = fx("hook-unavailable", |state| {
        let unreachable = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(100))
            .connect_lazy("postgres://nobody:x@127.0.0.1:1/none")
            .expect("a lazy pool");
        state.alert_ingest = Some(Arc::new(
            uptimepage::storage::alert_ingest::AlertIngest::new(unreachable),
        ));
    })
    .await
    else {
        return;
    };
    let (_, token) = fx.generic("Prod checks").await;
    let (status, _) = fx.post(&token, generic("trigger", "checkout-5xx")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let rows = fx.logged(1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, Status::Unavailable);
    assert!(rows[0].note.starts_with("not applied"), "{:?}", rows[0]);
    assert!(!rows[0].body.is_empty(), "with what was sent");
}
