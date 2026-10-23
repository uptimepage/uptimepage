//! Live-PG (+CH for the no-op org cascade) tests for the daily retention
//! job, plus a pure test that the configured windows equal what the Privacy
//! Policy and the ClickHouse migration promise.
//! DB tests skipped by default; run under `--run-ignored` with `DATABASE_URL`
//! and `CLICKHOUSE_URL` set. Each runs on its own database: the purge is
//! global, so on a shared one it deletes rows other tests seeded.

use crate::common;

use common::make_user;
use uptimepage::config::{
    CHECK_RESULTS_HISTORY_MONTHS, CHECK_RESULTS_RAW_DAYS, FLOW_EVIDENCE_DAYS, RetentionConfig,
    SessionConfig, TenancyConfig,
};
use uptimepage::jobs::retention::purge_old_data;
use uptimepage::security::app_link::PUSHOVER_OFFER_COOLDOWN;
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
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let Some((db_url, db_name)) = common::fresh_test_db("retention").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;

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

    // A Telegram code or a Slack offer is dead once it expires; a Pushover
    // offer only once the cooldown stops counting it too.
    let cooldown = PUSHOVER_OFFER_COOLDOWN.num_days();
    for (label, app, created_days_ago, expires_days_ago) in [
        ("offer-past", "pushover", cooldown + 1, cooldown),
        ("offer-cooling", "pushover", cooldown - 1, cooldown - 2),
        ("offer-unexpired", "pushover", cooldown + 1, -1),
        ("offer-live", "pushover", 0, -1),
        ("tg-expired", "telegram", 1, 1),
        ("tg-live", "telegram", 0, -1),
        ("slack-expired", "slack", 1, 1),
        ("slack-live", "slack", 0, -1),
    ] {
        let telegram = app == "telegram";
        sqlx::query(
            "INSERT INTO app_link_challenges \
                 (code_hash, app, user_id, external_hash, created_at, expires_at) \
             VALUES ($1 || $2, $3, $4, $5, \
                     now() - ($6::int * INTERVAL '1 day'), \
                     now() - ($7::int * INTERVAL '1 day'))",
        )
        .bind(&marker)
        .bind(label)
        .bind(app)
        .bind(telegram.then_some(user.0))
        .bind((!telegram).then_some(&marker))
        .bind(created_days_ago)
        .bind(expires_days_ago)
        .execute(&pool)
        .await
        .expect("insert link code");
    }

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
    assert_eq!(
        report.mcp_audit, 2,
        "the report feeds the only metric this table has, so it must count the \
         rows the delete actually removed"
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

    let codes: Vec<String> = sqlx::query_scalar(
        "SELECT substr(code_hash, length($1) + 1) FROM app_link_challenges \
         WHERE code_hash LIKE $1 || '%' ORDER BY code_hash",
    )
    .bind(&marker)
    .fetch_all(&pool)
    .await
    .expect("read link codes");
    assert_eq!(
        codes,
        [
            "offer-cooling",
            "offer-live",
            "offer-unexpired",
            "slack-live",
            "tg-live"
        ]
    );
    assert_eq!(report.app_link_codes, 3);

    pool.close().await;
    common::drop_test_db(&db_name).await;
}

