//! The regional-agent surface (`/api/agent/*`) on live Postgres: only an
//! agent's own token gets in, a disabled agent is turned away, an agent reports
//! only for monitors in its token's region and under each monitor's own org,
//! an authenticated call stamps the agent as seen, and every config pull
//! records whether the agent holds a config.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Regions are global rows, so each
//! test gets its own database.

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::CheckResult;
use uptimepage::domain::agent_wire::{AGENT_MONITORS, HeldConfig, IngestRequestRef};
use uptimepage::storage::InMemorySink;
use uptimepage::storage::operator::{AgentRow, OperatorRepo};
use uuid::Uuid;

use common::{
    build_test_app_with_pg_store_anon_tweaked, drop_test_db, fresh_test_db, open_test_pool,
};

const REGION: &str = "eu-test";
const OTHER_REGION: &str = "us-test";

struct Rig {
    app: Router,
    pool: PgPool,
    repo: OperatorRepo,
    sink: Arc<InMemorySink>,
    org: Uuid,
    db_name: String,
}

async fn rig(prefix: &str) -> Option<Rig> {
    let (db_url, db_name) = fresh_test_db(prefix).await?;
    let pool = open_test_pool(&db_url).await;
    let repo = OperatorRepo::new(pool.clone());
    for (id, name) in [(REGION, "Frankfurt"), (OTHER_REGION, "Ashburn")] {
        assert!(
            repo.create_region(id, name, None, None, None, None, None)
                .await
                .unwrap()
        );
    }
    let sink = Arc::new(InMemorySink::new());
    let results = sink.clone();
    let (app, org) = build_test_app_with_pg_store_anon_tweaked(
        pool.clone(),
        |_| {},
        move |mut state| {
            state.result_sink = results;
            state
        },
    )
    .await;
    Some(Rig {
        app,
        pool,
        repo,
        sink,
        org: org.0,
        db_name,
    })
}

impl Rig {
    async fn agent(&self, region: &str) -> (Uuid, String) {
        let (row, token) = self
            .repo
            .create_agent(region, "edge-1")
            .await
            .unwrap()
            .expect("region exists");
        (row.id, token)
    }

    async fn pull(&self, bearer: Option<&str>) -> (StatusCode, Value, Option<String>) {
        self.pull_with(bearer, &[]).await
    }

    async fn pull_with(
        &self,
        bearer: Option<&str>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value, Option<String>) {
        let mut req = Request::get("/api/agent/targets");
        if let Some(token) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let etag = resp
            .headers()
            .get(header::ETAG)
            .map(|v| v.to_str().unwrap().to_string());
        let body = if status == StatusCode::NOT_MODIFIED {
            Value::Null
        } else {
            common::body_json(resp).await
        };
        (status, body, etag)
    }

