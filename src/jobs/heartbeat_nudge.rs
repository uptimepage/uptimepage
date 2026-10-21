//! One reminder for a heartbeat nobody finished wiring up.
//!
//! A heartbeat is not evaluated until its first ping, so an unwired one is
//! silent. That is the right answer to "has this job failed" and the wrong
//! answer to "did you forget about me".

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::email::EmailTemplate;
use crate::jobs::owner_mail::{OwnerMail, RECIPIENT, RECIPIENT_JOINS, StampFailures};

/// Long enough that a monitor created ahead of a deploy is not nagged mid-work,
/// short enough that the reminder still lands while the intent is fresh. Dead
/// Man's Snitch settled on the same three days.
const NUDGE_AFTER_DAYS: i32 = 3;

/// Runaway guard, not a page size: the qualifying population is tiny, and
/// anything over the cap is picked up next tick.
const BATCH: i64 = 200;

/// Everything one reminder needs, resolved in a single statement so the sweep
/// does not fan out a query per monitor.
#[derive(sqlx::FromRow)]
struct Candidate {
    org_id: Uuid,
    target_id: Uuid,
    monitor_name: String,
    org_name: Option<String>,
    created_at: DateTime<Utc>,
    recipient: String,
}

/// Sends one reminder per unwired heartbeat and returns how many went out.
/// `nudged_at` is stamped only after the provider accepts the mail, so a
/// transport outage retries next tick rather than losing the reminder. A stamp
/// that fails costs a duplicate, bounded by
/// [`STAMP_FAILURE_LIMIT`](crate::jobs::owner_mail::STAMP_FAILURE_LIMIT).
pub async fn nudge_unwired_heartbeats(pool: &PgPool, cfg: &OwnerMail) -> Result<u64, sqlx::Error> {
    let candidates: Vec<Candidate> = sqlx::query_as(&format!(
        "SELECT hm.org_id, hm.target_id, t.name AS monitor_name, o.name AS org_name, hm.created_at, \
                {RECIPIENT} AS recipient \
           FROM heartbeat_monitors hm \
           JOIN targets t ON t.id = hm.target_id AND t.org_id = hm.org_id \
           JOIN organizations o ON o.id = hm.org_id AND o.deleted_at IS NULL \
            AND t.plan_hold_at IS NULL \
           {RECIPIENT_JOINS} \
          WHERE hm.first_ping_at IS NULL \
            AND hm.nudged_at IS NULL \
            AND t.enabled \
            AND hm.created_at < now() - make_interval(days => $1) \
            AND {RECIPIENT} IS NOT NULL \
          ORDER BY hm.created_at \
          LIMIT $2"
    ))
    .bind(NUDGE_AFTER_DAYS)
    .bind(BATCH)
    .fetch_all(pool)
    .await?;

    let mut sent = 0;
    let mut stamps = StampFailures::default();
    for c in candidates {
        if !send_one(&c, cfg).await {
            continue;
        }
        sent += 1;
        let stamped = sqlx::query(
            "UPDATE heartbeat_monitors SET nudged_at = now() \
             WHERE org_id = $1 AND target_id = $2",
        )
        .bind(c.org_id)
        .bind(c.target_id)
        .execute(pool)
        .await;
        if stamps.should_stop(stamped, "heartbeat nudge", sent) {
            break;
        }
    }
    Ok(sent)
}

async fn send_one(c: &Candidate, cfg: &OwnerMail) -> bool {
    let template = EmailTemplate::HeartbeatNeverPinged {
        monitor_name: c.monitor_name.clone(),
        waiting_secs: (Utc::now() - c.created_at).num_seconds().max(0),
        monitor_url: cfg.monitor_url(c.target_id),
        docs_url: cfg.docs_url.clone(),
        org_name: c.org_name.clone(),
    };
    let outgoing = cfg.to(&c.recipient, template);
    match cfg.email.sender.send(outgoing).await {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(
                target_id = %c.target_id,
                error = %err,
                "heartbeat nudge not sent, retrying next tick"
            );
            false
        }
    }
}
