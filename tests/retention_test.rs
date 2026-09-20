//! Live-PG (+CH for the no-op org cascade) tests for the daily retention
//! job, plus a pure test that the configured windows equal what the Privacy
//! Policy and the ClickHouse migration promise.
//! DB tests skipped by default; run under `--run-ignored` with `DATABASE_URL`
//! and `CLICKHOUSE_URL` set. Each test seeds rows tagged with a unique marker
//! and asserts only on its own rows against the shared dev DB.

mod common;

use common::make_user;
use uptimepage::config::{RetentionConfig, SessionConfig, TenancyConfig};
use uptimepage::jobs::retention::purge_old_data;
use uptimepage::storage::create_org_with_owner;
use uuid::Uuid;

async fn scalar_i64(pool: &sqlx::PgPool, sql: &str, marker: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sql)
        .bind(marker)
        .fetch_one(pool)
        .await
        .expect("count");
    n
}

#[tokio::test]
#[ignore]
async fn purges_past_window_and_keeps_fresh_rows() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };

    let marker = format!("ret-{}", Uuid::new_v4().simple());
    let user = make_user(&pool, "retention").await;
    let slug = format!("retn-{}", &Uuid::new_v4().simple().to_string()[..6]);
    let org = create_org_with_owner(&pool, user, &slug, "Retention Test")
        .await
        .expect("create org")
        .expect("org created")
        .id;

    // Each table gets a row past its own window, a row that sits between its
    // window and its neighbours' (so binding another table's window to this
    // query changes the surviving count), and a fresh row.
    for (days_ago, _label) in [(200_i64, "old"), (100_i64, "between"), (1_i64, "fresh")] {
        sqlx::query(
            "INSERT INTO login_attempts (user_id, method, success, ip_hash, occurred_at) \
             VALUES ($1, 'test', false, $2, now() - ($3::int * INTERVAL '1 day'))",
        )
        .bind(user.0)
        .bind(&marker)
        .bind(days_ago)
        .execute(&pool)
        .await
        .expect("insert login_attempt");
    }
    for days_ago in [120_i64, 100, 1] {
        sqlx::query(
            "INSERT INTO quota_events (org_id, user_id, event, ip_hash, occurred_at) \
             VALUES ($1, $2, 'test', $3, now() - ($4::int * INTERVAL '1 day'))",
        )
        .bind(org.0)
        .bind(user.0)
        .bind(&marker)
        .bind(days_ago)
        .execute(&pool)
        .await
        .expect("insert quota_event");
    }
    // Tag the action with the marker.
    for days_ago in [800_i64, 650, 1] {
        sqlx::query(
            "INSERT INTO org_audit_log (org_id, action, occurred_at) \
             VALUES ($1, $2, now() - ($3::int * INTERVAL '1 day'))",
        )
        .bind(org.0)
        .bind(&marker)
        .bind(days_ago)
        .execute(&pool)
        .await
        .expect("insert audit row");
    }
    // Tag the tool with the marker.
    for days_ago in [800_i64, 600, 1] {
        sqlx::query(
            "INSERT INTO mcp_audit (org_id, user_id, tool, outcome, created_at) \
             VALUES ($1, $2, $3, 'success', now() - ($4::int * INTERVAL '1 day'))",
        )
        .bind(org.0)
        .bind(user.0)
        .bind(&marker)
        .bind(days_ago)
        .execute(&pool)
        .await
        .expect("insert mcp_audit row");
    }
    // sessions: absolute-expired, idle-expired (alive absolute), and fresh.
    let sid = |k: &str| format!("{marker}-{k}");
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, expires_at, last_used_at) \
         VALUES ($1, $2, now() - INTERVAL '1 day', now())",
    )
    .bind(sid("expired"))
    .bind(user.0)
    .execute(&pool)
    .await
    .expect("insert expired session");
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, expires_at, last_used_at) \
         VALUES ($1, $2, now() + INTERVAL '30 days', now() - INTERVAL '60 days')",
    )
    .bind(sid("idle"))
    .bind(user.0)
    .execute(&pool)
    .await
    .expect("insert idle session");
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, expires_at, last_used_at) \
         VALUES ($1, $2, now() + INTERVAL '30 days', now())",
    )
    .bind(sid("fresh"))
    .bind(user.0)
    .execute(&pool)
    .await
    .expect("insert fresh session");

    // Every window distinct, so binding the wrong config field to a query
    // fails here instead of passing on a coincidence of equal defaults.
    let retention = RetentionConfig {
        login_attempts_days: 150,
        quota_events_days: 60,
        audit_log_days: 700,
        mcp_audit_days: 500,
        ..RetentionConfig::default()
    };
    let session = SessionConfig::default(); // idle 30d
    let grace = TenancyConfig::default().deletion_grace_period_days;

    let report = purge_old_data(&pool, &ch, &retention, &session, grace)
        .await
        .expect("retention run");

    // Each count below is unique to that table's own window: reach for a
    // neighbour's and the "between" row lands on the wrong side.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT count(*) FROM login_attempts WHERE ip_hash = $1",
            &marker
        )
        .await,
        2,
        "the 100-day and fresh login_attempts should remain under a 150-day window"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT count(*) FROM quota_events WHERE ip_hash = $1",
            &marker
        )
        .await,
        1,
        "only the fresh quota_event should remain under a 60-day window"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT count(*) FROM org_audit_log WHERE action = $1",
            &marker
        )
        .await,
        2,
        "the 650-day and fresh audit rows should remain under a 700-day window"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT count(*) FROM mcp_audit WHERE tool = $1",
            &marker
        )
        .await,
        1,
        "only the fresh MCP audit row should remain under a 500-day window"
    );
    assert!(
        report.mcp_audit >= 2,
        "the report feeds the only metric this table has, so it must count the \
         rows the delete actually removed (got {})",
        report.mcp_audit
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT count(*) FROM sessions WHERE id_hash LIKE $1 || '%'",
            &marker
        )
        .await,
        1,
        "only the fresh session should remain (absolute + idle both reaped)"
    );

    // Cleanup our own rows.
    let _ = sqlx::query("DELETE FROM login_attempts WHERE ip_hash = $1")
        .bind(&marker)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM sessions WHERE id_hash LIKE $1 || '%'")
        .bind(&marker)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org.0)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await;
}

