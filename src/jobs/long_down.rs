//! Long-down monitors: down from every region for [`LONG_DOWN`]`.after`, so
//! checked every [`LONG_DOWN`]`.interval` until they answer again.

use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::OrgId;
use crate::domain::long_down::LONG_DOWN;
use crate::email::templates::monitors_slowed::SlowedMonitor;
use crate::email::{EmailTemplate, SendClass};
use crate::error::Result;
use crate::jobs::owner_mail::{OwnerMail, RECIPIENT, RECIPIENT_JOINS, StampFailures};
use crate::metric_names;
use crate::quotas::QuotaService;
use crate::quotas::effective::{governed_interval, resolve_plans};
use crate::storage::admin::{NOT_HELD_PREDICATE, PROBED_KIND_PREDICATE};
use crate::storage::maintenance::window_spans_sql;
use crate::storage::traits::ResultsStore;

/// `$1` is always `LONG_DOWN.after` in seconds.
const SLOW_START: &str = "(now() - $1::bigint * interval '1 second')";
const SLOWED_START: &str = "(t.slowed_at - $1::bigint * interval '1 second')";
const LIVE_ORG: &str =
    "EXISTS (SELECT 1 FROM organizations o WHERE o.id = t.org_id AND o.deleted_at IS NULL)";

fn edited_since(start: &str) -> String {
    format!("t.updated_at > {start}")
}

fn down_since(start: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM incidents i \
                 WHERE i.org_id = t.org_id AND i.target_id = t.id AND i.origin = 'monitor' \
                   AND i.ended_at IS NULL AND i.started_at <= {start})"
    )
}

fn maintained_since(start: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM ({spans}) s WHERE s.span_start <= now() AND s.span_end >= {start})",
        spans = window_spans_sql("t.org_id", "ARRAY[t.id]"),
    )
}

/// The Postgres half of long-down over the window from `start` to now.
fn long_down_sql(start: &str) -> String {
    format!("{} AND {}", unattended_sql(start), down_since(start))
}

/// [`long_down_sql`] without the incident, for a read that starts from it.
fn unattended_sql(start: &str) -> String {
    format!(
        "t.enabled AND {NOT_HELD_PREDICATE} AND {PROBED_KIND_PREDICATE} AND {LIVE_ORG} \
         AND NOT {edited} AND NOT {maintained}",
        edited = edited_since(start),
        maintained = maintained_since(start),
    )
}

fn after_secs() -> i64 {
    i64::try_from(LONG_DOWN.after.as_secs()).unwrap_or(i64::MAX)
}

/// The rollup bucket holding `at`. Every result read is whole buckets, so a
/// window is only good to the hour.
fn hour_of(at: DateTime<Utc>) -> DateTime<Utc> {
    at.duration_trunc(TimeDelta::hours(1)).unwrap_or(at)
}

fn window_start(end: DateTime<Utc>) -> DateTime<Utc> {
    hour_of(end - TimeDelta::seconds(after_secs()))
}

/// The hourly pass: slows what is long-down, then mails whoever has not heard
/// about theirs. Returns how many were slowed.
pub async fn sweep(
    pool: &PgPool,
    results: &dyn ResultsStore,
    quotas: &QuotaService,
    mail: &OwnerMail,
) -> Result<u64> {
    let slowed = slow_pass(pool, results, quotas).await;
    match mail_pass(pool, mail).await {
        Ok(0) => {}
        Ok(sent) => tracing::info!(sent, "long-down mail"),
        Err(err) => tracing::warn!(error = %err, "long-down mail pass failed"),
    }
    slowed
}

/// Slows every monitor that is long-down now and returns how many.
pub async fn slow_pass(
    pool: &PgPool,
    results: &dyn ResultsStore,
    quotas: &QuotaService,
) -> Result<u64> {
    #[derive(sqlx::FromRow)]
    struct Candidate {
        id: Uuid,
        org_id: Uuid,
        interval_secs: i32,
        kind: Option<String>,
    }
    let candidates: Vec<Candidate> = sqlx::query_as(&format!(
        "SELECT t.id, t.org_id, t.interval_secs, t.kind \
         FROM incidents i \
         JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id \
         WHERE i.ended_at IS NULL AND i.origin = 'monitor' AND i.started_at <= {SLOW_START} \
           AND t.slowed_at IS NULL AND {}",
        unattended_sql(SLOW_START)
    ))
    .bind(after_secs())
    .fetch_all(pool)
    .await
    .context("long-down candidates")?;
    if candidates.is_empty() {
        return Ok(0);
    }

    let orgs: Vec<OrgId> = candidates.iter().map(|c| OrgId(c.org_id)).collect();
    let plans = resolve_plans(quotas, orgs).await;
    // An org whose plan did not resolve is judged next pass.
    let faster: Vec<(OrgId, Uuid)> = candidates
        .iter()
        .filter(|c| {
            let Some(Some(plan)) = plans.get(&OrgId(c.org_id)) else {
                return false;
            };
            let stored = Duration::from_secs(u64::try_from(c.interval_secs).unwrap_or(0));
            let kind = c.kind.as_deref().unwrap_or_default();
            governed_interval(stored, plan, kind) < LONG_DOWN.interval
        })
        .map(|c| (OrgId(c.org_id), c.id))
        .collect();
    if faster.is_empty() {
        return Ok(0);
    }

    // The clock the Postgres conditions are judged by, so both windows start
    // on the same hour.
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(pool)
        .await
        .context("database clock")?;
    let marks = results.pass_marks(&faster, window_start(now)).await?;
    // A monitor no region reported on lately is silence detection's to judge.
    let reported_since = hour_of(now) - TimeDelta::hours(1);
    let ids: Vec<Uuid> = faster
        .iter()
        .map(|(_, id)| *id)
        .filter(|id| {
            marks
                .get(id)
                .is_some_and(|m| m.last_passed.is_none() && m.last_checked >= reported_since)
        })
        .collect();
    if ids.is_empty() {
        return Ok(0);
    }

    let slowed = sqlx::query(&format!(
        "UPDATE targets t SET slowed_at = now() \
         WHERE t.id = ANY($2) AND t.slowed_at IS NULL AND {} \
         /* SAFE: fleet-wide job, ids come from the cross-tenant candidate read */",
        long_down_sql(SLOW_START)
    ))
    .bind(after_secs())
    .bind(&ids)
    .execute(pool)
    .await
    .context("slow long-down monitors")?
    .rows_affected();
    metrics::counter!(metric_names::LONG_DOWN_SLOWED).increment(slowed);
    Ok(slowed)
}

