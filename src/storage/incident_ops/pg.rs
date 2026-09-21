//! Postgres [`IncidentOpsStore`]: the state machine, its activity log, and the
//! console read model. Every statement filters `org_id`, and transitions run
//! under a per-incident advisory lock.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    ActorType, IncidentEvent, IncidentEventKind, IncidentMetrics, IncidentNotification,
    IncidentOrigin, IncidentSeverity, IncidentState, IncidentTransition, IncidentUrgency,
    IncidentVisibility, MetricBucket, MonitorIncidentCount, NewIncidentNotification,
    NewManualIncident, NotificationOutcome, NotificationReason, NotificationStatus, OpsIncident,
    OrgId, UserId, next_state,
};
use crate::error::Result;
use crate::storage::locks::{advisory_xact_lock, incident_lock_key};

use super::{
    AUTO_RESOLVED_MESSAGE, Actor, DueIncident, EmergencyAck, INCIDENT_DETAIL_ROW_CAP,
    IncidentOpsFilter, IncidentOpsStore, IncidentStateCounts, LifecycleOutcome,
    PendingNotification, QUEUED_TAKEOVER_SECS, opening_update_message,
};

pub struct PgIncidentOpsStore {
    pool: PgPool,
}

impl PgIncidentOpsStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Columns selected for an [`OpsIncident`]. Kept in one place so every
/// `RETURNING` / `SELECT` stays in lockstep with [`OpsIncidentRow`].
const OPS_COLS: &str = "id, target_id, title, state, severity, urgency, origin, visibility, \
     paging_enabled, counts_as_downtime, started_at, ended_at, acknowledged_at, \
     acknowledged_by, assigned_to, resolved_by, escalation_policy_id, escalation_level, \
     escalation_round, next_escalation_at, \
     check_count, error_sample, regions_down, regions_up, created_at, updated_at";

/// A `%…%` `LIKE` pattern with the operator's wildcards neutralised, so a
/// literal `%` or `_` in the search box matches itself, not everything.
fn like_contains(s: &str) -> String {
    format!(
        "%{}%",
        s.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

/// Public resolution line: the note, or a default when blank.
pub(super) fn resolved_public_message(note: Option<&str>) -> String {
    match note.map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => "This incident has been resolved.".to_string(),
    }
}

#[derive(sqlx::FromRow)]
struct OpsIncidentRow {
    id: Uuid,
    target_id: Option<Uuid>,
    title: Option<String>,
    state: String,
    severity: String,
    urgency: String,
    origin: String,
    visibility: String,
    paging_enabled: bool,
    counts_as_downtime: bool,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    acknowledged_at: Option<DateTime<Utc>>,
    acknowledged_by: Option<UserId>,
    assigned_to: Option<UserId>,
    resolved_by: Option<UserId>,
    escalation_policy_id: Option<Uuid>,
    escalation_level: i32,
    escalation_round: i32,
    next_escalation_at: Option<DateTime<Utc>>,
    check_count: i32,
    error_sample: Option<String>,
    regions_down: Option<Vec<String>>,
    regions_up: Option<Vec<String>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn row_to_ops(r: OpsIncidentRow) -> OpsIncident {
    OpsIncident {
        id: r.id,
        target_id: r.target_id,
        title: r.title,
        state: IncidentState::from_db_str(&r.state),
        severity: IncidentSeverity::from_db_str(&r.severity),
        urgency: IncidentUrgency::from_db_str(&r.urgency),
        origin: IncidentOrigin::from_db_str(&r.origin),
        visibility: IncidentVisibility::from_db_str(&r.visibility),
        paging_enabled: r.paging_enabled,
        counts_as_downtime: r.counts_as_downtime,
        started_at: r.started_at,
        ended_at: r.ended_at,
        acknowledged_at: r.acknowledged_at,
        acknowledged_by: r.acknowledged_by,
        assigned_to: r.assigned_to,
        resolved_by: r.resolved_by,
        escalation_policy_id: r.escalation_policy_id,
        escalation_level: r.escalation_level,
        escalation_round: r.escalation_round,
        next_escalation_at: r.next_escalation_at,
        check_count: r.check_count.max(0) as u64,
        error_sample: r.error_sample,
        regions_down: r.regions_down.unwrap_or_default(),
        regions_up: r.regions_up.unwrap_or_default(),
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

#[derive(sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    incident_id: Uuid,
    occurred_at: DateTime<Utc>,
    kind: String,
    actor_type: String,
    actor_id: Option<UserId>,
    detail: Value,
    message: Option<String>,
}

fn row_to_event(r: EventRow) -> IncidentEvent {
    IncidentEvent {
        id: r.id,
        incident_id: r.incident_id,
        occurred_at: r.occurred_at,
        kind: IncidentEventKind::from_db_str(&r.kind),
        actor_type: ActorType::from_db_str(&r.actor_type),
        actor_id: r.actor_id,
        detail: r.detail,
        message: r.message,
    }
}

const NOTIF_COLS: &str = "id, incident_id, escalation_level, target_user_id, channel_id, \
     transport, reason, status, attempt, error, created_at, sent_at, next_attempt_at, \
     provider_receipt, acked_at";

#[derive(sqlx::FromRow)]
struct NotifRow {
    id: Uuid,
    incident_id: Uuid,
    escalation_level: Option<i32>,
    target_user_id: Option<UserId>,
    channel_id: Option<Uuid>,
    transport: String,
    reason: String,
    status: String,
    attempt: i32,
    error: Option<String>,
    created_at: DateTime<Utc>,
    sent_at: Option<DateTime<Utc>>,
    next_attempt_at: Option<DateTime<Utc>>,
    provider_receipt: Option<String>,
    acked_at: Option<DateTime<Utc>>,
}

fn row_to_notif(r: NotifRow) -> IncidentNotification {
    IncidentNotification {
        id: r.id,
        incident_id: r.incident_id,
        escalation_level: r.escalation_level,
        target_user_id: r.target_user_id,
        channel_id: r.channel_id,
        transport: r.transport,
        reason: NotificationReason::from_db_str(&r.reason),
        status: NotificationStatus::from_db_str(&r.status),
        attempt: r.attempt,
        error: r.error,
        created_at: r.created_at,
        sent_at: r.sent_at,
        next_attempt_at: r.next_attempt_at,
        provider_receipt: r.provider_receipt,
        acked_at: r.acked_at,
    }
}

/// Insert one internal timeline entry inside an open transaction. Assumes the
/// caller already proved the incident exists in `org` (e.g. via the locking
/// `SELECT ... FOR UPDATE`), so it binds `org.0` directly.
async fn insert_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    incident_id: Uuid,
    kind: IncidentEventKind,
    actor: Actor,
    message: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO incident_events (org_id, incident_id, kind, actor_type, actor_id, message)
           VALUES ($1, $2, $3, $4, $5, $6)"#,
    )
    .bind(org.0)
    .bind(incident_id)
    .bind(kind.as_db_str())
    .bind(actor.actor_type().as_db_str())
    .bind(actor.user_id())
    .bind(message)
    .execute(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("insert incident_event: {e}"))?;
    Ok(())
}

/// Mirror an operator's incident action into `org_audit_log`: incidents and
/// their events cascade away with the monitor, leaving no trace the incident
/// existed. System transitions are skipped, or every recovery writes a row.
/// An actor with no user id still writes one, with its kind in the metadata to
/// say why `actor_id` is null.
async fn record_incident_audit_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    actor: Actor,
    action: &str,
    mut metadata: Value,
) -> Result<()> {
    if matches!(actor, Actor::System) {
        return Ok(());
    }
    if let Value::Object(map) = &mut metadata {
        map.insert(
            "actor_type".to_string(),
            Value::from(actor.actor_type().as_db_str()),
        );
    }
    crate::storage::orgs::record_audit_tx(tx, org, actor.user_id(), action, metadata).await
}

