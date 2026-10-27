//! The embedded migrations as a booting build runs them.

use crate::common::{MIGRATOR, drop_test_db, fresh_test_db, open_test_pool};

/// Rolling back starts an older image on a database a newer one migrated
/// further.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_build_boots_on_migrations_a_newer_build_applied() {
    let Some((url, test_db)) = fresh_test_db("migrator").await else {
        return;
    };
    let pool = open_test_pool(&url).await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ($1, 'from a newer build', true, decode('00', 'hex'), 0)",
    )
    .bind(i64::MAX)
    .execute(&pool)
    .await
    .unwrap();
    let ran = MIGRATOR.run(&pool).await;
    pool.close().await;
    drop_test_db(&test_db).await;
    ran.unwrap();
}

/// A database that ran without the owed-page mark gets it for every open
/// incident whose current episode shows no first page, and only those.
#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_owed_page_migration_marks_only_episodes_that_never_paged() {
    use crate::common::{make_user, unique_slug};
    use uuid::Uuid;
    let Some((url, test_db)) = fresh_test_db("owedmig").await else {
        return;
    };
    let pool = open_test_pool(&url).await;
    MIGRATOR
        .undo(&pool, 108)
        .await
        .expect("undo the owed-page migration");
    let user = make_user(&pool, "owedmig").await;
    let org = uptimepage::storage::create_org_with_owner(&pool, user, &unique_slug("owedmig"), "n")
        .await
        .unwrap()
        .unwrap()
        .id
        .0;
    let policy: Uuid = sqlx::query_scalar(
        "INSERT INTO escalation_policies (org_id, name) VALUES ($1, 'p') RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    // One monitor each: a monitor holds one open incident at a time.
    let incident = async |sql_tail: &str| -> Uuid {
        let target: Uuid = sqlx::query_scalar(
            "INSERT INTO targets (org_id, name, check_spec, interval_secs) \
             VALUES ($1, 'svc', '{}'::jsonb, 30) RETURNING id",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap();
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO incidents (org_id, target_id, started_at, status_at_start) \
             VALUES ($1, $2, now() - interval '2 hours', 'down') RETURNING id",
        )
        .bind(org)
        .bind(target)
        .fetch_one(&pool)
        .await
        .unwrap();
        if !sql_tail.is_empty() {
            let sql = format!("UPDATE incidents SET {sql_tail} WHERE id = $1");
            let mut update = sqlx::query(&sql).bind(id);
            if sql_tail.contains("$2") {
                update = update.bind(policy);
            }
            update.execute(&pool).await.unwrap();
        }
        id
    };
    let event = async |id: Uuid, kind: &str, ago_mins: i32| {
        sqlx::query(
            "INSERT INTO incident_events (org_id, incident_id, kind, actor_type, occurred_at) \
             VALUES ($1, $2, $3, 'system', now() - make_interval(mins => $4))",
        )
        .bind(org)
        .bind(id)
        .bind(kind)
        .bind(ago_mins)
        .execute(&pool)
        .await
        .unwrap();
    };
    let paged = async |id: Uuid, episode: i64, transport: &str| {
        sqlx::query(
            "INSERT INTO incident_notifications \
                 (org_id, incident_id, transport, reason, status, attempt, episode) \
             VALUES ($1, $2, $3, 'opened', 'suppressed', 0, $4)",
        )
        .bind(org)
        .bind(id)
        .bind(transport)
        .bind(episode)
        .execute(&pool)
        .await
        .unwrap();
    };

    let never_paged = incident("").await;
    let paged_once = incident("").await;
    paged(paged_once, 0, "webhook").await;
    let held = incident("").await;
    paged(held, 0, "damped").await;
    let armed_empty_rung = incident("escalation_policy_id = $2").await;
    let reopened_unpaged = incident("escalation_policy_id = $2").await;
    paged(reopened_unpaged, 0, "webhook").await;
    event(reopened_unpaged, "reopened", 30).await;
    // An escalated line is written before the arming it reports, so it
    // proves nothing on its own.
    event(reopened_unpaged, "escalated", 29).await;
    let reopened_armed =
        incident("escalation_policy_id = $2, next_escalation_at = now() + interval '5 minutes'")
            .await;
    paged(reopened_armed, 0, "webhook").await;
    event(reopened_armed, "reopened", 30).await;
    let ended = incident("ended_at = now(), state = 'resolved'").await;
    let quiet = incident("paging_enabled = false").await;

    MIGRATOR.run(&pool).await.expect("migrate again");

    let owed_at = async |id: Uuid| -> Option<chrono::DateTime<chrono::Utc>> {
        sqlx::query_scalar("SELECT page_owed_at FROM incidents WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let started: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT started_at FROM incidents WHERE id = $1")
            .bind(never_paged)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        owed_at(never_paged).await,
        Some(started),
        "owed since it opened"
    );
    let reopened_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT occurred_at FROM incident_events WHERE incident_id = $1 AND kind = 'reopened'",
    )
    .bind(reopened_unpaged)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        owed_at(reopened_unpaged).await,
        Some(reopened_at),
        "owed since it reopened"
    );
    for (id, why) in [
        (paged_once, "paged"),
        (held, "held"),
        (armed_empty_rung, "armed"),
        (reopened_armed, "armed again after the reopen"),
        (ended, "over"),
        (quiet, "asked for no alerts"),
    ] {
        assert_eq!(owed_at(id).await, None, "{why}");
    }
    pool.close().await;
    drop_test_db(&test_db).await;
}
