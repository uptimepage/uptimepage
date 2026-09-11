//! TTFB benchmark for the public-status aggregator under a 50-org × 50-component
//! fixture. Validates that the `(org_id, target_id, ts)` ORDER BY on
//! `check_results` keeps single-org lookups bounded — a regression to a
//! full-table scan would show up as a linear-in-org-count growth here.
//!
//! Requires a live Postgres + ClickHouse. If `DATABASE_URL` or `CLICKHOUSE_URL`
//! are unset the bench prints a skip message and exits cleanly, so this file
//! is safe to leave in `cargo bench` invocations on a laptop without docker.
//!
//! Run: `just up && DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/monitor \
//!                  CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo bench --bench public_status_ttfb`

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::runtime::Builder;
use uptimepage::domain::{
    CheckResult, CheckSpec, CheckStatus, ExpectedStatus, HttpCheck, HttpMethod, NewStatusPage,
    NewStatusPageComponent, NewTarget, OrgId, StatusPageId, UserId, WriteSource,
};
use uptimepage::public_status::{AggregatorConfig, OrgAggregator};
use uptimepage::storage::{
    ClickhouseResultSink, PgStatusPageStore, PostgresTargetStore, ResultSink, StatusPageStore,
    TargetStore, create_org_with_owner,
};
use url::Url;
use uuid::Uuid;

const ORG_COUNT: usize = 50;
const COMPONENTS_PER_ORG: usize = 50;
const RESULTS_PER_COMPONENT: usize = 60;

struct Fixture {
    pool: PgPool,
    ch: clickhouse::Client,
    aggregators: Vec<OrgAggregator>,
    /// Owned by the fixture so the bench-loop tenants stay alive; PG cascades
    /// clear their data on Drop.
    user_ids: Vec<UserId>,
    org_ids: Vec<OrgId>,
    /// One page per org, with all its targets curated as components — the page
    /// the benched `build` renders.
    page_ids: Vec<StatusPageId>,
}

async fn try_pg_pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()?;
    sqlx::migrate!("./migrations/postgres")
        .run(&pool)
        .await
        .ok()?;
    Some(pool)
}

async fn try_ch_client() -> Option<clickhouse::Client> {
    let url = std::env::var("CLICKHOUSE_URL").ok()?;
    let user = std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "monitor".into());
    let password = std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "monitor".into());
    let database = std::env::var("CLICKHOUSE_DATABASE").unwrap_or_else(|_| "monitor".into());
    let client = clickhouse::Client::default()
        .with_url(&url)
        .with_database(&database)
        .with_user(&user)
        .with_password(&password);
    uptimepage::storage::migrate(&client).await.ok()?;
    Some(client)
}

fn http_target(name: &str) -> NewTarget {
    let url = Url::parse("https://example.com/").unwrap();
    NewTarget {
        name: name.into(),
        check: CheckSpec::Http(HttpCheck {
            url,
            method: HttpMethod::Get,
            timeout: Duration::from_secs(5),
            follow_redirects: true,
            max_redirects: 3,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: Default::default(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        }),
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

fn ok_result(target_id: Uuid, org_id: Uuid, secs_ago: i64) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: Utc::now() - chrono::Duration::seconds(secs_ago),
        status: CheckStatus::Up,
        duration_ms: 42,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: None,
        diagnostic: None,
        error: None,
    }
}

async fn make_user(pool: &PgPool) -> UserId {
    let email = format!("ttfb-{}@bench.example", Uuid::now_v7());
    let (id,): (Uuid,) = sqlx::query_as(
        r#"INSERT INTO users (email, terms_version, privacy_version)
           VALUES ($1, $2, $3) RETURNING id"#,
    )
    .bind(&email)
    .bind(uptimepage::auth::consent::TERMS_VERSION)
    .bind(uptimepage::auth::consent::PRIVACY_VERSION)
    .fetch_one(pool)
    .await
    .expect("seed user");
    UserId(id)
}

fn unique_slug(prefix: &str) -> String {
    let id = Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &id[id.len() - 6..])
}