    async fn ingest(&self, token: &str, results: &[CheckResult]) -> (StatusCode, Value) {
        let body = serde_json::to_vec(&IngestRequestRef {
            batch_id: Uuid::new_v4(),
            results,
            flow_runs: &[],
        })
        .unwrap();
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::post("/api/agent/results")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, common::body_json(resp).await)
    }

    async fn target_in(&self, region: &str) -> Uuid {
        let (id,): (Uuid,) = sqlx::query_as(
            r#"INSERT INTO targets (org_id, name, check_spec, interval_secs)
               VALUES ($1, 'api', '{"type":"http","url":"https://example.com/"}'::jsonb, 60)
               RETURNING id"#,
        )
        .bind(self.org)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
            .bind(id)
            .bind(region)
            .execute(&self.pool)
            .await
            .unwrap();
        id
    }

    async fn last_seen(&self, agent: Uuid) -> Option<DateTime<Utc>> {
        self.row(agent).await.last_seen_at
    }

    async fn row(&self, agent: Uuid) -> AgentRow {
        self.repo
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.id == agent)
            .expect("agent listed")
    }

    /// Put the agent's row in a known state; ages are seconds before now.
    async fn set_state(
        &self,
        agent: Uuid,
        seen_ago: Option<i64>,
        missing_ago: Option<i64>,
        count: Option<i32>,
    ) {
        sqlx::query(
            "UPDATE agents SET \
                 last_seen_at = now() - make_interval(secs => $2), \
                 config_missing_since = now() - make_interval(secs => $3), \
                 scheduled_monitors = $4 \
             WHERE id = $1",
        )
        .bind(agent)
        .bind(seen_ago.map(|s| s as f64))
        .bind(missing_ago.map(|s| s as f64))
        .bind(count)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn done(self) {
        self.pool.close().await;
        drop_test_db(&self.db_name).await;
    }
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn only_the_agents_own_token_gets_in() {
    let Some(rig) = rig("agentauth_token").await else {
        return;
    };
    let (_, token) = rig.agent(REGION).await;
    let mut tampered = token.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });

    for bearer in [
        None,
        Some(format!("sm_live_{}", Uuid::new_v4().simple())),
        Some(format!("sm_agent_{}", Uuid::new_v4().simple())),
        Some(tampered),
    ] {
        let (status, body, _) = rig.pull(bearer.as_deref()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
        assert!(body["targets"].is_null(), "{bearer:?} saw config");
    }

    let (status, body, etag) = rig.pull(Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["region"], REGION);
    assert!(etag.is_some());

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_disabled_agent_is_turned_away() {
    let Some(rig) = rig("agentauth_disabled").await else {
        return;
    };
    let (id, token) = rig.agent(REGION).await;

    assert!(rig.repo.set_agent_enabled(id, false).await.unwrap());
    let (status, body, _) = rig.pull(Some(&token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "AGENT_NOT_FOUND");
    let (status, _) = rig.ingest(&token, &[]).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "disabling stops the push too"
    );

    assert!(rig.repo.set_agent_enabled(id, true).await.unwrap());
    let (status, _, _) = rig.pull(Some(&token)).await;
    assert_eq!(status, StatusCode::OK);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_agent_reports_only_for_its_own_regions_monitors() {
    let Some(rig) = rig("agentauth_region").await else {
        return;
    };
    let (_, here) = rig.agent(REGION).await;
    let (_, there) = rig.agent(OTHER_REGION).await;
    let target = rig.target_in(OTHER_REGION).await;
    let claimed_org = Uuid::new_v4();
    let result = CheckResult::error(target, claimed_org, "connection refused");

    let (status, body) = rig.ingest(&here, std::slice::from_ref(&result)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["accepted"], 0);
    assert_eq!(
        body["dropped"], 1,
        "a target outside the token's region is spoof"
    );
    assert!(rig.sink.is_empty());

    let (status, body) = rig.ingest(&there, std::slice::from_ref(&result)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["accepted"], 1);
    let written = rig.sink.snapshot();
    assert_eq!(written.len(), 1);
    assert_eq!(
        written[0].org_id, rig.org,
        "the org comes from the assignment, not the body"
    );

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_authenticated_call_stamps_the_agent_as_seen() {
    let Some(rig) = rig("agentauth_seen").await else {
        return;
    };
    let (id, token) = rig.agent(REGION).await;
    assert!(rig.last_seen(id).await.is_none());

    let (status, _, _) = rig.pull(Some(&token)).await;
    assert_eq!(status, StatusCode::OK);

    let mut stamped = None;
    for _ in 0..50 {
        stamped = rig.last_seen(id).await;
        if stamped.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stamped.is_some(), "last_seen_at never set");

    rig.done().await;
}

const WINDOW_SECS: u64 = 90;

fn recent(at: Option<DateTime<Utc>>) -> bool {
    at.is_some_and(|t| Utc::now() - t < chrono::Duration::seconds(30))
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_pull_without_a_config_marks_the_agent_once() {
    let Some(rig) = rig("agentcfg_bare").await else {
        return;
    };
    let (id, token) = rig.agent(REGION).await;
    rig.set_state(id, Some(5), None, Some(4)).await;

    let (status, _, _) = rig.pull(Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    let first = rig.row(id).await;
    assert!(
        first.config_missing_since.is_some(),
        "a bare pull holds none"
    );
    assert_eq!(first.scheduled_monitors, None);

    let (status, _, _) = rig
        .pull_with(Some(&token), &[(AGENT_MONITORS, "none")])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        rig.row(id).await.config_missing_since,
        first.config_missing_since,
        "the mark keeps the first time"
    );

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_pull_stores_the_count_the_agent_reports() {
    let Some(rig) = rig("agentcfg_count").await else {
        return;
    };
    let (id, token) = rig.agent(REGION).await;
    rig.set_state(id, Some(5), Some(60), None).await;

    let (status, _, etag) = rig.pull_with(Some(&token), &[(AGENT_MONITORS, "26")]).await;
    assert_eq!(status, StatusCode::OK);
    let row = rig.row(id).await;
    assert_eq!(row.config_missing_since, None);
    assert_eq!(row.scheduled_monitors, Some(26));

    let etag = etag.expect("a full reply names its version");
    let (status, _, _) = rig
        .pull_with(Some(&token), &[("if-none-match", etag.as_str())])
        .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    let row = rig.row(id).await;
    assert_eq!(
        (row.config_missing_since, row.scheduled_monitors),
        (None, None),
        "an older agent's etag shows a config with no count"
    );

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_pull_whose_config_read_fails_still_records_its_evidence() {
    let Some(rig) = rig("agentcfg_fail").await else {
        return;
    };
    let (id, token) = rig.agent(REGION).await;
    rig.set_state(id, Some(5), Some(60), None).await;
    sqlx::query("ALTER TABLE target_regions RENAME TO target_regions_gone")
        .execute(&rig.pool)
        .await
        .unwrap();

    let (status, _, _) = rig.pull_with(Some(&token), &[(AGENT_MONITORS, "5")]).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let row = rig.row(id).await;
    assert_eq!(
        (row.config_missing_since, row.scheduled_monitors),
        (None, Some(5))
    );

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn showing_a_config_clears_the_mark_and_stamps_the_agent_seen() {
    let Some(rig) = rig("agentcfg_clear").await else {
        return;
    };
    let (id, _) = rig.agent(REGION).await;
    rig.set_state(id, Some(600), Some(600), None).await;

    rig.repo
        .record_config_evidence(id, HeldConfig::Loaded(Some(3)), WINDOW_SECS)
        .await
        .unwrap();
    let row = rig.row(id).await;
    assert_eq!(row.config_missing_since, None);
    assert_eq!(row.scheduled_monitors, Some(3));
    assert!(recent(row.last_seen_at), "{:?}", row.last_seen_at);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn showing_none_after_a_return_keeps_the_time_it_came_back() {
    let Some(rig) = rig("agentcfg_return").await else {
        return;
    };
    let (id, _) = rig.agent(REGION).await;
    rig.set_state(id, Some(600), None, Some(26)).await;

    rig.repo.touch_agent_seen(id, WINDOW_SECS).await.unwrap();
    let voided = rig.row(id).await;
    assert!(voided.config_missing_since.is_some());
    assert_eq!(voided.scheduled_monitors, None, "the old count went too");

    rig.repo
        .record_config_evidence(id, HeldConfig::Nothing, WINDOW_SECS)
        .await
        .unwrap();
    let row = rig.row(id).await;
    assert_eq!(row.config_missing_since, voided.config_missing_since);
    assert_eq!(row.scheduled_monitors, None);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_heartbeat_from_a_stale_or_never_seen_agent_marks_it() {
    let Some(rig) = rig("agentcfg_hb_stale").await else {
        return;
    };
    let (never, _) = rig.agent(REGION).await;
    let (stale, _) = rig.agent(OTHER_REGION).await;
    rig.set_state(stale, Some(600), None, Some(26)).await;

    for id in [never, stale] {
        rig.repo.touch_agent_seen(id, WINDOW_SECS).await.unwrap();
        let row = rig.row(id).await;
        assert!(recent(row.config_missing_since), "{id}");
        assert!(recent(row.last_seen_at), "{id}");
        assert_eq!(row.scheduled_monitors, None, "{id}");
        assert!(!row.holds_config(Utc::now(), std::time::Duration::from_secs(WINDOW_SECS)));
    }

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_heartbeat_from_a_fresh_agent_leaves_the_mark_alone() {
    let Some(rig) = rig("agentcfg_hb_fresh").await else {
        return;
    };
    let (holds, _) = rig.agent(REGION).await;
    let (missing, _) = rig.agent(OTHER_REGION).await;
    rig.set_state(holds, Some(10), None, Some(3)).await;
    rig.set_state(missing, Some(10), Some(300), None).await;
    let marked = rig.row(missing).await.config_missing_since;

    for id in [holds, missing] {
        rig.repo.touch_agent_seen(id, WINDOW_SECS).await.unwrap();
    }
    let holds = rig.row(holds).await;
    assert_eq!(holds.config_missing_since, None);
    assert_eq!(holds.scheduled_monitors, Some(3));
    assert_eq!(rig.row(missing).await.config_missing_since, marked);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_heartbeat_that_runs_after_a_clearing_write_voids_nothing() {
    let Some(rig) = rig("agentcfg_hb_late").await else {
        return;
    };
    let (id, _) = rig.agent(REGION).await;
    rig.set_state(id, Some(600), Some(30), None).await;

    rig.repo
        .record_config_evidence(id, HeldConfig::Loaded(Some(2)), WINDOW_SECS)
        .await
        .unwrap();
    rig.repo.touch_agent_seen(id, WINDOW_SECS).await.unwrap();
    let row = rig.row(id).await;
    assert_eq!(row.config_missing_since, None);
    assert_eq!(row.scheduled_monitors, Some(2));

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_agent_back_with_its_config_whose_pull_lands_first_keeps_holding_it() {
    let Some(rig) = rig("agentcfg_back").await else {
        return;
    };
    let (id, _) = rig.agent(REGION).await;
    rig.set_state(id, Some(600), None, Some(26)).await;

    rig.repo
        .record_config_evidence(id, HeldConfig::Loaded(Some(26)), WINDOW_SECS)
        .await
        .unwrap();
    rig.repo.touch_agent_seen(id, WINDOW_SECS).await.unwrap();
    let row = rig.row(id).await;
    assert_eq!(row.config_missing_since, None);
    assert_eq!(row.scheduled_monitors, Some(26));
    assert!(recent(row.last_seen_at), "{:?}", row.last_seen_at);

    rig.done().await;
}
