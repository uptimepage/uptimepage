//! Long-down monitors: the slow and restore passes on live Postgres, with
//! results from the in-memory store. No-ops without `DATABASE_URL`.

use crate::common;

use chrono::{DateTime, Duration, Utc};
use common::{
    default_http_check, delete_org_and_account, make_user, pg_pool_from_env, unique_slug,
};
use sqlx::PgPool;
use uptimepage::config::AppConfig;
use uptimepage::domain::quota::RegionCaps;
use uptimepage::domain::{CheckResult, CheckSpec, CheckStatus, ExpectedStatus, OrgId, UserId};
use uptimepage::jobs::long_down::{restore_pass, slow_pass};
use uptimepage::quotas::QuotaService;
use uptimepage::quotas::effective::{plan_digest, region_targets, resolve_plans};
use uptimepage::storage::admin::AdminRepo;
use uptimepage::storage::long_down::clear_checked;
use uptimepage::storage::{
    ClickhouseResultSink, ClickhouseResultsStore, InMemorySink, ResultSink, ResultsStore,
};
use uuid::Uuid;

struct Monitor {
    pool: PgPool,
    org: OrgId,
    user: UserId,
    id: Uuid,
    results: InMemorySink,
    quotas: QuotaService,
}

impl Monitor {
    /// A 60 s monitor last edited four days ago, failing from one region.
    async fn new(pool: &PgPool) -> Self {
        Self::watching(pool, http(), 60).await
    }

    async fn watching(pool: &PgPool, check: serde_json::Value, interval_secs: i32) -> Self {
        let user = make_user(pool, "longdown").await;
        let (account,): (Uuid,) = sqlx::query_as(
            "INSERT INTO accounts (owner_user_id, plan_id) VALUES ($1, 'team') \
             ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL \
             DO UPDATE SET plan_id = excluded.plan_id RETURNING id",
        )
        .bind(user.0)
        .fetch_one(pool)
        .await
        .expect("account");
        let (org,): (Uuid,) = sqlx::query_as(
            "INSERT INTO organizations (slug, name, account_id) VALUES ($1, 'Long down', $2) \
             RETURNING id",
        )
        .bind(unique_slug("ld"))
        .bind(account)
        .fetch_one(pool)
        .await
        .expect("org");
        let (id,): (Uuid,) = sqlx::query_as(
            "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, \
                                  created_at, updated_at) \
             VALUES ($1, 'switched off', $2, $3, true, \
                     now() - interval '4 days', now() - interval '4 days') RETURNING id",
        )
        .bind(org)
        .bind(check)
        .bind(interval_secs)
        .fetch_one(pool)
        .await
        .expect("target");
        let cfg = AppConfig::load().expect("config");
        Self {
            pool: pool.clone(),
            org: OrgId(org),
            user,
            id,
            results: InMemorySink::new(),
            quotas: QuotaService::new(&cfg, Some(pool.clone())),
        }
    }

    /// Down since four days ago: an open incident from then, failing checks
    /// through the window and one minutes ago.
    async fn long_down(pool: &PgPool) -> Self {
        let m = Self::new(pool).await;
        m.open_incident(Duration::days(4)).await;
        m.failing().await;
        m
    }