/// Pure: the code windows, the Privacy Policy table and the ClickHouse TTL
/// must agree. If a window changes without the policy/migration being
/// updated, this fails — one source of truth.
#[test]
fn windows_match_privacy_policy_and_clickhouse_ttl() {
    // check-result retention is the ClickHouse table TTLs, not an app-side
    // knob — keep these in lockstep with the CH migrations and the public
    // claims in `docs/legal/privacy.md`. Raw + 1m rollup live 30 days; the 1h
    // rollup carries the aggregated history 13 months.
    const CHECK_RESULTS_RAW_DAYS: u32 = 30;
    const CHECK_RESULTS_HISTORY_MONTHS: u32 = 13;
    // Traces live the raw window. What they captured is content from behind the
    // customer's own login, so it goes sooner. Heartbeat pings split the same
    // way: the signal keeps, the job's own output takes the evidence window.
    const FLOW_EVIDENCE_DAYS: u32 = 7;
    let r = RetentionConfig::default();
    let s = SessionConfig::default();
    let grace = TenancyConfig::default().deletion_grace_period_days;

    let policy = include_str!("../docs/legal/privacy.md");
    let want = [
        format!("| Check results (raw per-check detail) | {CHECK_RESULTS_RAW_DAYS} days"),
        format!(
            "| Check result history (aggregated, hourly) | {CHECK_RESULTS_HISTORY_MONTHS} months"
        ),
        format!(
            "| Browser flow runs (which steps ran, and how long each took) | {CHECK_RESULTS_RAW_DAYS} days"
        ),
        format!(
            "| Browser flow failure evidence (page URL, title, visible text, browser console) | {FLOW_EVIDENCE_DAYS} days"
        ),
        format!(
            "| Heartbeat pings (when each signal arrived, its exit status, how long the run took) | {CHECK_RESULTS_RAW_DAYS} days"
        ),
        format!("| Output posted with a heartbeat ping | {FLOW_EVIDENCE_DAYS} days"),
        format!("| Login attempts | {} days", r.login_attempts_days),
        format!("| Quota events | {} days", r.quota_events_days),
        format!("| Sessions | {} days maximum", s.absolute_timeout_days),
        format!("recoverable for {grace} days"),
    ];
    for w in &want {
        assert!(
            policy.contains(w.as_str()),
            "Privacy Policy is missing/!= the configured retention: expected to find {w:?}"
        );
    }
    // 730 days is written as "2 years" in the policy — assert both the prose
    // and the exact number so neither can drift silently.
    assert_eq!(r.audit_log_days, 730, "audit window changed");
    assert!(
        policy.contains("| Audit log | 2 years |"),
        "Privacy Policy audit-log retention line changed"
    );
    assert_eq!(r.mcp_audit_days, 730, "MCP audit window changed");
    assert!(
        policy.contains(
            "| MCP write actions (tool, what it acted on, outcome, and the person and token behind it) | 2 years |"
        ),
        "Privacy Policy MCP audit retention line changed"
    );

    // Production loads default.toml, not the Rust `Default`, so pinning the
    // policy to only one of them leaves the other free to drift.
    let shipped = include_str!("../config/default.toml");
    for (key, days) in [
        ("login_attempts_days", r.login_attempts_days),
        ("quota_events_days", r.quota_events_days),
        ("audit_log_days", r.audit_log_days),
        ("mcp_audit_days", r.mcp_audit_days),
        ("api_tokens_post_expiry_days", r.api_tokens_post_expiry_days),
    ] {
        assert!(
            shipped
                .lines()
                .filter_map(|l| l.split_once('='))
                .any(|(k, v)| k.trim() == key
                    && v.split('#').next().unwrap_or_default().trim() == days.to_string()),
            "config/default.toml {key} disagrees with RetentionConfig::default() ({days})"
        );
    }

    let m1 = include_str!("../migrations/clickhouse/001_initial.sql");
    assert!(
        m1.contains(&format!("DEFAULT {CHECK_RESULTS_RAW_DAYS} CODEC(ZSTD(1))"))
            && m1.contains("TTL timestamp + toIntervalDay(ttl_days)"),
        "check_results raw retention must be the per-row ttl_days DEFAULT = CHECK_RESULTS_RAW_DAYS"
    );
    let m2 = include_str!("../migrations/clickhouse/002_check_results_1h.sql");
    assert!(
        m2.contains(&format!("INTERVAL {CHECK_RESULTS_HISTORY_MONTHS} MONTH")),
        "check_results_1h ClickHouse TTL must equal CHECK_RESULTS_HISTORY_MONTHS"
    );
    // Both flow windows are per-row, stamped from the plan; the DEFAULTs are
    // what an org with no snapshot yet gets, so they are the disclosed numbers.
    let m3 = include_str!("../migrations/clickhouse/003_flow_runs.sql");
    assert!(
        m3.contains(&format!(
            "ttl_days        UInt16 DEFAULT {CHECK_RESULTS_RAW_DAYS}"
        )) && m3.contains("TTL timestamp + toIntervalDay(ttl_days)"),
        "flow_runs row retention must be the per-row ttl_days DEFAULT = CHECK_RESULTS_RAW_DAYS"
    );
    assert!(
        m3.contains(&format!(
            "evidence_days   UInt16 DEFAULT {FLOW_EVIDENCE_DAYS}"
        )) && m3.contains("TTL timestamp + toIntervalDay(evidence_days)"),
        "flow evidence must expire on its own per-column TTL = FLOW_EVIDENCE_DAYS"
    );
    let m4 = include_str!("../migrations/clickhouse/004_heartbeat_pings.sql");
    assert!(
        m4.contains(&format!(
            "ttl_days      UInt16 DEFAULT {CHECK_RESULTS_RAW_DAYS}"
        )) && m4.contains("TTL received_at + toIntervalDay(ttl_days)"),
        "heartbeat_pings row retention must be the per-row ttl_days DEFAULT = CHECK_RESULTS_RAW_DAYS"
    );
    assert!(
        m4.contains(&format!(
            "evidence_days UInt16 DEFAULT {FLOW_EVIDENCE_DAYS}"
        )) && m4.contains("TTL received_at + toIntervalDay(evidence_days)"),
        "job output must expire on its own per-column TTL = FLOW_EVIDENCE_DAYS"
    );
    let pg = include_str!("../migrations/postgres/034_flow_plan_limits.up.sql");
    assert!(
        pg.contains(&format!(
            "evidence_days  INTEGER NOT NULL DEFAULT {FLOW_EVIDENCE_DAYS}"
        )),
        "the plan column the CH stamp reads must default to the disclosed window"
    );
}