/// Puts back on its own interval every slowed monitor whose incident closed,
/// or that a maintenance window started over, since it was slowed, and returns
/// how many. An edit clears it in Postgres, and a passing check someone ran
/// clears it at once.
pub async fn restore_pass(pool: &PgPool) -> Result<u64> {
    // The reasons are judged against the slow episode the row holds, so they
    // are read before the row is cleared, in the same statement.
    let restored: Vec<(bool,)> = sqlx::query_as(&format!(
        "WITH due AS ( \
             SELECT t.id, t.slowed_at, NOT {down} AS incident_gone FROM targets t \
             WHERE t.slowed_at IS NOT NULL AND {LIVE_ORG} AND (NOT {down} OR {maintained})) \
         UPDATE targets t SET slowed_at = NULL FROM due \
         WHERE t.id = due.id AND t.slowed_at = due.slowed_at \
         RETURNING due.incident_gone \
         /* SAFE: fleet-wide job over every slowed monitor */",
        down = down_since(SLOWED_START),
        maintained = maintained_since(SLOWED_START),
    ))
    .bind(after_secs())
    .fetch_all(pool)
    .await
    .context("restore long-down monitors")?;
    for (incident_gone,) in &restored {
        let reason = if *incident_gone {
            "incident"
        } else {
            "maintenance"
        };
        metrics::counter!(metric_names::LONG_DOWN_RESTORED, "reason" => reason).increment(1);
    }
    Ok(restored.len() as u64)
}

/// One mail per recipient listing every slowed monitor they have not heard
/// about, each stamped only after the provider accepts. Returns mails sent.
pub async fn mail_pass(pool: &PgPool, cfg: &OwnerMail) -> Result<u64> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        name: String,
        org_name: Option<String>,
        recipient: String,
    }
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT t.id, t.name, o.name AS org_name, {RECIPIENT} AS recipient \
           FROM targets t \
           JOIN organizations o ON o.id = t.org_id AND o.deleted_at IS NULL \
           {RECIPIENT_JOINS} \
          WHERE t.slowed_at IS NOT NULL \
            AND (t.slowed_notified_at IS NULL OR t.slowed_notified_at < t.slowed_at) \
            AND t.enabled AND {NOT_HELD_PREDICATE} \
            AND {RECIPIENT} IS NOT NULL \
          ORDER BY recipient, o.name, t.name \
          /* SAFE: fleet-wide job, each mail goes to the monitor's own owner */"
    ))
    .fetch_all(pool)
    .await
    .context("slowed monitors to mail")?;

    let mut sent = 0;
    let mut stamps = StampFailures::default();
    for group in rows.chunk_by(|a, b| a.recipient == b.recipient) {
        let monitors = group
            .iter()
            .map(|r| SlowedMonitor {
                name: r.name.clone(),
                org_name: r.org_name.clone(),
                url: cfg.monitor_url(r.id),
            })
            .collect();
        let template = EmailTemplate::MonitorsSlowed {
            monitors,
            docs_url: cfg.docs_url.clone(),
        };
        if let Err(err) = cfg
            .email
            .sender
            .send(cfg.to(&group[0].recipient, template), SendClass::Bulk)
            .await
        {
            metrics::counter!(metric_names::LONG_DOWN_MAIL, "outcome" => "failed").increment(1);
            tracing::warn!(error = %err, "long-down mail not sent, retrying next pass");
            // A spent or unreachable send budget fails the rest of the pass too.
            if err.is_pacing() {
                break;
            }
            continue;
        }
        metrics::counter!(metric_names::LONG_DOWN_MAIL, "outcome" => "sent").increment(1);
        sent += 1;
        let ids: Vec<Uuid> = group.iter().map(|r| r.id).collect();
        let stamped = sqlx::query(
            "UPDATE targets SET slowed_notified_at = now() WHERE id = ANY($1) \
             /* SAFE: ids come from the fleet-wide read above */",
        )
        .bind(&ids)
        .execute(pool)
        .await;
        if stamps.should_stop(stamped, "long-down mail", sent) {
            break;
        }
    }
    Ok(sent)
}
