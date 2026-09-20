//! Daily purge of soft-deleted orgs and users past their grace period.
//!
//! Each tick runs the org cascade/outbox below, then a back-pressured
//! hard-delete of soft-deleted users whose grace window has elapsed
//! (`deleted_at < now() - grace_days`, the sole gate — within the window the
//! user can still restore). The user row's FK `ON DELETE CASCADE`
//! erases memberships, oauth_identities, api_tokens, invitations, sessions and
//! re-auth grants; rows that reference the user as an actor (`login_attempts`,
//! `org_audit_log`, `quota_events`, `plan_overrides`, `mcp_audit`) keep their
//! rows with the actor nulled — audit survives, identity does not.
//!
//! Two-step outbox pattern across Postgres and ClickHouse. The naive shape
//! ("DELETE in PG, then DELETE in CH") leaves orphan rows in ClickHouse if the
//! worker dies between the two — invisible to queries (the org no longer
//! exists in PG so no API path filters them), but they sit on disk forever and
//! break the "data fully erased within 30 days" privacy claim.
//!
//! Resolution: `clickhouse_purge_queue` is a durable handoff. The PG side
//! enqueues and cascades in one transaction; the CH side drains the queue
//! idempotently. A retry can replay either half without producing duplicates
//! or losing the request.
//!
//! Each tick:
//!  1. Selects orgs with `deleted_at` past the grace window (cap of 10 per
//!     tick — back-pressure against accidental mass deletion or a stuck CH).
//!  2. For each, runs a single PG transaction: insert into the queue (idem),
//!     then `DELETE FROM organizations` which cascades to every tenant table.
//!  3. Drains pending queue rows by issuing `ALTER ... DELETE` against
//!     ClickHouse. The default mutation mode is async (accepted ≠ applied),
//!     which would let GDPR erasure be reported "done" while bytes still sit
//!     on disk. Two guards close that gap: the mutation runs with
//!     `mutations_sync=2` so the call returns only once it is applied, and
//!     the queue row is marked complete only after a `count()` confirms zero
//!     rows remain for that org. The privacy guarantee is "rows gone", so we
//!     assert exactly that rather than trusting the submit ack.
//!
//! Idempotency:
//!  * `ON CONFLICT (org_id) DO NOTHING` on the queue insert.
//!  * `ALTER TABLE ... DELETE WHERE org_id = ?` is safe to repeat — a second
//!    call on already-deleted rows is a no-op mutation. A row that already
//!    has zero CH rows (replay after a PG-side crash) skips the ALTER
//!    entirely and just settles the queue row.

use std::time::Duration;

use anyhow::{Context, anyhow};
use clickhouse::Client as ChClient;
use futures::future::join_all;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{OrgId, UserId};
use crate::error::Result;
use crate::storage::clickhouse::count_org_rows;
use crate::storage::locks::{advisory_xact_lock, user_delete_lock_key};
use crate::storage::orgs;

/// Hard cap on orgs processed per tick. Small enough that a stuck ClickHouse
/// can't accumulate a huge backlog between alerts; large enough that a normal
/// week of churn drains in one or two ticks.
const PURGE_BATCH_LIMIT: i64 = 10;

/// Drain a chunk this big each tick. ClickHouse mutations are cheap to enqueue
/// but the server queues them serially, so going wider than this just trades
/// PG round-trips for CH mutation backlog.
const DRAIN_BATCH_LIMIT: i64 = 50;

/// Upper bound on one org's synchronous CH erasure (both tables + verify). A
/// healthy single-org mutation is sub-second; this only bites when CH is
/// stuck, in which case we abandon the row (left pending, retried next tick)
/// rather than letting the daily job hang. Correctness is unaffected — a row
/// is never marked complete until `count()` proves the data is gone.
const DRAIN_ROW_TIMEOUT: Duration = Duration::from_secs(60);

/// Every ClickHouse table carrying per-org data. Erasure must clear all of them
/// before a queue row may settle.
const CH_TENANT_TABLES: [&str; 5] = [
    "check_results",
    "check_results_1m",
    "check_results_1h",
    "flow_runs",
    "heartbeat_pings",
];