/// Pure: the code windows, the Privacy Policy table and the ClickHouse TTL
/// must agree. If a window changes without the policy/migration being
/// updated, this fails — one source of truth.
#[test]
fn windows_match_privacy_policy_and_clickhouse_ttl() {
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
        // The daily tick keeps a code up to a day past the cooldown.
        format!(
            "| One-time codes for linking an account, and Pushover, Slack and Discord link offers | {} days |",
            PUSHOVER_OFFER_COOLDOWN.num_days() + 1
        ),
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
        ("page_access_events_days", r.page_access_events_days),
        ("absolute_timeout_days", s.absolute_timeout_days),
        ("deletion_grace_period_days", grace),
    ] {
        assert!(
            shipped
                .lines()
                .filter_map(|l| l.split_once('='))
                .any(|(k, v)| k.trim() == key
                    && v.split('#').next().unwrap_or_default().trim() == days.to_string()),
            "config/default.toml {key} disagrees with its Rust default ({days})"
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

#[tokio::test]
#[ignore]
async fn private_page_history_is_kept_for_its_own_window() {
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let Some((db_url, db_name)) = common::fresh_test_db("retention_page").await else {
        return;
    };
    let pool = common::open_test_pool(&db_url).await;
    let user = make_user(&pool, "retention-page").await;
    let slug = "retention-page";
    let org = create_org_with_owner(&pool, user, slug, "Retention Page")
        .await
        .expect("create org")
        .expect("org created")
        .id;
    let (page,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled) VALUES ($1, $2, 'p', true) \
         RETURNING id",
    )
    .bind(org.0)
    .bind(slug)
    .fetch_one(&pool)
    .await
    .expect("page");
    let (viewer,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_page_viewers (status_page_id, org_id, email, email_verified_at) \
         VALUES ($1, $2, 'anna@dealer.example', now()) RETURNING id",
    )
    .bind(page)
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .expect("viewer");
    for days_ago in [400_i32, 200, 1] {
        sqlx::query(
            "INSERT INTO status_page_access_events \
                 (status_page_id, org_id, viewer_id, kind, occurred_at) \
             VALUES ($1, $2, $3, 'signed_in', now() - make_interval(days => $4))",
        )
        .bind(page)
        .bind(org.0)
        .bind(viewer)
        .bind(days_ago)
        .execute(&pool)
        .await
        .expect("event");
        sqlx::query(
            "INSERT INTO status_page_viewer_days (viewer_id, day, status_page_id, org_id) \
             VALUES ($1, (now() - make_interval(days => $4))::date, $2, $3)",
        )
        .bind(viewer)
        .bind(page)
        .bind(org.0)
        .bind(days_ago)
        .execute(&pool)
        .await
        .expect("visit day");
    }
    for (id, last_used_days, expired) in
        [("idle", 400, false), ("live", 1, false), ("ended", 1, true)]
    {
        sqlx::query(
            "INSERT INTO status_page_sessions \
                 (id_hash, status_page_id, org_id, method, viewer_id, last_used_at, expires_at) \
             VALUES ($1, $2, $3, 'email', $4, now() - make_interval(days => $5), \
                     CASE WHEN $6 THEN now() - interval '1 hour' END)",
        )
        .bind(format!("{page}-{id}"))
        .bind(page)
        .bind(org.0)
        .bind(viewer)
        .bind(last_used_days)
        .bind(expired)
        .execute(&pool)
        .await
        .expect("session");
    }

    let retention = RetentionConfig {
        page_access_events_days: 300,
        ..RetentionConfig::default()
    };
    purge_old_data(
        &pool,
        &ch,
        &retention,
        &SessionConfig::default(),
        TenancyConfig::default().deletion_grace_period_days,
    )
    .await
    .expect("retention run");

    let page_text = page.to_string();
    let count = |sql: &'static str| scalar_i64(&pool, sql, &page_text);
    assert_eq!(
        count("SELECT count(*) FROM status_page_access_events WHERE status_page_id = $1::uuid")
            .await,
        2
    );
    assert_eq!(
        count("SELECT count(*) FROM status_page_viewer_days WHERE status_page_id = $1::uuid").await,
        2
    );
    let left: Vec<String> = sqlx::query_scalar(
        "SELECT id_hash FROM status_page_sessions WHERE status_page_id = $1 ORDER BY id_hash",
    )
    .bind(page)
    .fetch_all(&pool)
    .await
    .expect("sessions");
    assert_eq!(left, vec![format!("{page}-live")]);

    pool.close().await;
    common::drop_test_db(&db_name).await;
}