/// Counted off the append-only timeline rather than a column of its own, so it
/// cannot run backwards: nothing deletes `incident_events` short of the
/// incident itself cascading away, which voids every link to it anyway.
async fn reopen_count_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM incident_events \
         WHERE incident_id = $1 AND org_id = $2 AND kind = 'reopened'",
    )
    .bind(id)
    .bind(org.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("reopen count: {e}").into())
}

async fn incident_was_published(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM incident_events \
         WHERE incident_id = $1 AND org_id = $2 AND kind = 'published')",
    )
    .bind(id)
    .bind(org.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("published-event check: {e}").into())
}

impl PgIncidentOpsStore {
    /// Shared transition core: lock, read current state, apply the pure state
    /// machine, run `update` to mutate the row, then log `kind`.
    /// `public_resolution`, when set, appends a `resolved` public update when
    /// the incident is (or ever was) public.
    #[allow(clippy::too_many_arguments)]
    async fn transition(
        &self,
        org: OrgId,
        id: Uuid,
        transition: IncidentTransition,
        event_kind: IncidentEventKind,
        actor: Actor,
        note: Option<String>,
        update_sql: &str,
        public_resolution: Option<String>,
        expect_generation: Option<i64>,
    ) -> Result<LifecycleOutcome> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        advisory_xact_lock(&mut *tx, &incident_lock_key(id))
            .await
            .map_err(|e| anyhow::anyhow!("incident lock: {e}"))?;

        let current: Option<(String,)> =
            sqlx::query_as("SELECT state FROM incidents WHERE id = $1 AND org_id = $2 FOR UPDATE")
                .bind(id)
                .bind(org.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("load incident state: {e}"))?;
        let Some((state_str,)) = current else {
            return Ok(LifecycleOutcome::NotFound);
        };
        // Under the transition's own locks, so a reopen cannot slip between
        // this check and the update.
        if let Some(expected) = expect_generation
            && reopen_count_tx(&mut tx, org, id).await? != expected
        {
            return Ok(LifecycleOutcome::Stale);
        }

        let from = IncidentState::from_db_str(&state_str);
        let to = match next_state(from, transition) {
            Ok(to) => to,
            Err(err) => return Ok(LifecycleOutcome::IllegalTransition(err)),
        };
        // A transition that moves nothing leaves the row alone, so the first
        // acknowledger keeps the credit an unknown one included, which the
        // update's `COALESCE` would otherwise backfill. It is still logged: an
        // engineer taking a page a notification already acknowledged is a real
        // action. The public update is not repeated, though — subscribers
        // should not read the same resolution twice.
        let unchanged = from == to;
        let row: OpsIncidentRow = if unchanged {
            let sql = format!("SELECT {OPS_COLS} FROM incidents WHERE id = $1 AND org_id = $2");
            sqlx::query_as(&sql)
                .bind(id)
                .bind(org.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("load unchanged incident: {e}"))?
        } else {
            match sqlx::query_as(update_sql)
                .bind(id)
                .bind(org.0)
                .bind(actor.user_id())
                .fetch_one(&mut *tx)
                .await
            {
                Ok(row) => row,
                // Reopen clears ended_at; if one of the same origin is already open
                // for the target, its unique index rejects it — surface 409.
                Err(e)
                    if matches!(
                        e.as_database_error().and_then(|d| d.constraint()),
                        Some("idx_incidents_org_open_monitor" | "idx_incidents_org_open_manual")
                    ) =>
                {
                    return Err(crate::error::AppError::conflict(
                        crate::error::codes::INCIDENT_ALREADY_OPEN,
                        "another incident is already open for this monitor",
                    ));
                }
                Err(e) => return Err(anyhow::anyhow!("apply transition: {e}").into()),
            }
        };

        insert_event_tx(&mut tx, org, id, event_kind, actor, note.as_deref()).await?;
        record_incident_audit_tx(
            &mut tx,
            org,
            actor,
            &format!("incident.{}", event_kind.as_db_str()),
            serde_json::json!({ "incident_id": id, "target_id": row.target_id }),
        )
        .await?;
        // An incident unpublished before it resolves still has subscribers who
        // were told it opened; write the closing update whenever it was ever
        // public, not only while currently public.
        if let Some(message) = public_resolution
            && !unchanged
            && (row.visibility == "public" || incident_was_published(&mut tx, org, id).await?)
        {
            let author = actor
                .user_id()
                .map(|u| u.0.to_string())
                .unwrap_or_else(|| "system".to_string());
            sqlx::query(
                r#"INSERT INTO incident_updates (org_id, incident_id, phase, message, author)
                   VALUES ($1, $2, 'resolved', $3, $4)"#,
            )
            .bind(org.0)
            .bind(id)
            .bind(message)
            .bind(author)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("resolve public update: {e}"))?;
        }
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(LifecycleOutcome::Updated(Box::new(row_to_ops(row))))
    }
}

