//! The regional-agent surface (`/api/agent/*`) on live Postgres: only an
//! agent's own token gets in, a disabled agent is turned away, an agent reports
//! only for monitors in its token's region and under each monitor's own org,
//! and an authenticated call stamps the agent as seen.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Regions are global rows, so each
//! test gets its own database.

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::CheckResult;
use uptimepage::domain::agent_wire::IngestRequestRef;
use uptimepage::storage::InMemorySink;
use uptimepage::storage::operator::OperatorRepo;
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
        let mut req = Request::get("/api/agent/targets");
        if let Some(token) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
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
        (status, common::body_json(resp).await, etag)
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

    async fn last_seen(&self, agent: Uuid) -> Option<chrono::DateTime<chrono::Utc>> {
        self.repo
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.id == agent)
            .expect("agent listed")
            .last_seen_at
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