/// Run one full purge cycle: cascade PG-side deletes for past-grace orgs,
/// then drain whatever pending CH purges exist (including ones enqueued on
/// previous ticks that didn't succeed).
///
/// **Invocation contract:** in production this is called only from
/// [`crate::jobs::retention::run`], whose tick body sits inside
/// [`crate::storage::locks::try_job`]. Two concurrent ticks of this work
/// would race on the same `(org_id, ALTER … DELETE)` pairs in ClickHouse;
/// the lock is the single defence. Add a new caller only behind the same
/// lock — or no lock at all if the caller is the tests, which carry their
/// own database.
pub async fn purge_tick(pool: &PgPool, ch: &ChClient, grace_days: u32) -> Result<PurgeStats> {
    let cascaded = cascade_past_grace(pool, grace_days).await?;
    let drained = drain_clickhouse_purge_queue(pool, ch).await?;
    // Users last: an org the user solo-owns may be cascaded above in the
    // same tick, so the user purge runs against already-settled org state.
    let users = purge_users_past_grace(pool, grace_days).await?;
    // A purged owner leaves their account behind (the FK is SET NULL so a
    // cascade can never reach a live org through it); once its orgs are gone
    // too, nothing references it.
    let accounts = crate::storage::accounts::reap_orphaned(pool).await?;
    if accounts > 0 {
        tracing::info!(accounts, "orphaned accounts reaped");
    }
    Ok(PurgeStats {
        cascaded,
        drained,
        users,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeStats {
    pub cascaded: u32,
    pub drained: u32,
    pub users: u32,
}

/// PG-side step: pick orgs past the grace window, enqueue + cascade in one
/// transaction per org. Returns the count actually cascaded so the caller can
/// emit a metric.
async fn cascade_past_grace(pool: &PgPool, grace_days: u32) -> Result<u32> {
    let orgs: Vec<(Uuid,)> = sqlx::query_as(
        r#"SELECT id FROM organizations
           WHERE deleted_at IS NOT NULL
             AND deleted_at < now() - ($1::int * INTERVAL '1 day')
           ORDER BY deleted_at ASC
           LIMIT $2"#,
    )
    .bind(i64::from(grace_days))
    .bind(PURGE_BATCH_LIMIT)
    .fetch_all(pool)
    .await
    .context("purge: select past-grace orgs")?;

    let mut cascaded = 0u32;
    for (org_id,) in orgs {
        let mut tx = pool.begin().await.context("purge: begin tx")?;

        // Lock every owner (ordered by id, deadlock-free) so a concurrent
        // `undelete_in_tx` can't lift this org's tombstone between the re-check
        // and the DELETE below — otherwise the cascade wipes a recovered org.
        let owners = orgs::owner_user_ids(&mut *tx, OrgId(org_id)).await?;
        for owner in &owners {
            advisory_xact_lock(&mut *tx, &user_delete_lock_key(*owner))
                .await
                .context("purge: owner delete lock")?;
        }

        sqlx::query(
            r#"INSERT INTO clickhouse_purge_queue (org_id)
               VALUES ($1)
               ON CONFLICT (org_id) DO NOTHING"#,
        )
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .context("purge: enqueue ch")?;

        // ON DELETE CASCADE on every tenant table empties PG-side data.
        //
        // The `deleted_at` predicate is load-bearing, not cosmetic: the SELECT
        // above runs outside any transaction, so a restore commit
        // (`auth::account::undelete_in_tx`, which sets `deleted_at = NULL`)
        // between the SELECT and this DELETE would otherwise wipe a recovered org
        // (and via FK CASCADE, every tenant row it owns). Re-checking the
        // grace-window predicate inside the transaction makes the DELETE a
        // no-op for any row that has since been restored.
        let affected = sqlx::query(
            r#"DELETE FROM organizations
                WHERE id = $1
                  AND deleted_at IS NOT NULL
                  AND deleted_at < now() - ($2::int * INTERVAL '1 day')"#,
        )
        .bind(org_id)
        .bind(i64::from(grace_days))
        .execute(&mut *tx)
        .await
        .context("purge: cascade delete")?
        .rows_affected();

        if affected == 0 {
            // Recovery raced us between the SELECT and the DELETE. Roll back
            // so the queue INSERT above also vanishes — otherwise the CH-side
            // drain would happily erase a *live* org's check_results.
            tx.rollback().await.context("purge: rollback cascade")?;
            tracing::info!(%org_id, "cascade skipped: org recovered after select");
            continue;
        }
        tx.commit().await.context("purge: commit cascade")?;
        // The page resolver 404s a purged org's pages (slug lookup joins the
        // now-deleted rows), so a stale cache entry is unreachable and ages out
        // on TTL — no explicit invalidation needed.
        cascaded += 1;
        tracing::info!(%org_id, "org cascade-purged from postgres");
    }
    Ok(cascaded)
}

/// CH-side step: drain pending queue rows. For each org every
/// [`CH_TENANT_TABLES`] entry is erased with a synchronous mutation, then a
/// `count()` confirms zero rows
/// remain — only then is the queue row settled. Any error, timeout, or
/// still-present rows leave the row pending with `attempts`/`last_error`
/// bumped so the next tick retries.
pub async fn drain_clickhouse_purge_queue(pool: &PgPool, ch: &ChClient) -> Result<u32> {
    let pending: Vec<(Uuid,)> = sqlx::query_as(
        r#"SELECT org_id FROM clickhouse_purge_queue
           WHERE completed_at IS NULL
           ORDER BY queued_at ASC
           LIMIT $1"#,
    )
    .bind(DRAIN_BATCH_LIMIT)
    .fetch_all(pool)
    .await
    .context("purge: select pending queue")?;

    let mut drained = 0u32;
    for (org_id,) in pending {
        match tokio::time::timeout(DRAIN_ROW_TIMEOUT, erase_org_from_clickhouse(ch, org_id)).await {
            Ok(Ok(())) => {
                mark_queue_complete(pool, org_id).await?;
                drained += 1;
                tracing::info!(%org_id, "clickhouse purge complete");
            }
            Ok(Err(err)) => {
                let msg = format!("{err:?}");
                mark_queue_attempt(pool, org_id, &msg).await?;
                tracing::warn!(%org_id, err = %msg, "clickhouse purge failed; will retry");
            }
            Err(_) => {
                let msg = format!("timed out after {}s", DRAIN_ROW_TIMEOUT.as_secs());
                mark_queue_attempt(pool, org_id, &msg).await?;
                tracing::warn!(%org_id, "clickhouse purge timed out; will retry");
            }
        }
    }
    Ok(drained)
}

/// Erase one org from every CH tenant table and prove it.
///
/// 1. Count first. If the org already has zero rows everywhere — a replay
///    after a PG-side crash, or a row left pending by an earlier failed
///    verify — settle without issuing any mutation. This is what keeps a
///    stuck ClickHouse from accumulating one fresh no-op `ALTER` per table
///    per daily tick on top of an already-jammed mutation queue.
/// 2. Otherwise issue `ALTER … DELETE` per table with `mutations_sync=2`, so
///    each call returns only once applied (single-node ⇒ "applied on this
///    server").
/// 3. Re-count. Zero is the authoritative gate — it, not the mutation ack,
///    is what backs the "data fully erased" privacy claim, independent of
///    mutation-mode semantics or server version.
async fn erase_org_from_clickhouse(ch: &ChClient, org_id: Uuid) -> Result<()> {
    if count_org_total(ch, org_id).await? == 0 {
        return Ok(());
    }

    // Distinct tables, no shared server-side lock; run in parallel and
    // collect every error rather than short-circuiting on the first.
    let errs: Vec<String> =
        join_all(CH_TENANT_TABLES.map(|t| async move { (t, alter_delete(ch, t, org_id).await) }))
            .await
            .into_iter()
            .filter_map(|(t, r)| r.err().map(|e| format!("{t}: {e}")))
            .collect();
    if !errs.is_empty() {
        return Err(anyhow!("alter delete: {}", errs.join(" | ")).into());
    }

    let remaining = count_org_total(ch, org_id).await?;
    if remaining != 0 {
        return Err(
            anyhow!("mutation accepted but {remaining} row(s) still present for org").into(),
        );
    }
    Ok(())
}

/// Total live rows for one org across every CH tenant table. Zero proves the
/// org's check data is gone. Per-table counts run concurrently.
async fn count_org_total(ch: &ChClient, org_id: Uuid) -> Result<u64> {
    let counts = join_all(CH_TENANT_TABLES.map(|t| count_org_rows(ch, t, org_id))).await;
    counts.into_iter().sum()
}

/// Synchronous `ALTER … DELETE` for one org. `table` is a fixed in-crate
/// constant (never user input), so the format interpolation is injection-free.
async fn alter_delete(
    ch: &ChClient,
    table: &str,
    org_id: Uuid,
) -> std::result::Result<(), clickhouse::error::Error> {
    ch.query(&format!("ALTER TABLE {table} DELETE WHERE org_id = ?"))
        .with_setting("mutations_sync", "2")
        .bind(org_id)
        .execute()
        .await
}

async fn mark_queue_complete(pool: &PgPool, org_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE clickhouse_purge_queue SET completed_at = now() WHERE org_id = $1")
        .bind(org_id)
        .execute(pool)
        .await
        .context("purge: mark queue complete")?;
    Ok(())
}

async fn mark_queue_attempt(pool: &PgPool, org_id: Uuid, err: &str) -> Result<()> {
    sqlx::query(
        "UPDATE clickhouse_purge_queue
         SET attempts = attempts + 1, last_error = $2
         WHERE org_id = $1",
    )
    .bind(org_id)
    .bind(err)
    .execute(pool)
    .await
    .context("purge: mark queue attempt")?;
    Ok(())
}

/// Depth of the not-yet-drained CH purge queue, plus the age of its oldest
/// entry. Surfaced as gauges so an alert can fire when erasure is stuck —
/// a backlog that keeps growing means the "erased within 30 days" promise is
/// at risk well before the window closes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueDepth {
    pub pending: u64,
    pub oldest_age_secs: i64,
}

pub async fn purge_queue_depth<'e>(executor: impl sqlx::PgExecutor<'e>) -> Result<QueueDepth> {
    let (pending, oldest_age): (i64, Option<f64>) = sqlx::query_as(
        r#"SELECT count(*)::bigint,
                  EXTRACT(EPOCH FROM (now() - min(queued_at)))::float8
           FROM clickhouse_purge_queue
           WHERE completed_at IS NULL"#,
    )
    .fetch_one(executor)
    .await
    .context("purge: queue depth")?;
    Ok(QueueDepth {
        // `count(*)::bigint` is provably non-negative, so the conversion can't
        // fail; saturate high (not 0) on the impossible path so a broken
        // metric reads as "backlog", never as a false all-clear.
        pending: u64::try_from(pending).unwrap_or(u64::MAX),
        oldest_age_secs: oldest_age.unwrap_or(0.0) as i64,
    })
}

/// Hard-delete soft-deleted users whose grace window has elapsed. The window
/// is gated solely by `deleted_at` age — within it the user can still restore.
/// The same `PURGE_BATCH_LIMIT` back-pressure as the org
/// cascade caps a runaway mass-deletion. The `users` FK cascade erases
/// dependent rows; `login_attempts` / `org_audit_log` keep theirs with the
/// actor nulled.
///
/// Each row is deleted under the per-user delete lock (the lock
/// `undelete_in_tx` takes), so a restore serialises with the purge; the in-tx
/// re-check no-ops a row restored after the select.
pub async fn purge_users_past_grace(pool: &PgPool, grace_days: u32) -> Result<u32> {
    let candidates: Vec<(Uuid,)> = sqlx::query_as(
        r#"SELECT id FROM users
            WHERE deleted_at IS NOT NULL
              AND deleted_at < now() - ($1::int * INTERVAL '1 day')
            ORDER BY deleted_at ASC
            LIMIT $2"#,
    )
    .bind(i64::from(grace_days))
    .bind(PURGE_BATCH_LIMIT)
    .fetch_all(pool)
    .await
    .context("purge: select past-grace users")?;

    let mut purged = 0u32;
    for (id,) in candidates {
        let mut tx = pool.begin().await.context("purge: begin user tx")?;
        advisory_xact_lock(&mut *tx, &user_delete_lock_key(UserId(id)))
            .await
            .context("purge: user delete lock")?;
        let affected = sqlx::query(
            r#"DELETE FROM users
                WHERE id = $1
                  AND deleted_at IS NOT NULL
                  AND deleted_at < now() - ($2::int * INTERVAL '1 day')"#,
        )
        .bind(id)
        .bind(i64::from(grace_days))
        .execute(&mut *tx)
        .await
        .context("purge: delete past-grace user")?
        .rows_affected();
        tx.commit().await.context("purge: commit user delete")?;
        if affected > 0 {
            purged += 1;
            tracing::debug!(%id, "user hard-purged");
        } else {
            tracing::info!(%id, "user purge skipped: restored after select");
        }
    }
    Ok(purged)
}