async fn build_fixture() -> Option<Fixture> {
    let pool = try_pg_pool().await?;
    let ch = try_ch_client().await?;

    let mut aggregators = Vec::with_capacity(ORG_COUNT);
    let mut user_ids = Vec::with_capacity(ORG_COUNT);
    let mut org_ids = Vec::with_capacity(ORG_COUNT);
    let mut page_ids = Vec::with_capacity(ORG_COUNT);

    let page_store = PgStatusPageStore::new(pool.clone());

    for i in 0..ORG_COUNT {
        let user = make_user(&pool).await;
        let org = create_org_with_owner(&pool, user, &unique_slug(&format!("ttfb{i}")), "ttfb")
            .await
            .expect("create org")
            .expect("slug fresh");
        let target_store =
            Arc::new(PostgresTargetStore::from_pool(pool.clone(), None)) as Arc<dyn TargetStore>;
        let sink = ClickhouseResultSink::new(
            ch.clone(),
            "default".into(),
            "default".into(),
            uptimepage::storage::OrgTtlDays::new(),
        );

        // Curate every target onto one page so the page-keyed aggregator has
        // the full component set to render.
        let page_id = page_store
            .create(
                org.id,
                NewStatusPage {
                    slug: unique_slug(&format!("page{i}")),
                    name: "status".into(),
                    enabled: true,
                },
                WriteSource::Ui,
                i64::MAX,
                None,
            )
            .await
            .expect("create page")
            .expect("page not capped")
            .id;

        for j in 0..COMPONENTS_PER_ORG {
            let t = target_store
                .create(
                    org.id,
                    http_target(&format!("ttfb-{i}-{j}")),
                    WriteSource::Ui,
                    i64::MAX,
                    i64::MAX,
                )
                .await
                .expect("create target");
            page_store
                .add_component(
                    org.id,
                    page_id,
                    NewStatusPageComponent {
                        target_id: t.id,
                        public_name: None,
                        public_description: None,
                        public_group: None,
                        sort_order: j as i32,
                        detail_link_enabled: false,
                    },
                    i64::MAX,
                    None,
                )
                .await
                .expect("add component");
            let rows: Vec<CheckResult> = (0..RESULTS_PER_COMPONENT)
                .map(|k| ok_result(t.id, org.id.0, (k as i64) * 30))
                .collect();
            sink.write_batch(&rows).await.expect("ch insert");
        }
        aggregators.push(OrgAggregator::new(
            pool.clone(),
            ch.clone(),
            AggregatorConfig::default(),
            None,
        ));
        user_ids.push(user);
        org_ids.push(org.id);
        page_ids.push(page_id);
    }

    // Single MV merge after all inserts. OPTIMIZE FINAL is table-wide; doing
    // it once per org would burn most of the fixture's wall-clock budget
    // re-merging the same partitions on every iteration.
    ch.query("OPTIMIZE TABLE check_results_1m FINAL")
        .execute()
        .await
        .expect("optimize mv");

    Some(Fixture {
        pool,
        ch,
        aggregators,
        user_ids,
        org_ids,
        page_ids,
    })
}

async fn teardown(fixture: &Fixture) {
    for org in &fixture.org_ids {
        let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(org.0)
            .execute(&fixture.pool)
            .await;
    }
    for user in &fixture.user_ids {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user.0)
            .execute(&fixture.pool)
            .await;
    }
    // Best-effort CH cleanup — the rows for these orgs are isolated by org_id,
    // so a regression test can simply DELETE WHERE org_id IN (...).
    for org in &fixture.org_ids {
        let _ = fixture
            .ch
            .query("ALTER TABLE check_results DELETE WHERE org_id = ?")
            .bind(org.0)
            .execute()
            .await;
        let _ = fixture
            .ch
            .query("ALTER TABLE check_results_1m DELETE WHERE org_id = ?")
            .bind(org.0)
            .execute()
            .await;
    }
}

fn bench_public_status_build(c: &mut Criterion) {
    let rt = Builder::new_multi_thread().enable_all().build().unwrap();
    let Some(fixture) = rt.block_on(build_fixture()) else {
        eprintln!(
            "skipped: DATABASE_URL / CLICKHOUSE_URL not set — run `just up` first to enable this bench"
        );
        return;
    };

    let mut group = c.benchmark_group("public_status_ttfb");
    group.sample_size(40);
    // Pick the median org so the bench measures a typical tenant, not the
    // first one (which may benefit from PG/CH page-cache warmup) or the last
    // (which may have a colder block cache).
    let middle = ORG_COUNT / 2;
    let middle_org = fixture.org_ids[middle];
    let middle_page = fixture.page_ids[middle];
    group.bench_function("aggregator_build_50x50", |b| {
        b.to_async(&rt).iter(|| async {
            fixture.aggregators[middle]
                .build(middle_page, middle_org)
                .await
                .expect("aggregator build");
        });
    });
    group.finish();

    rt.block_on(teardown(&fixture));
}

criterion_group!(benches, bench_public_status_build);
criterion_main!(benches);
