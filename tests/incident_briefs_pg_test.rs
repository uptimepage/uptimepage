//! Postgres-backed coverage for `list_briefs`, the query behind the dashboard
//! banner and the MCP incident list. The in-memory store can't check the SQL
//! itself: the optional-parameter casts, the target join that drops manual
//! incidents, and the window that must still keep a long-running open incident.
//!
//! `#[ignore]`d by default; runs under `--run-ignored all` with `DATABASE_URL`
//! set. The harness auto-applies migrations on first connect.

use crate::common;

use common::{make_user, unique_slug};
use sqlx::PgPool;
use uptimepage::domain::OrgId;
use uptimepage::storage::{
    IncidentBriefFilter, IncidentNarrationStore, PgIncidentNarrationStore, TimeRange,
    create_org_with_owner,
};
use uuid::Uuid;

async fn seed_target(pool: &PgPool, org: OrgId, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
         VALUES ($1, $2, '{}'::jsonb, 30) RETURNING id",
    )
    .bind(org.0)
    .bind(name)
    .fetch_one(pool)
    .await
    .expect("insert target")
}

async fn seed_incident(
    pool: &PgPool,
    org: OrgId,
    target: Option<Uuid>,
    started: &str,
    ended: Option<&str>,
) {
    // A target-less incident seeded from scratch can only be operator-declared:
    // the schema's `incident_monitor_named` check enforces it.
    let sql = format!(
        "INSERT INTO incidents (org_id, target_id, started_at, ended_at, status_at_start, \
                                check_count, state, visibility, origin) \
         VALUES ($1, $2, now() - interval '{started}', {}, 'down', 1, {}, 'internal', {})",
        match ended {
            Some(e) => format!("now() - interval '{e}'"),
            None => "NULL".to_string(),
        },
        if ended.is_some() {
            "'resolved'"
        } else {
            "'triggered'"
        },
        if target.is_some() {
            "'monitor'"
        } else {
            "'manual'"
        }
    );
    sqlx::query(&sql)
        .bind(org.0)
        .bind(target)
        .execute(pool)
        .await
        .expect("insert incident");
}

#[tokio::test]
#[ignore]
async fn list_briefs_windows_filters_and_pages_pg() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let user = make_user(&pool, "brf").await;
    let org = create_org_with_owner(&pool, user, &unique_slug("brf"), "svc")
        .await
        .expect("create org")
        .expect("org created")
        .id;
    let api = seed_target(&pool, org, "api").await;
    let web = seed_target(&pool, org, "web").await;
    let db = seed_target(&pool, org, "db").await;

    seed_incident(&pool, org, Some(api), "10 minute", None).await;
    seed_incident(&pool, org, Some(api), "3 day", Some("2 day")).await;
    seed_incident(&pool, org, Some(api), "400 day", Some("399 day")).await;
    // Older than any window we ask for, still running.
    seed_incident(&pool, org, Some(db), "200 day", None).await;
    seed_incident(&pool, org, Some(web), "1 hour", Some("30 minute")).await;
    // Manual incident: no target, so no name to join — must never appear.
    seed_incident(&pool, org, None, "5 minute", None).await;

    let store = PgIncidentNarrationStore::new(pool.clone());
    let now = chrono::Utc::now();
    let month = TimeRange {
        from: now - chrono::Duration::days(30),
        to: now,
    };

    let open = store
        .list_briefs(org, IncidentBriefFilter::default())
        .await
        .expect("open incidents");
    assert_eq!(open.len(), 2, "both open monitor incidents, no manual row");
    assert!(open.iter().all(|i| i.ended_at.is_none()));
    assert!(
        open.iter()
            .all(|i| ["api", "db"].contains(&&*i.target_name))
    );

    let window = store
        .list_briefs(
            org,
            IncidentBriefFilter {
                range: Some(month),
                open_only: false,
                ..Default::default()
            },
        )
        .await
        .expect("windowed incidents");
    // The 400-day-old resolved one is out; the 200-day-old open one stays.
    assert_eq!(window.len(), 4);
    assert!(
        window
            .windows(2)
            .all(|w| w[0].started_at >= w[1].started_at),
        "newest first"
    );

    let one_monitor = store
        .list_briefs(
            org,
            IncidentBriefFilter {
                range: Some(month),
                target_id: Some(web),
                open_only: false,
                ..Default::default()
            },
        )
        .await
        .expect("per-monitor incidents");
    assert_eq!(one_monitor.len(), 1);
    assert_eq!(one_monitor[0].target_name, "web");
    assert!(one_monitor[0].ended_at.is_some());

    let page2 = store
        .list_briefs(
            org,
            IncidentBriefFilter {
                range: Some(month),
                open_only: false,
                limit: 2,
                offset: 2,
                ..Default::default()
            },
        )
        .await
        .expect("second page");
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].id, window[2].id, "offset skipped the first page");

    let oldest_first = store
        .list_briefs(
            org,
            IncidentBriefFilter {
                range: Some(month),
                open_only: false,
                oldest_first: true,
                ..Default::default()
            },
        )
        .await
        .expect("oldest first");
    assert_eq!(oldest_first[0].id, window[window.len() - 1].id);

    // A cursor past what Postgres can express is an empty page, not an error:
    // OFFSET rejects a negative, and `usize` reaches further than `i64`.
    let past_the_end = store
        .list_briefs(
            org,
            IncidentBriefFilter {
                range: Some(month),
                open_only: false,
                offset: usize::MAX,
                ..Default::default()
            },
        )
        .await
        .expect("an out-of-range offset must not error");
    assert!(past_the_end.is_empty());
}