#[async_trait]
impl IncidentOpsStore for PgIncidentOpsStore {
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<OpsIncident>> {
        let sql = format!("SELECT {OPS_COLS} FROM incidents WHERE id = $1 AND org_id = $2");
        let row: Option<OpsIncidentRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(org.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("get ops incident: {e}"))?;
        Ok(row.map(row_to_ops))
    }

    async fn list(&self, org: OrgId, filter: IncidentOpsFilter) -> Result<Vec<OpsIncident>> {
        let cap = filter.limit.unwrap_or(100).clamp(1, 1000) as i64;
        let off = filter.offset as i64;
        let state = filter.state.map(|s| s.as_db_str());
        let severity = filter.severity.map(|s| s.as_db_str());
        let assignee = filter.assignee.map(|u| u.0);
        let query = filter
            .query
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(like_contains);
        let sql = format!(
            "SELECT {OPS_COLS} FROM incidents \
             WHERE org_id = $1 AND ($2::text IS NULL OR state = $2) \
                 AND ($3::text IS NULL OR severity = $3) \
                 AND ($4::uuid IS NULL OR assigned_to = $4) \
                 AND ($7::text IS NULL OR title ILIKE $7 OR EXISTS ( \
                     SELECT 1 FROM targets t \
                      WHERE t.id = incidents.target_id AND t.name ILIKE $7)) \
             ORDER BY {} LIMIT $5 OFFSET $6",
            filter.sort.order_sql()
        );
        let rows: Vec<OpsIncidentRow> = sqlx::query_as(&sql)
            .bind(org.0)
            .bind(state)
            .bind(severity)
            .bind(assignee)
            .bind(cap)
            .bind(off)
            .bind(query)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("list ops incidents: {e}"))?;
        Ok(rows.into_iter().map(row_to_ops).collect())
    }

    async fn count(&self, org: OrgId, filter: &IncidentOpsFilter) -> Result<usize> {
        let state = filter.state.map(|s| s.as_db_str());
        let severity = filter.severity.map(|s| s.as_db_str());
        let assignee = filter.assignee.map(|u| u.0);
        let query = filter
            .query
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(like_contains);
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM incidents \
             WHERE org_id = $1 AND ($2::text IS NULL OR state = $2) \
                 AND ($3::text IS NULL OR severity = $3) \
                 AND ($4::uuid IS NULL OR assigned_to = $4) \
                 AND ($5::text IS NULL OR title ILIKE $5 OR EXISTS ( \
                     SELECT 1 FROM targets t \
                      WHERE t.id = incidents.target_id AND t.name ILIKE $5))",
        )
        .bind(org.0)
        .bind(state)
        .bind(severity)
        .bind(assignee)
        .bind(query)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("count ops incidents: {e}"))?;
        Ok(n.max(0) as usize)
    }

    async fn counts_by_state(
        &self,
        org: OrgId,
        filter: &IncidentOpsFilter,
    ) -> Result<IncidentStateCounts> {
        let severity = filter.severity.map(|s| s.as_db_str());
        let assignee = filter.assignee.map(|u| u.0);
        let query = filter
            .query
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(like_contains);
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT state, count(*) FROM incidents \
             WHERE org_id = $1 AND ($2::text IS NULL OR severity = $2) \
                 AND ($3::uuid IS NULL OR assigned_to = $3) \
                 AND ($4::text IS NULL OR title ILIKE $4 OR EXISTS ( \
                     SELECT 1 FROM targets t \
                      WHERE t.id = incidents.target_id AND t.name ILIKE $4)) \
             GROUP BY state",
        )
        .bind(org.0)
        .bind(severity)
        .bind(assignee)
        .bind(query)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("counts_by_state: {e}"))?;
        let mut c = IncidentStateCounts::default();
        for (state, n) in rows {
            let n = n.max(0) as usize;
            match IncidentState::from_db_str(&state) {
                IncidentState::Triggered => c.triggered = n,
                IncidentState::Acknowledged => c.acknowledged = n,
                IncidentState::Resolved => c.resolved = n,
            }
        }
        Ok(c)
    }

    async fn metrics(&self, org: OrgId, window_days: u32) -> Result<IncidentMetrics> {
        let since = Utc::now() - chrono::Duration::days(window_days.clamp(1, 365) as i64);

        #[derive(sqlx::FromRow)]
        struct Scalars {
            total: i64,
            mtta: Option<f64>,
            mttr: Option<f64>,
            auto_resolved: i64,
            human_resolved: i64,
        }
        let s: Scalars = sqlx::query_as(
            "SELECT count(*) AS total, \
                 (avg(extract(epoch FROM (acknowledged_at - started_at))) \
                     FILTER (WHERE acknowledged_at IS NOT NULL))::float8 AS mtta, \
                 (avg(extract(epoch FROM (ended_at - started_at))) \
                     FILTER (WHERE ended_at IS NOT NULL))::float8 AS mttr, \
                 count(*) FILTER (WHERE state = 'resolved' AND resolved_by IS NULL) AS auto_resolved, \
                 count(*) FILTER (WHERE state = 'resolved' AND resolved_by IS NOT NULL) AS human_resolved \
             FROM incidents WHERE org_id = $1 AND started_at >= $2",
        )
        .bind(org.0)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("incident metrics scalars: {e}"))?;

        let bucket = |sql: &str| {
            let sql = sql.to_string();
            async move {
                let rows: Vec<(String, i64)> = sqlx::query_as(&sql)
                    .bind(org.0)
                    .bind(since)
                    .fetch_all(&self.pool)
                    .await
                    .map_err(|e| anyhow::anyhow!("incident metrics bucket: {e}"))?;
                Ok::<_, anyhow::Error>(
                    rows.into_iter()
                        .map(|(key, count)| MetricBucket {
                            key,
                            count: count.max(0) as u64,
                        })
                        .collect::<Vec<_>>(),
                )
            }
        };
        let by_severity = bucket(
            "SELECT severity, count(*) FROM incidents \
             WHERE org_id = $1 AND started_at >= $2 GROUP BY severity ORDER BY count DESC",
        )
        .await?;
        let by_state = bucket(
            "SELECT state, count(*) FROM incidents \
             WHERE org_id = $1 AND started_at >= $2 GROUP BY state ORDER BY count DESC",
        )
        .await?;

        let top: Vec<(Uuid, i64)> = sqlx::query_as(
            "SELECT target_id, count(*) FROM incidents \
             WHERE org_id = $1 AND started_at >= $2 AND target_id IS NOT NULL \
             GROUP BY target_id ORDER BY count DESC, target_id LIMIT 10",
        )
        .bind(org.0)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("incident metrics top monitors: {e}"))?;

        Ok(IncidentMetrics {
            window_days,
            total: s.total.max(0) as u64,
            mtta_secs: s.mtta,
            mttr_secs: s.mttr,
            by_severity,
            by_state,
            auto_resolved: s.auto_resolved.max(0) as u64,
            human_resolved: s.human_resolved.max(0) as u64,
            top_monitors: top
                .into_iter()
                .map(|(target_id, count)| MonitorIncidentCount {
                    target_id,
                    count: count.max(0) as u64,
                })
                .collect(),
        })
    }

    async fn declare(
        &self,
        org: OrgId,
        new: NewManualIncident,
        actor: Actor,
    ) -> Result<OpsIncident> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        // status_at_start is a non-null column shared with monitor-opened
        // incidents; a declared incident has no check status, so it records the
        // declared problem as 'down'.
        // A target-bound declare conflicts only with another open declaration;
        // DO NOTHING yields no row → 409, not a 500.
        let sql = format!(
            "INSERT INTO incidents \
                (org_id, target_id, started_at, status_at_start, origin, state, \
                 severity, urgency, title, visibility, paging_enabled, counts_as_downtime) \
             VALUES ($1, $2, now(), 'down', 'manual', 'triggered', $3, $4, $5, 'internal', $6, $7) \
             ON CONFLICT (org_id, target_id) WHERE ended_at IS NULL AND origin = 'manual' \
             DO NOTHING \
             RETURNING {OPS_COLS}"
        );
        let row: Option<OpsIncidentRow> = sqlx::query_as(&sql)
            .bind(org.0)
            .bind(new.target_id)
            .bind(new.severity.as_db_str())
            .bind(new.urgency.as_db_str())
            .bind(new.title)
            .bind(new.notify)
            .bind(new.counts_as_downtime)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("declare incident: {e}"))?;
        let row = row.ok_or_else(|| {
            crate::error::AppError::conflict(
                crate::error::codes::INCIDENT_ALREADY_OPEN,
                "this monitor already has an open incident",
            )
        })?;
        let id = row.id;
        insert_event_tx(&mut tx, org, id, IncidentEventKind::Triggered, actor, None).await?;
        record_incident_audit_tx(
            &mut tx,
            org,
            actor,
            "incident.declared",
            serde_json::json!({
                "incident_id": id,
                "target_id": row.target_id,
                "severity": row.severity,
                "urgency": row.urgency,
            }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(row_to_ops(row))
    }

    async fn acknowledge(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
        expect_generation: Option<i64>,
    ) -> Result<LifecycleOutcome> {
        // COALESCE preserves the first acker + ack time on a re-ack (the state
        // machine treats Acknowledged→Acknowledged as idempotent), so MTTA and
        // on-call attribution reflect who actually took the page first.
        let sql = format!(
            "UPDATE incidents \
             SET state = 'acknowledged', \
                 acknowledged_at = COALESCE(acknowledged_at, now()), \
                 acknowledged_by = COALESCE(acknowledged_by, $3), \
                 next_escalation_at = NULL, updated_at = now() \
             WHERE id = $1 AND org_id = $2 RETURNING {OPS_COLS}"
        );
        self.transition(
            org,
            id,
            IncidentTransition::Acknowledge,
            IncidentEventKind::Acknowledged,
            actor,
            note,
            &sql,
            None,
            expect_generation,
        )
        .await
    }

    async fn generation(&self, org: OrgId, id: Uuid) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT (SELECT count(*) FROM incident_events e \
                     WHERE e.incident_id = i.id AND e.org_id = i.org_id AND e.kind = 'reopened') \
             FROM incidents i WHERE i.id = $1 AND i.org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("incident generation: {e}"))?;
        Ok(row.map(|(n,)| n))
    }

    async fn resolve(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
    ) -> Result<LifecycleOutcome> {
        // Manual resolve: resolved_by = actor's user (bound as $3).
        let sql = format!(
            "UPDATE incidents \
             SET state = 'resolved', ended_at = COALESCE(ended_at, now()), \
                 duration_secs = COALESCE(duration_secs, \
                     GREATEST(0, EXTRACT(EPOCH FROM (now() - started_at))::int)), \
                 resolved_by = $3, next_escalation_at = NULL, updated_at = now() \
             WHERE id = $1 AND org_id = $2 RETURNING {OPS_COLS}"
        );
        let public_resolution = Some(resolved_public_message(note.as_deref()));
        self.transition(
            org,
            id,
            IncidentTransition::Resolve,
            IncidentEventKind::Resolved,
            actor,
            note,
            &sql,
            public_resolution,
            None,
        )
        .await
    }

    async fn auto_resolve(&self, org: OrgId, id: Uuid) -> Result<LifecycleOutcome> {
        // System recovery: resolved_by stays NULL ($3 = system actor's NULL user).
        let sql = format!(
            "UPDATE incidents \
             SET state = 'resolved', ended_at = COALESCE(ended_at, now()), \
                 duration_secs = COALESCE(duration_secs, \
                     GREATEST(0, EXTRACT(EPOCH FROM (now() - started_at))::int)), \
                 resolved_by = $3, next_escalation_at = NULL, updated_at = now() \
             WHERE id = $1 AND org_id = $2 RETURNING {OPS_COLS}"
        );
        self.transition(
            org,
            id,
            IncidentTransition::AutoResolve,
            IncidentEventKind::Resolved,
            Actor::System,
            None,
            &sql,
            Some(AUTO_RESOLVED_MESSAGE.to_string()),
            None,
        )
        .await
    }

    async fn reopen(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        note: Option<String>,
    ) -> Result<LifecycleOutcome> {
        // Reopen does not use the actor's user id in the row, but the shared
        // `transition` helper always binds it as $3 (ack/resolve do use it), so
        // the predicate references $3 as a deliberate no-op to keep the bound
        // parameter count in step with the statement.
        let sql = format!(
            "UPDATE incidents \
             SET state = 'triggered', ended_at = NULL, duration_secs = NULL, resolved_by = NULL, \
                 acknowledged_at = NULL, acknowledged_by = NULL, \
                 escalation_level = 0, escalation_round = 0, renotify_count = 0, \
                 updated_at = now() \
             WHERE id = $1 AND org_id = $2 AND ($3::uuid IS NULL OR true) RETURNING {OPS_COLS}"
        );
        self.transition(
            org,
            id,
            IncidentTransition::Reopen,
            IncidentEventKind::Reopened,
            actor,
            note,
            &sql,
            None,
            None,
        )
        .await
    }

    async fn assign(
        &self,
        org: OrgId,
        id: Uuid,
        assignee: Option<UserId>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        // The assignee must belong to this org — the FK only proves the user
        // exists globally, so without this an org could pin (and the 200-vs-error
        // result would probe for) a user from another tenant.
        if let Some(uid) = assignee {
            let member: Option<Uuid> = sqlx::query_scalar(
                "SELECT user_id FROM memberships WHERE user_id = $1 AND org_id = $2",
            )
            .bind(uid.0)
            .bind(org.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("validate assignee membership: {e}"))?;
            if member.is_none() {
                return Err(crate::error::AppError::unprocessable(
                    crate::error::codes::ASSIGNEE_NOT_MEMBER,
                    "assignee is not a member of this organization",
                ));
            }
        }
        let sql = format!(
            "UPDATE incidents SET assigned_to = $3, updated_at = now() \
             WHERE id = $1 AND org_id = $2 RETURNING {OPS_COLS}"
        );
        let row: Option<OpsIncidentRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(org.0)
            .bind(assignee)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("assign incident: {e}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let kind = if assignee.is_some() {
            IncidentEventKind::Assigned
        } else {
            IncidentEventKind::Unassigned
        };
        insert_event_tx(&mut tx, org, id, kind, actor, None).await?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(Some(row_to_ops(row)))
    }

    async fn publish(
        &self,
        org: OrgId,
        id: Uuid,
        public_title: Option<String>,
        public_description: Option<String>,
        actor: Actor,
    ) -> Result<Option<OpsIncident>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        // Lock the row and read the pre-publish visibility so the opening
        // update below only fires on an internal->public transition, not on a
        // re-publish.
        let prior_visibility: Option<String> = sqlx::query_scalar(
            "SELECT visibility FROM incidents WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("publish lock: {e}"))?;
        let opening_message =
            opening_update_message(public_title.as_deref(), public_description.as_deref());
        // A provided narration field overwrites; an omitted one keeps the stored
        // copy (clearing is the narration patch endpoint's job, not publish's).
        let sql = format!(
            "UPDATE incidents \
             SET visibility = 'public', \
                 public_title = CASE WHEN $3::bool THEN $4 ELSE public_title END, \
                 public_description = CASE WHEN $5::bool THEN $6 ELSE public_description END, \
                 updated_at = now() \
             WHERE id = $1 AND org_id = $2 RETURNING {OPS_COLS}"
        );
        let row: Option<OpsIncidentRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(org.0)
            .bind(public_title.is_some())
            .bind(public_title)
            .bind(public_description.is_some())
            .bind(public_description)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("publish incident: {e}"))?;
        let Some(row) = row else { return Ok(None) };
        insert_event_tx(&mut tx, org, id, IncidentEventKind::Published, actor, None).await?;
        record_incident_audit_tx(
            &mut tx,
            org,
            actor,
            "incident.published",
            serde_json::json!({ "incident_id": id, "target_id": row.target_id }),
        )
        .await?;
        // On the first publish of a still-active incident, post an opening
        // update unless the operator already narrated one, so subscriber
        // fan-out has a row to send. A retro-published, already-resolved
        // incident gets no "investigating" blast.
        if prior_visibility.as_deref() == Some("internal") && row.state != "resolved" {
            let has_update: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM incident_updates WHERE incident_id = $1 AND org_id = $2)",
            )
            .bind(id)
            .bind(org.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("publish update check: {e}"))?;
            if !has_update {
                let author = actor
                    .user_id()
                    .map(|u| u.0.to_string())
                    .unwrap_or_else(|| "system".to_string());
                sqlx::query(
                    "INSERT INTO incident_updates (org_id, incident_id, phase, message, author) \
                     VALUES ($1, $2, 'investigating', $3, $4)",
                )
                .bind(org.0)
                .bind(id)
                .bind(opening_message)
                .bind(author)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("publish opening update: {e}"))?;
            }
        }
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(Some(row_to_ops(row)))
    }

    async fn unpublish(&self, org: OrgId, id: Uuid, actor: Actor) -> Result<Option<OpsIncident>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        let sql = format!(
            "UPDATE incidents SET visibility = 'internal', updated_at = now() \
             WHERE id = $1 AND org_id = $2 RETURNING {OPS_COLS}"
        );
        let row: Option<OpsIncidentRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(org.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("unpublish incident: {e}"))?;
        let Some(row) = row else { return Ok(None) };
        insert_event_tx(
            &mut tx,
            org,
            id,
            IncidentEventKind::Unpublished,
            actor,
            None,
        )
        .await?;
        record_incident_audit_tx(
            &mut tx,
            org,
            actor,
            "incident.unpublished",
            serde_json::json!({ "incident_id": id, "target_id": row.target_id }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(Some(row_to_ops(row)))
    }

    async fn add_note(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        message: String,
    ) -> Result<Option<IncidentEvent>> {
        // SELECT-guarded INSERT: appending to another tenant's incident (or a
        // missing one) yields a clean no-op rather than an orphan event.
        let row: Option<EventRow> = sqlx::query_as(
            r#"INSERT INTO incident_events (org_id, incident_id, kind, actor_type, actor_id, message)
               SELECT i.org_id, $1, 'note', $2, $3, $4
               FROM incidents i WHERE i.id = $1 AND i.org_id = $5
               RETURNING id, incident_id, occurred_at, kind, actor_type, actor_id, detail, message"#,
        )
        .bind(id)
        .bind(actor.actor_type().as_db_str())
        .bind(actor.user_id())
        .bind(message)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("add_note: {e}"))?;
        Ok(row.map(row_to_event))
    }

    async fn timeline(&self, org: OrgId, id: Uuid) -> Result<Vec<IncidentEvent>> {
        let rows: Vec<EventRow> = sqlx::query_as(
            r#"SELECT id, incident_id, occurred_at, kind, actor_type, actor_id, detail, message
               FROM incident_events
               WHERE incident_id = $1 AND org_id = $2
               ORDER BY occurred_at ASC LIMIT $3"#,
        )
        .bind(id)
        .bind(org.0)
        .bind(INCIDENT_DETAIL_ROW_CAP)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("incident timeline: {e}"))?;
        Ok(rows.into_iter().map(row_to_event).collect())
    }

    async fn append_event(
        &self,
        org: OrgId,
        id: Uuid,
        kind: IncidentEventKind,
        actor: Actor,
        message: Option<String>,
    ) -> Result<()> {
        // SELECT-guarded so an event can't be appended to another tenant's (or
        // a missing) incident.
        sqlx::query(
            r#"INSERT INTO incident_events (org_id, incident_id, kind, actor_type, actor_id, message)
               SELECT i.org_id, $1, $2, $3, $4, $5
               FROM incidents i WHERE i.id = $1 AND i.org_id = $6"#,
        )
        .bind(id)
        .bind(kind.as_db_str())
        .bind(actor.actor_type().as_db_str())
        .bind(actor.user_id())
        .bind(message)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("append incident_event: {e}"))?;
        Ok(())
    }

    async fn notifications_for(&self, org: OrgId, id: Uuid) -> Result<Vec<IncidentNotification>> {
        let sql = format!(
            "SELECT {NOTIF_COLS} FROM incident_notifications \
             WHERE incident_id = $1 AND org_id = $2 ORDER BY created_at ASC"
        );
        let rows: Vec<NotifRow> = sqlx::query_as(&sql)
            .bind(id)
            .bind(org.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("notifications_for: {e}"))?;
        Ok(rows.into_iter().map(row_to_notif).collect())
    }

    async fn record_notification(&self, n: NewIncidentNotification) -> Result<Uuid> {
        let row: (Uuid,) = sqlx::query_as(
            r#"INSERT INTO incident_notifications
                 (org_id, incident_id, escalation_level, target_user_id, channel_id,
                  transport, reason, status, attempt, error, sent_at)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
               RETURNING id"#,
        )
        .bind(n.org.0)
        .bind(n.incident_id)
        .bind(n.escalation_level)
        .bind(n.target_user_id)
        .bind(n.channel_id)
        .bind(n.transport)
        .bind(n.reason.as_db_str())
        .bind(n.status.as_db_str())
        .bind(n.attempt)
        .bind(n.error)
        .bind(n.sent_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("record_notification: {e}"))?;
        Ok(row.0)
    }

    async fn pending_notifications(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        max_attempts: i32,
    ) -> Result<Vec<PendingNotification>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            incident_id: Uuid,
            channel_id: Option<Uuid>,
            transport: String,
            reason: String,
            attempt: i32,
        }
        let queued_before = now - chrono::Duration::seconds(QUEUED_TAKEOVER_SECS);
        let rows: Vec<Row> = sqlx::query_as(
            r#"SELECT id, org_id, incident_id, channel_id, transport, reason, attempt
               FROM incident_notifications
               WHERE (status = 'failed' OR (status = 'queued' AND created_at < $4))
                 AND attempt < $1
                 AND (next_attempt_at IS NULL OR next_attempt_at <= $3)
               ORDER BY next_attempt_at ASC NULLS FIRST LIMIT $2"#,
        )
        .bind(max_attempts)
        .bind(cap)
        .bind(now)
        .bind(queued_before)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("pending_notifications: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| PendingNotification {
                id: r.id,
                org: OrgId(r.org_id),
                incident_id: r.incident_id,
                channel_id: r.channel_id,
                transport: r.transport,
                reason: NotificationReason::from_db_str(&r.reason),
                attempt: r.attempt,
            })
            .collect())
    }

    async fn mark_notification(
        &self,
        org: OrgId,
        id: Uuid,
        outcome: NotificationOutcome,
    ) -> Result<()> {
        sqlx::query(
            r#"UPDATE incident_notifications
               SET status = $3, attempt = $4, error = $5, sent_at = $6, next_attempt_at = $7,
                   provider_receipt = COALESCE($8, provider_receipt)
               WHERE id = $1 AND org_id = $2"#,
        )
        .bind(id)
        .bind(org.0)
        .bind(outcome.status.as_db_str())
        .bind(outcome.attempt)
        .bind(outcome.error)
        .bind(outcome.sent_at)
        .bind(outcome.next_attempt_at)
        .bind(outcome.provider_receipt)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("mark_notification: {e}"))?;
        Ok(())
    }

    /// Dated by `sent_at`: the retry sweep reuses the row it was queued on, so
    /// an attempt that failed before a reopen and landed after it belongs to
    /// the new episode.
    async fn due_emergency_acks(&self, limit: usize) -> Result<Vec<EmergencyAck>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            incident_id: Uuid,
            channel_id: Uuid,
            provider_receipt: String,
            generation: i64,
        }
        let rows: Vec<Row> = sqlx::query_as(
            r#"SELECT id, org_id, incident_id, channel_id, provider_receipt,
                      (SELECT count(*) FROM incident_events e
                       WHERE e.incident_id = n.incident_id AND e.org_id = n.org_id
                         AND e.kind = 'reopened'
                         AND e.occurred_at <= COALESCE(n.sent_at, n.created_at)) AS generation
               FROM incident_notifications n
               WHERE provider_receipt IS NOT NULL AND acked_at IS NULL
                 AND status = 'sent' AND channel_id IS NOT NULL
               ORDER BY sent_at ASC LIMIT $1"#,
        )
        .bind(cap)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("due_emergency_acks: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| EmergencyAck {
                id: r.id,
                org: OrgId(r.org_id),
                incident_id: r.incident_id,
                channel_id: r.channel_id,
                receipt: r.provider_receipt,
                generation: r.generation,
            })
            .collect())
    }

    async fn emergency_acks_for_incident(
        &self,
        org: OrgId,
        incident_id: Uuid,
    ) -> Result<Vec<EmergencyAck>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            incident_id: Uuid,
            channel_id: Uuid,
            provider_receipt: String,
            generation: i64,
        }
        let rows: Vec<Row> = sqlx::query_as(
            r#"SELECT id, org_id, incident_id, channel_id, provider_receipt,
                      (SELECT count(*) FROM incident_events e
                       WHERE e.incident_id = n.incident_id AND e.org_id = n.org_id
                         AND e.kind = 'reopened'
                         AND e.occurred_at <= COALESCE(n.sent_at, n.created_at)) AS generation
               FROM incident_notifications n
               WHERE org_id = $1 AND incident_id = $2
                 AND provider_receipt IS NOT NULL AND acked_at IS NULL
                 AND status = 'sent' AND channel_id IS NOT NULL"#,
        )
        .bind(org.0)
        .bind(incident_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("emergency_acks_for_incident: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| EmergencyAck {
                id: r.id,
                org: OrgId(r.org_id),
                incident_id: r.incident_id,
                channel_id: r.channel_id,
                receipt: r.provider_receipt,
                generation: r.generation,
            })
            .collect())
    }

    async fn mark_acked(&self, org: OrgId, id: Uuid, acked_at: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE incident_notifications SET acked_at = $3 WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .bind(acked_at)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("mark_acked: {e}"))?;
        Ok(())
    }

    async fn clear_receipt(&self, org: OrgId, id: Uuid) -> Result<()> {
        sqlx::query(
            "UPDATE incident_notifications SET provider_receipt = NULL WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("clear_receipt: {e}"))?;
        Ok(())
    }

    async fn begin_escalation(
        &self,
        org: OrgId,
        id: Uuid,
        policy_id: Uuid,
        level: i32,
        next_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE incidents \
             SET escalation_policy_id = $3, escalation_level = $4, escalation_round = 0, \
                 next_escalation_at = $5, updated_at = now() \
             WHERE id = $1 AND org_id = $2 AND state = 'triggered'",
        )
        .bind(id)
        .bind(org.0)
        .bind(policy_id)
        .bind(level)
        .bind(next_at)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("begin_escalation: {e}"))?;
        Ok(res.rows_affected() > 0)
    }

    async fn record_escalation(
        &self,
        org: OrgId,
        id: Uuid,
        level: i32,
        round: i32,
        next_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE incidents \
             SET escalation_level = $3, escalation_round = $4, \
                 next_escalation_at = $5, updated_at = now() \
             WHERE id = $1 AND org_id = $2 AND state = 'triggered'",
        )
        .bind(id)
        .bind(org.0)
        .bind(level)
        .bind(round)
        .bind(next_at)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("record_escalation: {e}"))?;
        Ok(res.rows_affected() > 0)
    }

    async fn due_for_escalation(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        lease_secs: i64,
    ) -> Result<Vec<DueIncident>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            target_id: Option<Uuid>,
            escalation_policy_id: Option<Uuid>,
            escalation_level: i32,
            escalation_round: i32,
        }
        // Claim-and-lease: lock the due rows with SKIP LOCKED and push their
        // timer forward so a concurrent engine instance never re-selects them
        // before this one pages + records the real next time.
        let rows: Vec<Row> = sqlx::query_as(
            "UPDATE incidents SET next_escalation_at = $1 + make_interval(secs => $2::double precision) \
             WHERE id IN ( \
                 SELECT id FROM incidents \
                 WHERE state = 'triggered' AND next_escalation_at IS NOT NULL \
                     AND next_escalation_at <= $1 \
                 ORDER BY next_escalation_at ASC LIMIT $3 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING id, org_id, target_id, escalation_policy_id, escalation_level, escalation_round",
        )
        .bind(now)
        .bind(lease_secs.max(1) as f64)
        .bind(cap)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("due_for_escalation: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| DueIncident {
                id: r.id,
                org: OrgId(r.org_id),
                target_id: r.target_id,
                escalation_policy_id: r.escalation_policy_id,
                escalation_level: r.escalation_level,
                escalation_round: r.escalation_round,
            })
            .collect())
    }

    async fn due_for_reconcile(
        &self,
        window: (DateTime<Utc>, DateTime<Utc>),
        limit: usize,
    ) -> Result<Vec<DueIncident>> {
        let (since, cutoff) = window;
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            target_id: Option<Uuid>,
            escalation_policy_id: Option<Uuid>,
            escalation_level: i32,
            escalation_round: i32,
        }
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, org_id, target_id, escalation_policy_id, escalation_level, escalation_round \
             FROM incidents i \
             WHERE state = 'triggered' AND started_at <= $1 AND started_at >= $2 \
                 AND paging_enabled \
                 AND escalation_policy_id IS NULL AND next_escalation_at IS NULL \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM incident_notifications n WHERE n.incident_id = i.id \
                 ) \
             ORDER BY started_at ASC LIMIT $3",
        )
        .bind(cutoff)
        .bind(since)
        .bind(cap)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("due_for_reconcile: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| DueIncident {
                id: r.id,
                org: OrgId(r.org_id),
                target_id: r.target_id,
                escalation_policy_id: r.escalation_policy_id,
                escalation_level: r.escalation_level,
                escalation_round: r.escalation_round,
            })
            .collect())
    }

    async fn due_for_renotify(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<DueIncident>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            target_id: Option<Uuid>,
            escalation_policy_id: Option<Uuid>,
            escalation_level: i32,
            escalation_round: i32,
        }
        // Only rows that reached a channel count as attempts: the damper's
        // bookkeeping rows have no `channel_id`, and counting them would pin
        // this gate open forever on a held incident.
        // Cadence keys off the last page *attempt* (`max(created_at)`), not the
        // last success: gating on `sent_at` would leave a failing channel's
        // incident perpetually overdue and re-page it every tick. NULL (no page
        // yet) fails the `<=`, leaving an unpaged incident to reconcile/retry.
        // Each reminder doubles the next gap, capped at a day but never shorter
        // than the interval the monitor asked for. Each one writes a fresh row,
        // advancing the gate past the widened gap; within-interval delivery
        // retries are the retry sweep's job. The
        // `(org_id, incident_id, created_at)` index serves the max as an
        // index-only scan.
        let sql = format!(
            "SELECT i.id, i.org_id, i.target_id, i.escalation_policy_id, \
                    i.escalation_level, i.escalation_round \
             FROM incidents i \
             JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id \
             WHERE i.state = 'triggered' AND i.ended_at IS NULL \
                 AND i.next_escalation_at IS NULL \
                 AND t.renotify_interval_secs > 0 \
                 AND {not_held} \
                 AND NOT {window} \
                 AND ( \
                     SELECT max(n.created_at) FROM incident_notifications n \
                     WHERE n.incident_id = i.id AND n.org_id = i.org_id \
                       AND n.channel_id IS NOT NULL \
                 ) <= $1 - make_interval(secs => GREATEST( \
                     t.renotify_interval_secs::double precision, \
                     LEAST(t.renotify_interval_secs::double precision \
                               * pow(2, LEAST(i.renotify_count, 20)), \
                           86400))) \
             ORDER BY ( \
                 SELECT max(n.created_at) FROM incident_notifications n \
                 WHERE n.incident_id = i.id AND n.org_id = i.org_id \
                       AND n.channel_id IS NOT NULL \
                 ) ASC \
             LIMIT $2",
            not_held = crate::storage::admin::NOT_HELD_PREDICATE,
            window = crate::storage::suppressing_window_sql("t.id", "t.org_id"),
        );
        let rows: Vec<Row> = sqlx::query_as(&sql)
            .bind(now)
            .bind(cap)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("due_for_renotify: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| DueIncident {
                id: r.id,
                org: OrgId(r.org_id),
                target_id: r.target_id,
                escalation_policy_id: r.escalation_policy_id,
                escalation_level: r.escalation_level,
                escalation_round: r.escalation_round,
            })
            .collect())
    }

    async fn bump_renotify_count(&self, org: OrgId, id: Uuid) -> Result<()> {
        sqlx::query(
            "UPDATE incidents SET renotify_count = renotify_count + 1, updated_at = now() \
             WHERE id = $1 AND org_id = $2 AND state = 'triggered'",
        )
        .bind(id)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("bump_renotify_count: {e}"))?;
        Ok(())
    }

    async fn due_for_maintenance_release(&self, limit: usize) -> Result<Vec<DueIncident>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            target_id: Option<Uuid>,
            escalation_policy_id: Option<Uuid>,
            escalation_level: i32,
            escalation_round: i32,
        }
        // Same "nothing newer than the hold" shape as the flap release: it is
        // what makes this fire once even when the release reaches no channel.
        // The marker EXISTS runs first so a fleet holding nothing pays neither
        // the LATERAL nor the window join.
        let sql = format!(
            "SELECT i.id, i.org_id, i.target_id, i.escalation_policy_id, i.escalation_level, \
                    i.escalation_round \
             FROM incidents i \
             JOIN LATERAL ( \
                 SELECT max(created_at) AS held_at FROM incident_notifications h \
                 WHERE h.incident_id = i.id AND h.org_id = i.org_id \
                   AND h.channel_id IS NULL AND h.transport = 'maintenance' \
             ) held ON TRUE \
             WHERE EXISTS ( \
                   SELECT 1 FROM incident_notifications m \
                   WHERE m.incident_id = i.id AND m.org_id = i.org_id \
                     AND m.channel_id IS NULL AND m.transport = 'maintenance' \
               ) \
               AND i.state = 'triggered' AND i.ended_at IS NULL \
               AND i.target_id IS NOT NULL \
               AND held.held_at IS NOT NULL \
               AND NOT EXISTS ( \
                   SELECT 1 FROM incident_notifications n \
                   WHERE n.incident_id = i.id AND n.org_id = i.org_id \
                     AND n.created_at > held.held_at \
               ) \
               AND NOT {window} \
               AND {not_held} \
             ORDER BY held.held_at ASC LIMIT $1",
            not_held = crate::storage::admin::not_held_sql("i.target_id"),
            window = crate::storage::suppressing_window_sql("i.target_id", "i.org_id"),
        );
        let rows: Vec<Row> = sqlx::query_as(&sql)
            .bind(cap)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("due_for_maintenance_release: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| DueIncident {
                id: r.id,
                org: OrgId(r.org_id),
                target_id: r.target_id,
                escalation_policy_id: r.escalation_policy_id,
                escalation_level: r.escalation_level,
                escalation_round: r.escalation_round,
            })
            .collect())
    }

    async fn due_for_flap_release(
        &self,
        now: DateTime<Utc>,
        hold: chrono::Duration,
        limit: usize,
    ) -> Result<Vec<DueIncident>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            target_id: Option<Uuid>,
            escalation_policy_id: Option<Uuid>,
            escalation_level: i32,
            escalation_round: i32,
        }
        // "Nothing newer than the hold" is what makes this fire once even when
        // the release reaches no channel, and what lets a reopened incident be
        // held and released again on its own merits.
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT i.id, i.org_id, i.target_id, i.escalation_policy_id, i.escalation_level, \
                    i.escalation_round \
             FROM incidents i \
             JOIN LATERAL ( \
                 SELECT max(created_at) AS held_at FROM incident_notifications h \
                 WHERE h.incident_id = i.id AND h.org_id = i.org_id \
                   AND h.channel_id IS NULL AND h.transport = 'damped' \
             ) held ON TRUE \
             WHERE i.state = 'triggered' \
               AND held.held_at IS NOT NULL \
               AND held.held_at <= $1 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM incident_notifications n \
                   WHERE n.incident_id = i.id AND n.org_id = i.org_id \
                     AND n.created_at > held.held_at \
               ) \
             ORDER BY held.held_at ASC LIMIT $2",
        )
        .bind(now - hold)
        .bind(cap)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("due_for_flap_release: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| DueIncident {
                id: r.id,
                org: OrgId(r.org_id),
                target_id: r.target_id,
                escalation_policy_id: r.escalation_policy_id,
                escalation_level: r.escalation_level,
                escalation_round: r.escalation_round,
            })
            .collect())
    }

    async fn flapping_targets(
        &self,
        org: OrgId,
        since: DateTime<Utc>,
        min_opens: u32,
    ) -> Result<std::collections::HashSet<Uuid>> {
        if min_opens == 0 {
            return Ok(Default::default());
        }
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT target_id FROM incidents \
             WHERE org_id = $1 AND started_at >= $2 AND target_id IS NOT NULL \
               AND origin <> 'manual' \
             GROUP BY target_id HAVING count(*) >= $3",
        )
        .bind(org.0)
        .bind(since)
        .bind(i64::from(min_opens))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("flapping_targets: {e}"))?;
        Ok(ids.into_iter().collect())
    }

    async fn opens_since(&self, org: OrgId, target_id: Uuid, since: DateTime<Utc>) -> Result<u32> {
        // A hand-declared incident (maintenance, a customer report) must not
        // push a monitor over the threshold and silence its next real alert.
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM incidents \
             WHERE org_id = $1 AND target_id = $2 AND started_at >= $3 \
               AND origin <> 'manual'",
        )
        .bind(org.0)
        .bind(target_id)
        .bind(since)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("opens_since: {e}"))?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────