    async fn open_incident(&self, ago: Duration) {
        sqlx::query(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start) \
             VALUES ($1, $2, $3, 'down')",
        )
        .bind(self.org.0)
        .bind(self.id)
        .bind(Utc::now() - ago)
        .execute(&self.pool)
        .await
        .expect("incident");
    }

    async fn failing(&self) {
        for ago in [
            Duration::hours(60),
            Duration::hours(30),
            Duration::minutes(5),
        ] {
            self.check(ago, CheckStatus::Down).await;
        }
    }

    async fn check(&self, ago: Duration, status: CheckStatus) {
        self.results
            .write_batch(&[result(self.org, self.id, Utc::now() - ago, status)])
            .await
            .expect("result");
    }

    async fn slow(&self) {
        slow_pass(&self.pool, &self.results, &self.quotas)
            .await
            .expect("slow pass");
    }

    async fn restore(&self) {
        restore_pass(&self.pool).await.expect("restore pass");
    }

    async fn slowed_at(&self) -> Option<DateTime<Utc>> {
        sqlx::query_scalar("SELECT slowed_at FROM targets WHERE id = $1")
            .bind(self.id)
            .fetch_one(&self.pool)
            .await
            .expect("slowed_at")
    }

    async fn is_slowed(&self) -> bool {
        self.slowed_at().await.is_some()
    }

    /// A window over this monitor; `None` ends leave it running for an hour.
    async fn window(&self, starts: Duration, ends: Option<Duration>) -> Uuid {
        let now = Utc::now();
        sqlx::query_scalar(
            "INSERT INTO maintenance_windows (org_id, title, starts_at, ends_at) \
             VALUES ($1, 'work', $2, $3) RETURNING id",
        )
        .bind(self.org.0)
        .bind(now - starts)
        .bind(ends.map_or(now + Duration::hours(1), |ago| now - ago))
        .fetch_one(&self.pool)
        .await
        .expect("window")
    }

    async fn join(&self, window: Uuid, joined: Option<Duration>) {
        sqlx::query(
            "INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id, joined_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(self.org.0)
        .bind(window)
        .bind(self.id)
        .bind(joined.map(|ago| Utc::now() - ago))
        .execute(&self.pool)
        .await
        .expect("component");
    }

    async fn depart(&self, window: Uuid, left: Duration) {
        sqlx::query(
            "INSERT INTO maintenance_window_departures (org_id, maintenance_id, target_id, left_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(self.org.0)
        .bind(window)
        .bind(self.id)
        .bind(Utc::now() - left)
        .execute(&self.pool)
        .await
        .expect("departure");
    }

    async fn close_incident(&self) {
        self.exec(
            "UPDATE incidents SET ended_at = now() WHERE target_id = $1 AND ended_at IS NULL",
        )
        .await;
    }

    async fn exec(&self, sql: &str) {
        sqlx::query(sql)
            .bind(self.id)
            .execute(&self.pool)
            .await
            .expect(sql);
    }

    async fn cleanup(self) {
        delete_org_and_account(&self.pool, self.org.0).await;
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(self.user.0)
            .execute(&self.pool)
            .await;
    }
}

fn http() -> serde_json::Value {
    serde_json::to_value(CheckSpec::Http(default_http_check(
        "https://example.com".parse().expect("url"),
        ExpectedStatus::Exact(200),
    )))
    .expect("check spec")
}

fn result(org: OrgId, target: Uuid, at: DateTime<Utc>, status: CheckStatus) -> CheckResult {
    CheckResult {
        target_id: target,
        org_id: org.0,
        timestamp: at,
        status,
        duration_ms: 100,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        network: None,
        diagnostic: None,
        error: None,
        details: None,
    }
}

async fn fresh_region(pool: &PgPool, target: Uuid) -> String {
    let region = unique_slug("ld");
    sqlx::query("INSERT INTO regions (id, name, enabled) VALUES ($1, $1, true)")
        .bind(&region)
        .execute(pool)
        .await
        .expect("region");
    sqlx::query("INSERT INTO target_regions (target_id, region) VALUES ($1, $2)")
        .bind(target)
        .bind(&region)
        .execute(pool)
        .await
        .expect("assign region");
    region
}

async fn drop_region(pool: &PgPool, region: &str) {
    let _ = sqlx::query("DELETE FROM regions WHERE id = $1")
        .bind(region)
        .execute(pool)
        .await;
}

/// The interval an agent pulling `region` is handed for `target`, and the
/// etag that validates the pull.
async fn pulled(m: &Monitor, region: &str) -> (Option<u64>, String) {
    let repo = AdminRepo::new(m.pool.clone(), None, "long_down_test");
    let orgs = repo.region_org_ids(region).await.expect("org ids");
    let plans = resolve_plans(&m.quotas, orgs).await;
    let etag = repo
        .region_pull_etag(region, &RegionCaps::from(&plans), &plan_digest(&plans))
        .await
        .expect("etag");
    let interval = region_targets(&repo, region, true, &plans)
        .await
        .expect("region set")
        .into_iter()
        .find(|(_, t)| t.id == m.id)
        .map(|(_, t)| t.interval.as_secs());
    (interval, etag)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_down_for_days_is_handed_out_slower_until_its_incident_closes() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    let region = fresh_region(&pool, m.id).await;
    let (interval, full_speed) = pulled(&m, &region).await;
    assert_eq!(interval, Some(60));

    m.slow().await;
    assert!(m.is_slowed().await);
    let (interval, slowed) = pulled(&m, &region).await;
    assert_eq!(interval, Some(900));
    assert_ne!(full_speed, slowed, "a flip must invalidate the pull");

    m.close_incident().await;
    m.restore().await;
    assert!(!m.is_slowed().await);
    let (interval, restored) = pulled(&m, &region).await;
    assert_eq!(interval, Some(60));
    assert_ne!(slowed, restored);

    drop_region(&pool, &region).await;
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_edit_inside_the_window_keeps_full_speed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.exec("UPDATE targets SET updated_at = now() - interval '2 days' WHERE id = $1")
        .await;
    m.slow().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_incident_younger_than_the_window_keeps_full_speed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::new(&pool).await;
    m.open_incident(Duration::days(2)).await;
    m.failing().await;
    m.slow().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn no_open_incident_keeps_full_speed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::new(&pool).await;
    m.failing().await;
    m.slow().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_up_or_degraded_check_inside_the_window_keeps_full_speed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    for status in [CheckStatus::Up, CheckStatus::Degraded] {
        let m = Monitor::long_down(&pool).await;
        m.check(Duration::hours(48), status).await;
        m.slow().await;
        assert!(!m.is_slowed().await, "{status:?} counts as passing");
        m.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_pass_before_the_window_does_not_keep_full_speed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.check(Duration::hours(80), CheckStatus::Up).await;
    m.slow().await;
    assert!(m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_no_region_reported_on_lately_is_left_to_silence_detection() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::new(&pool).await;
    m.open_incident(Duration::days(4)).await;
    m.check(Duration::hours(60), CheckStatus::Down).await;
    m.check(Duration::hours(5), CheckStatus::Down).await;
    m.slow().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_already_at_the_slower_interval_is_not_slowed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::watching(&pool, http(), 900).await;
    m.open_incident(Duration::days(4)).await;
    m.failing().await;
    m.slow().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn certificate_and_domain_monitors_are_never_slowed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let checks = [
        serde_json::json!({"type": "tls_cert", "host": "example.com"}),
        serde_json::json!({"type": "domain_expiry", "domain": "example.com"}),
    ];
    for check in checks {
        let m = Monitor::watching(&pool, check, 60).await;
        m.open_incident(Duration::days(4)).await;
        m.failing().await;
        m.slow().await;
        assert!(!m.is_slowed().await);
        m.cleanup().await;
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_window_that_ran_inside_the_last_days_keeps_full_speed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    let w = m.window(Duration::days(2), Some(Duration::days(1))).await;
    m.join(w, None).await;
    m.slow().await;
    assert!(!m.is_slowed().await, "paging held or not, a window counts");
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn windows_it_was_not_in_during_the_window_do_not_count() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    let ended = m.window(Duration::days(5), Some(Duration::days(4))).await;
    m.join(ended, None).await;
    m.window(Duration::days(2), None).await;
    m.slow().await;
    assert!(m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_cancelled_window_counts_until_it_was_cancelled() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    let w = m.window(Duration::days(2), None).await;
    m.join(w, None).await;
    sqlx::query(
        "UPDATE maintenance_windows SET deleted_at = now() - interval '1 day' WHERE id = $1",
    )
    .bind(w)
    .execute(&pool)
    .await
    .expect("cancel");
    m.slow().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_window_cancelled_before_its_start_never_counted() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    let w = m.window(Duration::days(2), None).await;
    m.join(w, None).await;
    sqlx::query(
        "UPDATE maintenance_windows SET deleted_at = starts_at - interval '1 hour' WHERE id = $1",
    )
    .bind(w)
    .execute(&pool)
    .await
    .expect("cancel");
    m.slow().await;
    assert!(m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_dropped_from_a_started_window_counts_until_it_left() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let recent = Monitor::long_down(&pool).await;
    let w = recent.window(Duration::days(5), None).await;
    recent.depart(w, Duration::days(1)).await;
    recent.slow().await;
    assert!(!recent.is_slowed().await, "left a day ago");
    recent.cleanup().await;

    let long_ago = Monitor::long_down(&pool).await;
    let w = long_ago.window(Duration::days(5), None).await;
    long_ago.depart(w, Duration::days(4)).await;
    long_ago.slow().await;
    assert!(long_ago.is_slowed().await, "left four days ago");
    long_ago.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_added_to_a_running_window_counts_once_it_joined() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    let w = m.window(Duration::days(6), None).await;
    m.join(w, Some(Duration::minutes(1))).await;
    m.slow().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_passing_check_alone_does_not_restore() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    for status in [CheckStatus::Up, CheckStatus::Degraded] {
        m.check(Duration::minutes(1), status).await;
    }
    m.restore().await;
    assert!(m.is_slowed().await, "it waits for its incident to close");
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_passing_check_someone_ran_restores_at_once() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    let seen = m.slowed_at().await.expect("slowed");
    let elsewhere = OrgId(uuid::Uuid::now_v7());
    assert!(
        !clear_checked(&pool, elsewhere, m.id, seen)
            .await
            .expect("clear")
    );
    assert!(m.is_slowed().await, "another org cannot clear it");
    assert!(
        clear_checked(&pool, m.org, m.id, seen)
            .await
            .expect("clear")
    );
    assert!(!m.is_slowed().await);
    assert!(
        !clear_checked(&pool, m.org, m.id, seen)
            .await
            .expect("clear")
    );
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_restore_pass_with_nothing_new_keeps_it_slowed() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    m.restore().await;
    assert!(m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_edit_clears_it_at_once() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    m.exec("UPDATE targets SET name = name WHERE id = $1").await;
    assert!(
        m.is_slowed().await,
        "a write that leaves updated_at alone keeps it"
    );
    m.exec("UPDATE targets SET updated_at = now() WHERE id = $1")
        .await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_window_starting_restores() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    let w = m.window(Duration::minutes(1), None).await;
    m.join(w, None).await;
    m.restore().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_window_scheduled_for_later_does_not_restore() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    let w = m
        .window(-Duration::hours(1), Some(-Duration::hours(2)))
        .await;
    m.join(w, None).await;
    m.restore().await;
    assert!(m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_monitor_added_and_dropped_between_restore_passes_restores() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    let w = m.window(Duration::days(1), None).await;
    sqlx::query(
        "INSERT INTO maintenance_window_departures \
             (org_id, maintenance_id, target_id, joined_at, left_at) \
         VALUES ($1, $2, $3, now() - interval '2 minutes', now() - interval '1 minute')",
    )
    .bind(m.org.0)
    .bind(w)
    .bind(m.id)
    .execute(&pool)
    .await
    .expect("departure");
    m.restore().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_manual_resolve_restores() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    m.exec("UPDATE incidents SET ended_at = now() WHERE target_id = $1 AND ended_at IS NULL")
        .await;
    m.open_incident(Duration::zero()).await;
    m.restore().await;
    assert!(!m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_check_someone_ran_leaves_a_newer_episode_alone() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let m = Monitor::long_down(&pool).await;
    m.slow().await;
    let judged = m.slowed_at().await.expect("slowed");
    m.exec("UPDATE targets SET slowed_at = slowed_at + interval '1 hour' WHERE id = $1")
        .await;
    assert!(
        !clear_checked(&pool, m.org, m.id, judged)
            .await
            .expect("clear")
    );
    assert!(m.is_slowed().await);
    m.cleanup().await;
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn pass_marks_read_the_newest_buckets_from_the_rollup() {
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "default".into(),
        "default".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch);
    let org = OrgId(Uuid::now_v7());
    let (failing, recovered, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let now = Utc::now();
    sink.write_batch(&[
        result(org, failing, now - Duration::hours(50), CheckStatus::Down),
        result(org, failing, now - Duration::minutes(5), CheckStatus::Error),
        result(org, recovered, now - Duration::hours(80), CheckStatus::Up),
        result(
            org,
            recovered,
            now - Duration::hours(30),
            CheckStatus::Degraded,
        ),
        result(org, recovered, now - Duration::hours(2), CheckStatus::Down),
        result(OrgId(Uuid::now_v7()), other, now, CheckStatus::Up),
    ])
    .await
    .expect("insert");

    let since = now - Duration::hours(72);
    let marks = store
        .pass_marks(&[(org, failing), (org, recovered), (org, other)], since)
        .await
        .expect("marks");
    let hour = |at: DateTime<Utc>| {
        use chrono::DurationRound;
        at.duration_trunc(Duration::hours(1)).expect("hour")
    };
    let failing_marks = marks[&failing];
    assert_eq!(failing_marks.last_passed, None);
    assert_eq!(failing_marks.last_checked, hour(now - Duration::minutes(5)));
    let recovered_marks = marks[&recovered];
    assert_eq!(
        recovered_marks.last_passed,
        Some(hour(now - Duration::hours(30)))
    );
    assert_eq!(recovered_marks.last_checked, hour(now - Duration::hours(2)));
    assert!(
        !marks.contains_key(&other),
        "a target id under another org reads nothing"
    );
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn pass_marks_answers_for_more_targets_than_one_query_can_hold() {
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "default".into(),
        "default".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );
    let limited = ch.with_setting("max_query_size", "262144");
    let store = ClickhouseResultsStore::from_client(limited);
    let org = OrgId(Uuid::now_v7());
    let (first, last) = (Uuid::now_v7(), Uuid::now_v7());
    let now = Utc::now();
    sink.write_batch(&[
        result(org, first, now - Duration::minutes(5), CheckStatus::Down),
        result(org, last, now - Duration::minutes(5), CheckStatus::Up),
    ])
    .await
    .expect("insert");

    // About 320 KB of IN list in one statement, past the 256 KiB default.
    let mut targets = vec![(org, first)];
    targets.extend((0..4_000).map(|_| (org, Uuid::now_v7())));
    targets.push((org, last));
    let marks = store
        .pass_marks(&targets, now - Duration::hours(72))
        .await
        .expect("marks");
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[&first].last_passed, None);
    assert!(marks[&last].last_passed.is_some());
}
