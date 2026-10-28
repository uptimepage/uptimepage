//! Postgres [`IncidentOpsStore`]: the state machine, its activity log, and the
//! console read model. Every statement filters `org_id`, and transitions run
//! under a per-incident advisory lock.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    ActorType, IncidentAcknowledgement, IncidentEvent, IncidentEventKind, IncidentGroupLink,
    IncidentMetrics, IncidentNotification, IncidentOrigin, IncidentSeverity, IncidentState,
    IncidentTransition, IncidentUrgency, IncidentVisibility, MetricBucket, MonitorIncidentCount,
    NewIncidentNotification, NewManualIncident, NotificationOutcome, NotificationReason,
    NotificationStatus, OpsIncident, OrgId, UserId, next_state,
};
use crate::error::Result;
use crate::storage::admin::NOT_HELD_PREDICATE;
use crate::storage::group_incidents::{GroupLock, lock_group_of};
use crate::storage::locks::{advisory_xact_lock, incident_lock_key};

use super::{
    AUTO_RESOLVED_MESSAGE, Acknowledged, Actor, ClaimedEscalation, ClosingNotice,
    DueClosingNotices, DueIncident, EPISODE_EVENT_KINDS, EmergencyAck, FanOut, FanOutKind,
    INCIDENT_DETAIL_ROW_CAP, IncidentOpsFilter, IncidentOpsStore, IncidentStateCounts,
    LifecycleOutcome, OwedPage, PendingNotification, QUEUED_TAKEOVER_SECS, UpdateText,
    may_page_predicate, opening_update_message, pages_with_monitor, status_page_required,
    unnamed_sender,
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
const OPS_COLS: &str = "id, target_id, target_ref, target_name, target_kind, closed_by_monitor_delete, title, state, severity, urgency, origin, visibility, \
     paging_enabled, counts_as_downtime, started_at, ended_at, acknowledged_at, \
     acknowledged_by, assigned_to, resolved_by, escalation_policy_id, escalation_level, \
     escalation_round, next_escalation_at, recovering_since, \
     check_count, error_sample, regions_down, regions_up, created_at, updated_at, \
     group_id, group_name, routed_group_id, group_incident_id, closed_by_group_delete";

/// Point a monitor-less incident at exactly `pages`. A page outside `org`
/// is refused as unknown, so the answer never confirms another tenant's id.
async fn replace_status_pages_tx(
    conn: &mut sqlx::PgConnection,
    org: OrgId,
    id: Uuid,
    pages: &[Uuid],
) -> Result<Vec<Uuid>> {
    let mut wanted = pages.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    let known: i64 =
        sqlx::query_scalar("SELECT count(*) FROM status_pages WHERE org_id = $1 AND id = ANY($2)")
            .bind(org.0)
            .bind(&wanted)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| anyhow::anyhow!("check incident pages: {e}"))?;
    if known != wanted.len() as i64 {
        return Err(super::unknown_status_pages(
            wanted.len() - known.max(0) as usize,
        ));
    }
    // A page kept from before keeps its row, and with it the name a deleted
    // monitor had there.
    sqlx::query(
        "DELETE FROM incident_status_pages \
         WHERE org_id = $1 AND incident_id = $2 AND status_page_id <> ALL($3)",
    )
    .bind(org.0)
    .bind(id)
    .bind(&wanted)
    .execute(&mut *conn)
    .await
    .map_err(|e| anyhow::anyhow!("clear incident pages: {e}"))?;
    sqlx::query(
        "INSERT INTO incident_status_pages (org_id, incident_id, status_page_id) \
         SELECT $1, $2, unnest($3::uuid[]) \
         ON CONFLICT (incident_id, status_page_id) DO NOTHING",
    )
    .bind(org.0)
    .bind(id)
    .bind(&wanted)
    .execute(&mut *conn)
    .await
    .map_err(|e| anyhow::anyhow!("link incident pages: {e}"))?;
    Ok(wanted)
}

/// Names the pages in the audit row only when they are the incident's own;
/// a monitor's incident reaches pages through their components instead.
fn incident_audit_meta(mut meta: serde_json::Value, pages: &[Uuid]) -> serde_json::Value {
    if !pages.is_empty() {
        meta["status_page_ids"] = serde_json::json!(pages);
    }
    meta
}

/// A monitor's incident reaches pages through their components; a page that
/// kept it after the monitor left is history, not a page it was published to.
async fn status_pages_tx(conn: &mut sqlx::PgConnection, org: OrgId, id: Uuid) -> Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT isp.status_page_id FROM incident_status_pages isp \
         JOIN incidents i ON i.id = isp.incident_id AND i.org_id = isp.org_id \
         WHERE isp.org_id = $1 AND isp.incident_id = $2 AND i.target_id IS NULL \
         ORDER BY isp.status_page_id",
    )
    .bind(org.0)
    .bind(id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| anyhow::anyhow!("incident pages: {e}").into())
}

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
pub(super) fn resolved_public_message(note: Option<&str>) -> UpdateText {
    UpdateText::or_default(note, "This incident has been resolved.")
}

#[derive(sqlx::FromRow)]
struct OpsIncidentRow {
    id: Uuid,
    target_id: Option<Uuid>,
    target_ref: Option<Uuid>,
    target_name: Option<String>,
    target_kind: Option<String>,
    closed_by_monitor_delete: bool,
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
    recovering_since: Option<DateTime<Utc>>,
    check_count: i32,
    error_sample: Option<String>,
    regions_down: Option<Vec<String>>,
    regions_up: Option<Vec<String>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    group_id: Option<Uuid>,
    group_name: Option<String>,
    routed_group_id: Option<Uuid>,
    group_incident_id: Option<Uuid>,
    closed_by_group_delete: bool,
}

fn row_to_ops(r: OpsIncidentRow) -> OpsIncident {
    OpsIncident {
        id: r.id,
        target_id: r.target_id,
        target_ref: r.target_ref,
        target_name: r.target_name,
        target_kind: r.target_kind,
        closed_by_monitor_delete: r.closed_by_monitor_delete,
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
        recovering_since: r.recovering_since,
        check_count: r.check_count.max(0) as u64,
        error_sample: r.error_sample,
        regions_down: r.regions_down.unwrap_or_default(),
        regions_up: r.regions_up.unwrap_or_default(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        group: IncidentGroupLink {
            group_id: r.group_id,
            group_name: r.group_name,
            routed_group_id: r.routed_group_id,
            group_incident_id: r.group_incident_id,
            closed_by_group_delete: r.closed_by_group_delete,
        },
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
     transport, reason, status, attempt, error, episode, created_at, sent_at, next_attempt_at, \
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
    episode: i64,
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
        episode: r.episode,
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

/// Arm the ladder an opening found, only while the incident is triggered: an
/// ack or a resolve that landed first cleared the timer, and stays.
async fn arm_escalation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
    arm: Option<super::Arming>,
) -> Result<()> {
    let Some(arm) = arm else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE incidents \
         SET escalation_policy_id = $3, escalation_level = $4, escalation_round = 0, \
             next_escalation_at = $5, updated_at = now() \
         WHERE id = $1 AND org_id = $2 AND state = 'triggered'",
    )
    .bind(id)
    .bind(org.0)
    .bind(arm.policy_id)
    .bind(arm.level)
    .bind(arm.next_at)
    .execute(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("arm escalation: {e}"))?;
    Ok(())
}

/// A group incident is watched while one of its members still down is: on,
/// held by no plan and in no window that holds alerts. With all of them paused
/// or held, its pages wait as a paused monitor's do. For the incidents row the
/// query names `i`.
fn group_watched_sql(i: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM incidents m \
                 JOIN targets mt ON mt.id = m.target_id AND mt.org_id = m.org_id \
                 WHERE m.group_incident_id = {i}.id AND m.org_id = {i}.org_id \
                   AND m.ended_at IS NULL AND mt.enabled AND mt.plan_hold_at IS NULL \
                   AND NOT {window})",
        window = crate::storage::suppressing_window_sql("mt.id", "mt.org_id"),
    )
}

/// A digest is due on group incident `i` of group `g`: it is open and alerts
/// again, members joined past the last word, and that word went out a window
/// ago.
fn digest_due_sql(i: &str, g: &str) -> String {
    format!(
        "{i}.ended_at IS NULL AND {g}.alert_again AND {i}.member_seq > {i}.member_notice_seq \
         AND {i}.last_member_notice_at <= now() - make_interval(secs => {window})",
        window = super::MEMBER_NOTICE_WINDOW_SECS,
    )
}

/// Resolving a group incident resolves its members still open, as the same
/// actor and owing no notice of their own: the group's covers them. A member
/// still failing opens a fresh incident on the writer's next tick.
async fn resolve_members_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    group_incident: Uuid,
    actor: Actor,
) -> Result<()> {
    let members: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM incidents \
         WHERE org_id = $1 AND group_incident_id = $2 AND ended_at IS NULL ORDER BY id",
    )
    .bind(org.0)
    .bind(group_incident)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("group members to resolve: {e}"))?;
    for member in &members {
        advisory_xact_lock(&mut **tx, &incident_lock_key(*member))
            .await
            .map_err(|e| anyhow::anyhow!("member lock: {e}"))?;
    }
    let resolved: Vec<(Uuid, String)> = sqlx::query_as(
        "UPDATE incidents \
         SET state = 'resolved', ended_at = COALESCE(ended_at, recovering_since, now()), \
             duration_secs = COALESCE(duration_secs, GREATEST(0, EXTRACT(EPOCH FROM \
                 (COALESCE(ended_at, recovering_since, now()) - started_at))::int)), \
             resolved_by = $3, next_escalation_at = NULL, recovering_since = NULL, \
             resolved_at = now(), updated_at = now() \
         WHERE id = ANY($1) AND org_id = $2 AND ended_at IS NULL \
         RETURNING id, visibility",
    )
    .bind(&members)
    .bind(org.0)
    .bind(actor.user_id())
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("resolve group members: {e}"))?;
    let resolution = resolved_public_message(None);
    for (member, visibility) in resolved {
        insert_event_tx(tx, org, member, IncidentEventKind::Resolved, actor, None).await?;
        if visibility == "public" {
            sqlx::query(
                "INSERT INTO incident_updates (org_id, incident_id, phase, message, author, generated) \
                 VALUES ($1, $2, 'resolved', $3, 'system', true)",
            )
            .bind(org.0)
            .bind(member)
            .bind(&resolution.message)
            .execute(&mut **tx)
            .await
            .map_err(|e| anyhow::anyhow!("member public resolution: {e}"))?;
        }
    }
    Ok(())
}

/// The members a due digest covers, numbered `from..=to` and still down.
struct Digest {
    from: i32,
    to: i32,
    down: Vec<Uuid>,
}

/// Takes the digest due on `group_incident`, the caller holding the group's
/// lock, and moves its watermark past the members it covers. `None` when none
/// is due; a range whose members are all back up is passed over with no rows,
/// and the minute keeps counting from the last word that went out.
async fn take_digest(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    group_incident: Uuid,
) -> Result<Option<Digest>> {
    let range: Option<(i32, i32)> = sqlx::query_as(&format!(
        "SELECT i.member_notice_seq + 1, i.member_seq FROM incidents i \
         JOIN monitor_groups g ON g.id = i.group_id AND g.org_id = i.org_id \
         WHERE i.id = $1 AND i.org_id = $2 AND {}",
        digest_due_sql("i", "g")
    ))
    .bind(group_incident)
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("digest due: {e}"))?;
    let Some((from, to)) = range else {
        return Ok(None);
    };
    let down: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM incidents \
         WHERE org_id = $1 AND group_incident_id = $2 AND ended_at IS NULL \
           AND group_member_seq BETWEEN $3 AND $4 \
         ORDER BY group_member_seq",
    )
    .bind(org.0)
    .bind(group_incident)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("digest members down: {e}"))?;
    sqlx::query(
        "UPDATE incidents SET member_notice_seq = $3, \
             last_member_notice_at = CASE WHEN $4 THEN now() ELSE last_member_notice_at END, \
             updated_at = now() \
         WHERE id = $1 AND org_id = $2",
    )
    .bind(group_incident)
    .bind(org.0)
    .bind(to)
    .bind(!down.is_empty())
    .execute(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("advance digest: {e}"))?;
    Ok(Some(Digest { from, to, down }))
}

/// Stores on a group page's rows the members still down that it names. The
/// first page to reach anyone also covers every member numbered so far, and
/// marks them told. The caller holds the group's lock, so no attach or close
/// lands between this read and the rows.
async fn cover_members(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    group_incident: Uuid,
    rows: &[Uuid],
    first_word: bool,
) -> Result<()> {
    let first: Option<i32> = if first_word {
        sqlx::query_scalar(
            "UPDATE incidents SET member_notice_seq = member_seq, \
                 last_member_notice_at = now(), updated_at = now() \
             WHERE id = $1 AND org_id = $2 AND last_member_notice_at IS NULL \
             RETURNING member_seq",
        )
        .bind(group_incident)
        .bind(org.0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| anyhow::anyhow!("mark members told: {e}"))?
    } else {
        None
    };
    sqlx::query(
        "UPDATE incident_notifications SET \
             member_from_seq = CASE WHEN $4::int IS NOT NULL THEN 1 END, \
             member_to_seq = $4, \
             member_incident_ids = ARRAY( \
                 SELECT id FROM incidents \
                 WHERE org_id = $2 AND group_incident_id = $3 AND ended_at IS NULL \
                 ORDER BY group_member_seq) \
         WHERE id = ANY($1) AND org_id = $2",
    )
    .bind(rows)
    .bind(org.0)
    .bind(group_incident)
    .bind(first)
    .execute(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("cover members: {e}"))?;
    Ok(())
}

async fn insert_notification<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    n: NewIncidentNotification,
) -> Result<Uuid> {
    let row: (Uuid,) = sqlx::query_as(
        r#"INSERT INTO incident_notifications
             (org_id, incident_id, escalation_level, target_user_id, channel_id,
              transport, reason, status, attempt, error, sent_at, episode)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
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
    .bind(n.episode)
    .fetch_one(executor)
    .await
    .map_err(|e| anyhow::anyhow!("record_notification: {e}"))?;
    Ok(row.0)
}

/// Mirror an operator's incident action into `org_audit_log`, the org's one
/// trail of who did what. System transitions are skipped, or every recovery
/// writes a row.
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
/// incident itself going with its org, which voids every link to it anyway.
async fn reopen_count_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<i64> {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM incident_events \
         WHERE incident_id = $1 AND org_id = $2 AND kind IN {EPISODE_EVENT_KINDS}"
    ))
    .bind(id)
    .bind(org.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("reopen count: {e}").into())
}

/// Add `actor` to the people who acknowledged episode `episode`; false when
/// they already are. The unique indexes decide, so two presses racing each
/// other still land once.
async fn record_acknowledgement_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
    actor: Actor,
    episode: i64,
) -> Result<bool> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO incident_acknowledgements \
             (org_id, incident_id, episode, actor_type, actor_id, anonymous, sender) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(org.0)
    .bind(id)
    .bind(episode)
    .bind(actor.actor_type().as_db_str())
    .bind(actor.user_id())
    .bind(actor.user_id().is_none())
    .bind(unnamed_sender(actor).map(|s| s.hex()))
    .fetch_optional(&mut **tx)
    .await
    .map_err(|e| anyhow::anyhow!("record acknowledgement: {e}"))?;
    Ok(row.is_some())
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
        public_resolution: Option<UpdateText>,
        expect_generation: Option<i64>,
    ) -> Result<Acknowledged> {
        let unlisted = |outcome| {
            Ok(Acknowledged {
                outcome,
                listed: false,
            })
        };
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        // Resolving a group incident resolves its members.
        lock_group_of(&mut tx, org, id).await?;
        advisory_xact_lock(&mut *tx, &incident_lock_key(id))
            .await
            .map_err(|e| anyhow::anyhow!("incident lock: {e}"))?;

        let current: Option<(String, bool, Option<Uuid>, bool)> = sqlx::query_as(
            "SELECT state, origin = 'monitor' AND target_id IS NULL AND target_name IS NOT NULL, \
                    group_incident_id, origin = 'group' \
             FROM incidents WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("load incident state: {e}"))?;
        let Some((state_str, ended_with_monitor, attached, is_group)) = current else {
            return unlisted(LifecycleOutcome::NotFound);
        };
        if let Some(group_incident) = attached {
            return Err(super::use_group_incident(group_incident));
        }
        if is_group && transition == IncidentTransition::Reopen {
            return Err(super::group_incident_no_reopen());
        }
        // Read under the row lock the monitor delete also takes, so a reopen
        // cannot land on an incident the delete has just closed.
        if ended_with_monitor && transition == IncidentTransition::Reopen {
            return Err(super::monitor_deleted());
        }
        // Under the transition's own locks, so a reopen cannot slip between
        // this check and the update.
        let episode = reopen_count_tx(&mut tx, org, id).await?;
        if expect_generation.is_some_and(|expected| expected != episode) {
            return unlisted(LifecycleOutcome::Stale);
        }

        let from = IncidentState::from_db_str(&state_str);
        let to = match next_state(from, transition) {
            Ok(to) => to,
            Err(err) => return unlisted(LifecycleOutcome::IllegalTransition(err)),
        };
        // A transition that moves nothing leaves the row alone, so the first
        // acknowledger keeps the credit an unknown one included, which the
        // update's `COALESCE` would otherwise backfill. It is still logged: an
        // engineer taking a page a notification already acknowledged is a real
        // action. Someone acknowledging an episode they already took is not,
        // and neither is a repeated public update — subscribers should not
        // read the same resolution twice.
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

        let acknowledging = transition == IncidentTransition::Acknowledge;
        let newly_listed = if acknowledging {
            record_acknowledgement_tx(&mut tx, org, id, actor, episode).await?
        } else {
            false
        };
        let repeat = acknowledging && unchanged && !newly_listed;
        if repeat {
            // A note someone wrote is kept; a channel's is a fixed label.
            if let Some(note) = note.as_deref().filter(|_| actor.writes_notes()) {
                insert_event_tx(&mut tx, org, id, IncidentEventKind::Note, actor, Some(note))
                    .await?;
            }
        } else {
            insert_event_tx(&mut tx, org, id, event_kind, actor, note.as_deref()).await?;
            if is_group && to == IncidentState::Resolved && !unchanged {
                resolve_members_tx(&mut tx, org, id, actor).await?;
            }
            record_incident_audit_tx(
                &mut tx,
                org,
                actor,
                &format!("incident.{}", event_kind.as_db_str()),
                serde_json::json!({ "incident_id": id, "target_id": row.target_id }),
            )
            .await?;
        }
        // An incident unpublished before it resolves still has subscribers who
        // were told it opened; write the closing update whenever it was ever
        // public, not only while currently public.
        if let Some(resolution) = public_resolution
            && !unchanged
            && (row.visibility == "public" || incident_was_published(&mut tx, org, id).await?)
        {
            let author = actor
                .user_id()
                .map(|u| u.0.to_string())
                .unwrap_or_else(|| "system".to_string());
            sqlx::query(
                r#"INSERT INTO incident_updates
                       (org_id, incident_id, phase, message, author, generated)
                   VALUES ($1, $2, 'resolved', $3, $4, $5)"#,
            )
            .bind(org.0)
            .bind(id)
            .bind(resolution.message)
            .bind(author)
            .bind(resolution.generated)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("resolve public update: {e}"))?;
        }
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(Acknowledged {
            outcome: LifecycleOutcome::Updated(Box::new(row_to_ops(row))),
            listed: newly_listed,
        })
    }

    async fn resolve_by(
        &self,
        org: OrgId,
        id: Uuid,
        transition: IncidentTransition,
        actor: Actor,
        note: Option<String>,
        expect_generation: Option<i64>,
    ) -> Result<LifecycleOutcome> {
        // Manual resolve: resolved_by = actor's user (bound as $3). One waiting
        // out its recovery ended where the recovery began.
        let sql = format!(
            "UPDATE incidents \
             SET state = 'resolved', ended_at = COALESCE(ended_at, recovering_since, now()), \
                 duration_secs = COALESCE(duration_secs, GREATEST(0, EXTRACT(EPOCH FROM \
                     (COALESCE(ended_at, recovering_since, now()) - started_at))::int)), \
                 resolved_by = $3, next_escalation_at = NULL, recovering_since = NULL, \
                 closing_notice_at = now(), resolved_at = now(), updated_at = now() \
             WHERE id = $1 AND org_id = $2 RETURNING {OPS_COLS}"
        );
        let public_resolution = Some(resolved_public_message(note.as_deref()));
        self.transition(
            org,
            id,
            transition,
            IncidentEventKind::Resolved,
            actor,
            note,
            &sql,
            public_resolution,
            expect_generation,
        )
        .await
        .map(|a| a.outcome)
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
                 AND ($7::text IS NULL OR title ILIKE $7 OR target_name ILIKE $7) \
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
                 AND ($5::text IS NULL OR title ILIKE $5 OR target_name ILIKE $5)",
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
                 AND ($4::text IS NULL OR title ILIKE $4 OR target_name ILIKE $4) \
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
            closed_with_monitor: i64,
        }
        // A group incident counts once for its members, which are left out. A
        // member a group delete detached counts, but its time to that point
        // was the group's outage.
        let s: Scalars = sqlx::query_as(
            "SELECT count(*) AS total, \
                 (avg(extract(epoch FROM (acknowledged_at - started_at))) \
                     FILTER (WHERE acknowledged_at IS NOT NULL AND detached_at IS NULL))::float8 AS mtta, \
                 (avg(extract(epoch FROM (ended_at - started_at))) \
                     FILTER (WHERE ended_at IS NOT NULL AND NOT closed_by_monitor_delete \
                         AND NOT closed_by_group_delete AND detached_at IS NULL))::float8 AS mttr, \
                 count(*) FILTER (WHERE state = 'resolved' AND resolved_by IS NULL \
                     AND NOT closed_by_monitor_delete AND NOT closed_by_group_delete) AS auto_resolved, \
                 count(*) FILTER (WHERE state = 'resolved' AND resolved_by IS NOT NULL \
                     AND NOT closed_by_monitor_delete AND NOT closed_by_group_delete) AS human_resolved, \
                 count(*) FILTER (WHERE closed_by_monitor_delete OR closed_by_group_delete) \
                     AS closed_with_monitor \
             FROM incidents \
             WHERE org_id = $1 AND started_at >= $2 AND group_incident_id IS NULL",
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
             WHERE org_id = $1 AND started_at >= $2 AND group_incident_id IS NULL \
             GROUP BY severity ORDER BY count DESC",
        )
        .await?;
        let by_state = bucket(
            "SELECT state, count(*) FROM incidents \
             WHERE org_id = $1 AND started_at >= $2 AND group_incident_id IS NULL \
             GROUP BY state ORDER BY count DESC",
        )
        .await?;

        // Grouped by the monitor's kept id, so two deleted monitors that shared
        // a name stay two rows.
        let top: Vec<(Option<Uuid>, String, i64)> = sqlx::query_as(
            "SELECT target_id, target_name, count(*) FROM incidents \
             WHERE org_id = $1 AND started_at >= $2 AND target_ref IS NOT NULL \
             GROUP BY target_ref, target_id, target_name \
             ORDER BY count DESC, target_name, target_ref LIMIT 10",
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
            closed_with_monitor: s.closed_with_monitor.max(0) as u64,
            top_monitors: top
                .into_iter()
                .map(|(target_id, name, count)| MonitorIncidentCount {
                    target_id,
                    name,
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
        if new.target_id.is_some() && !new.status_page_ids.is_empty() {
            return Err(pages_with_monitor());
        }
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
                 severity, urgency, title, visibility, paging_enabled, counts_as_downtime, \
                 page_owed_at) \
             VALUES ($1, $2, now(), 'down', 'manual', 'triggered', $3, $4, $5, 'internal', $6, $7, \
                     CASE WHEN $6 AND $2::uuid IS NOT NULL THEN now() END) \
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
        let pages = replace_status_pages_tx(&mut tx, org, id, &new.status_page_ids).await?;
        insert_event_tx(&mut tx, org, id, IncidentEventKind::Triggered, actor, None).await?;
        record_incident_audit_tx(
            &mut tx,
            org,
            actor,
            "incident.declared",
            incident_audit_meta(
                serde_json::json!({
                    "incident_id": id,
                    "target_id": row.target_id,
                    "severity": row.severity,
                    "urgency": row.urgency,
                }),
                &pages,
            ),
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
    ) -> Result<Acknowledged> {
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
        let row: Option<(i64,)> = sqlx::query_as(&format!(
            "SELECT (SELECT count(*) FROM incident_events e \
                     WHERE e.incident_id = i.id AND e.org_id = i.org_id \
                       AND e.kind IN {EPISODE_EVENT_KINDS}) \
             FROM incidents i WHERE i.id = $1 AND i.org_id = $2"
        ))
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
        self.resolve_by(org, id, IncidentTransition::Resolve, actor, note, None)
            .await
    }

    async fn resolve_episode(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Actor,
        episode: i64,
    ) -> Result<LifecycleOutcome> {
        self.resolve_by(
            org,
            id,
            IncidentTransition::ResolveOpen,
            actor,
            None,
            Some(episode),
        )
        .await
    }

    async fn auto_resolve(&self, org: OrgId, id: Uuid) -> Result<LifecycleOutcome> {
        // System recovery: resolved_by stays NULL ($3 = system actor's NULL user).
        let sql = format!(
            "UPDATE incidents \
             SET state = 'resolved', ended_at = COALESCE(ended_at, recovering_since, now()), \
                 duration_secs = COALESCE(duration_secs, GREATEST(0, EXTRACT(EPOCH FROM \
                     (COALESCE(ended_at, recovering_since, now()) - started_at))::int)), \
                 resolved_by = $3, next_escalation_at = NULL, recovering_since = NULL, \
                 closing_notice_at = now(), resolved_at = now(), updated_at = now() \
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
            Some(UpdateText {
                message: AUTO_RESOLVED_MESSAGE.to_string(),
                generated: true,
            }),
            None,
        )
        .await
        .map(|a| a.outcome)
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
        // parameter count in step with the statement. An incident running again
        // no longer owes the notice that it ended, and owes its first page
        // anew if it may page. One reopened on a monitor
        // paused since it closed is paused from that close: the checks had
        // cleared it, then nothing checked it.
        let sql = format!(
            "UPDATE incidents \
             SET state = 'triggered', ended_at = NULL, duration_secs = NULL, resolved_by = NULL, \
                 acknowledged_at = NULL, acknowledged_by = NULL, \
                 escalation_level = 0, escalation_round = 0, renotify_count = 0, \
                 closing_notice_at = NULL, resolved_at = NULL, recovering_since = NULL, \
                 page_owed_at = CASE WHEN {may_page} THEN now() END, \
                 paused_since = CASE WHEN paused_since IS NULL AND origin = 'monitor' AND EXISTS ( \
                     SELECT 1 FROM targets t \
                     WHERE t.id = incidents.target_id AND t.org_id = incidents.org_id AND NOT t.enabled \
                 ) THEN ended_at ELSE paused_since END, \
                 updated_at = now() \
             WHERE id = $1 AND org_id = $2 AND ($3::uuid IS NULL OR true) RETURNING {OPS_COLS}",
            may_page = may_page_predicate!("incidents"),
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
        .map(|a| a.outcome)
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
        status_page_ids: Option<Vec<Uuid>>,
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
        let prior: Option<(String, Option<Uuid>, bool, bool)> = sqlx::query_as(
            "SELECT visibility, target_id, \
                    origin = 'monitor' AND target_id IS NULL AND target_name IS NOT NULL, \
                    origin = 'group' \
             FROM incidents WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("publish lock: {e}"))?;
        let Some((prior_visibility, target_id, ended_with_monitor, is_group)) = prior else {
            return Ok(None);
        };
        if is_group {
            return Err(super::group_incident_internal());
        }
        let pages = if target_id.is_none() {
            // No other page ever carried the deleted monitor, and none would
            // know what to call it.
            if ended_with_monitor && let Some(wanted) = &status_page_ids {
                let shown = status_pages_tx(&mut tx, org, id).await?;
                if wanted.iter().any(|p| !shown.contains(p)) {
                    return Err(super::pages_beyond_deleted_monitor());
                }
            }
            let pages = match &status_page_ids {
                Some(pages) => replace_status_pages_tx(&mut tx, org, id, pages).await?,
                None => status_pages_tx(&mut tx, org, id).await?,
            };
            if pages.is_empty() {
                return Err(if ended_with_monitor {
                    super::pages_beyond_deleted_monitor()
                } else {
                    status_page_required()
                });
            }
            pages
        } else if status_page_ids.is_some_and(|p| !p.is_empty()) {
            return Err(pages_with_monitor());
        } else {
            Vec::new()
        };
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
            incident_audit_meta(
                serde_json::json!({ "incident_id": id, "target_id": row.target_id }),
                &pages,
            ),
        )
        .await?;
        // On the first publish of a still-active incident, post an opening
        // update unless the operator already narrated one, so subscriber
        // fan-out has a row to send. A retro-published, already-resolved
        // incident gets no "investigating" blast.
        if prior_visibility == "internal" && row.state != "resolved" {
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
                    "INSERT INTO incident_updates \
                         (org_id, incident_id, phase, message, author, generated) \
                     VALUES ($1, $2, 'investigating', $3, $4, $5)",
                )
                .bind(org.0)
                .bind(id)
                .bind(opening_message.message)
                .bind(author)
                .bind(opening_message.generated)
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

    async fn status_pages(&self, org: OrgId, id: Uuid) -> Result<Vec<Uuid>> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("acquire: {e}"))?;
        status_pages_tx(&mut conn, org, id).await
    }

    async fn kept_on_pages(&self, org: OrgId, id: Uuid) -> Result<Vec<Uuid>> {
        sqlx::query_scalar(
            "SELECT status_page_id FROM incident_status_pages \
             WHERE org_id = $1 AND incident_id = $2 ORDER BY status_page_id",
        )
        .bind(org.0)
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("kept pages: {e}").into())
    }

    async fn closed_with_monitors(&self, org: OrgId, monitors: &[Uuid]) -> Result<Vec<Uuid>> {
        // A group incident whose last member down left with its monitor
        // closed in the same delete.
        sqlx::query_scalar(
            "SELECT id FROM incidents i \
             WHERE org_id = $1 AND closed_by_monitor_delete AND ( \
                 (target_ref = ANY($2) AND target_id IS NULL) \
                 OR (origin = 'group' AND EXISTS ( \
                     SELECT 1 FROM incidents m \
                     WHERE m.org_id = i.org_id AND m.group_incident_id = i.id \
                       AND m.target_ref = ANY($2))))",
        )
        .bind(org.0)
        .bind(monitors)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("incidents closed with monitors: {e}").into())
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

    async fn acknowledgements(
        &self,
        org: OrgId,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<IncidentAcknowledgement>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Rows are stamped at insert, under the incident lock, so their order
        // is the order the acknowledgements landed and the credited one leads.
        let rows: Vec<(Uuid, String, Option<Uuid>, bool, DateTime<Utc>)> =
            sqlx::query_as(&format!(
                "WITH current AS ( \
                 SELECT i.id, (SELECT count(*) FROM incident_events r \
                               WHERE r.incident_id = i.id AND r.org_id = i.org_id \
                                 AND r.kind IN {EPISODE_EVENT_KINDS}) AS episode \
                 FROM incidents i WHERE i.org_id = $1 AND i.id = ANY($2)) \
             SELECT a.incident_id, a.actor_type, a.actor_id, a.anonymous, a.acknowledged_at \
             FROM incident_acknowledgements a \
             JOIN current c ON c.id = a.incident_id AND c.episode = a.episode \
             WHERE a.org_id = $1 \
             ORDER BY a.acknowledged_at, a.id"
            ))
            .bind(org.0)
            .bind(ids)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("list acknowledgements: {e}"))?;
        let mut out: HashMap<Uuid, Vec<IncidentAcknowledgement>> = HashMap::new();
        for (incident_id, actor_type, actor_id, anonymous, at) in rows {
            out.entry(incident_id)
                .or_default()
                .push(IncidentAcknowledgement {
                    incident_id,
                    actor_type: ActorType::from_db_str(&actor_type),
                    actor_id: actor_id.map(UserId),
                    anonymous,
                    at,
                });
        }
        Ok(out)
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
        insert_notification(&self.pool, n).await
    }

    async fn record_fan_out(&self, f: FanOut) -> Result<Option<Vec<Uuid>>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        // A group page names its members under the group's lock.
        let group = match lock_group_of(&mut tx, f.org, f.incident_id).await? {
            GroupLock::Gone => return Ok(None),
            GroupLock::Held => Some(()),
            GroupLock::NotGroup => None,
        };
        // The lifecycle transitions' lock and order, so a reopen and a page
        // serialise and each reads what the other committed.
        advisory_xact_lock(&mut *tx, &incident_lock_key(f.incident_id))
            .await
            .map_err(|e| anyhow::anyhow!("incident lock: {e}"))?;
        let pageable: Option<(bool,)> = sqlx::query_as(concat!(
            "SELECT page_owed_at IS NOT NULL FROM incidents \
             WHERE id = $1 AND org_id = $2 AND ",
            may_page_predicate!("incidents"),
            " AND state <> 'resolved' FOR UPDATE"
        ))
        .bind(f.incident_id)
        .bind(f.org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("lock incident for fan-out: {e}"))?;
        let Some((owed,)) = pageable else {
            return Ok(None);
        };
        if reopen_count_tx(&mut tx, f.org, f.incident_id).await? != f.episode {
            return Ok(None);
        }
        let mut digest = None;
        let settled =
            match f.kind {
                FanOutKind::Opening(arm) => {
                    if !owed {
                        return Ok(None);
                    }
                    sqlx::query(
                        "UPDATE incidents SET page_owed_at = NULL, updated_at = now() \
                     WHERE id = $1 AND org_id = $2",
                    )
                    .bind(f.incident_id)
                    .bind(f.org.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| anyhow::anyhow!("settle owed page: {e}"))?;
                    // A row of the episode already there was recorded by a build
                    // that does not settle what is owed, as one still running
                    // beside this one during a deploy: the page went out, so
                    // settle it and record nothing.
                    let recorded: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM incident_notifications \
                     WHERE incident_id = $1 AND org_id = $2 AND episode = $3)",
                    )
                    .bind(f.incident_id)
                    .bind(f.org.0)
                    .bind(f.episode)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| anyhow::anyhow!("episode already paged: {e}"))?;
                    if recorded {
                        tx.commit()
                            .await
                            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
                        return Ok(None);
                    }
                    arm_escalation_tx(&mut tx, f.org, f.incident_id, arm).await?;
                    true
                }
                // Only an opening makes a hold, and it settles what is owed, so a
                // hold met by an owed page is a past episode's: the page owed now
                // is the one to send.
                FanOutKind::Release(arm) => {
                    if owed {
                        return Ok(None);
                    }
                    arm_escalation_tx(&mut tx, f.org, f.incident_id, arm).await?;
                    true
                }
                FanOutKind::Rung {
                    claim,
                    level,
                    round,
                    next_at,
                } => {
                    sqlx::query(
                        "UPDATE incidents \
                 SET escalation_level = $3, escalation_round = $4, \
                     next_escalation_at = $5, updated_at = now() \
                 WHERE id = $1 AND org_id = $2 AND state = 'triggered' \
                     AND recovering_since IS NULL AND next_escalation_at = $6",
                    )
                    .bind(f.incident_id)
                    .bind(f.org.0)
                    .bind(level)
                    .bind(round)
                    .bind(next_at)
                    .bind(claim)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| anyhow::anyhow!("advance escalation: {e}"))?
                    .rows_affected()
                        > 0
                }
                FanOutKind::MemberDigest => {
                    if group.is_none() {
                        return Ok(None);
                    }
                    let Some(range) = take_digest(&mut tx, f.org, f.incident_id).await? else {
                        return Ok(None);
                    };
                    digest = Some(range);
                    true
                }
                FanOutKind::Reminder => sqlx::query(
                    "UPDATE incidents SET renotify_count = renotify_count + 1, updated_at = now() \
                 WHERE id = $1 AND org_id = $2 AND state = 'triggered' \
                     AND recovering_since IS NULL AND page_owed_at IS NULL",
                )
                .bind(f.incident_id)
                .bind(f.org.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("count reminder: {e}"))?
                .rows_affected()
                    > 0,
            };
        if !settled {
            return Ok(None);
        }
        if digest.as_ref().is_some_and(|d| d.down.is_empty()) {
            tx.commit()
                .await
                .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
            return Ok(Some(Vec::new()));
        }
        let reaches = f.rows.iter().any(|n| n.channel_id.is_some());
        let first_word = reaches
            && matches!(
                f.kind,
                FanOutKind::Opening(_) | FanOutKind::Release(_) | FanOutKind::Rung { .. }
            );
        let mut ids = Vec::with_capacity(f.rows.len());
        for n in f.rows {
            ids.push(insert_notification(&mut *tx, n).await?);
        }
        match digest {
            Some(Digest { from, to, down }) => {
                sqlx::query(
                    "UPDATE incident_notifications \
                     SET member_from_seq = $3, member_to_seq = $4, member_incident_ids = $5 \
                     WHERE id = ANY($1) AND org_id = $2",
                )
                .bind(&ids)
                .bind(f.org.0)
                .bind(from)
                .bind(to)
                .bind(&down)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("digest members: {e}"))?;
            }
            None if group.is_some() => {
                cover_members(&mut tx, f.org, f.incident_id, &ids, first_word).await?;
            }
            None => {}
        }
        for (kind, message) in &f.events {
            insert_event_tx(
                &mut tx,
                f.org,
                f.incident_id,
                *kind,
                Actor::System,
                Some(message),
            )
            .await?;
        }
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(Some(ids))
    }

    async fn notice_members(&self, org: OrgId, notification_id: Uuid) -> Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT i.target_name FROM incident_notifications n \
             CROSS JOIN LATERAL unnest(n.member_incident_ids) WITH ORDINALITY AS m(id, ord) \
             JOIN incidents i ON i.id = m.id AND i.org_id = n.org_id \
             WHERE n.id = $1 AND n.org_id = $2 AND i.target_name IS NOT NULL \
             ORDER BY m.ord",
        )
        .bind(notification_id)
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("notice members: {e}").into())
    }

    async fn attach_member(&self, org: OrgId, member: Uuid) -> Result<super::Attach> {
        crate::storage::group_incidents::attach_member(&self.pool, org, member).await
    }

    async fn member_digest_due(&self, org: OrgId, group_incident: Uuid) -> Result<bool> {
        sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM incidents i \
             JOIN monitor_groups g ON g.id = i.group_id AND g.org_id = i.org_id \
             WHERE i.id = $1 AND i.org_id = $2 AND {})",
            digest_due_sql("i", "g")
        ))
        .bind(group_incident)
        .bind(org.0)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("member digest due: {e}").into())
    }

    async fn group_watched(&self, org: OrgId, group_incident: Uuid) -> Result<bool> {
        sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM incidents i \
             WHERE i.id = $1 AND i.org_id = $2 AND {})",
            group_watched_sql("i")
        ))
        .bind(group_incident)
        .bind(org.0)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("group watched: {e}").into())
    }

    async fn due_member_digests(&self, limit: usize) -> Result<Vec<super::DueDigest>> {
        let cap = (limit as i64).clamp(1, 1000);
        // Unwatched ones wait without holding the head of the queue.
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(&format!(
            "SELECT i.id, i.org_id FROM incidents i \
             JOIN monitor_groups g ON g.id = i.group_id AND g.org_id = i.org_id \
             WHERE i.origin = 'group' AND {} AND {} \
             ORDER BY i.last_member_notice_at LIMIT $1",
            digest_due_sql("i", "g"),
            group_watched_sql("i")
        ))
        .bind(cap)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("due member digests: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|(id, org)| super::DueDigest {
                id,
                org: OrgId(org),
            })
            .collect())
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
               WHERE ((status = 'failed' AND attempt < $1)
                      OR (status = 'queued' AND created_at < $4))
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
               SET status = $3, attempt = GREATEST(attempt, $4), error = $5, sent_at = $6,
                   next_attempt_at = $7, provider_receipt = COALESCE($8, provider_receipt)
               WHERE id = $1 AND org_id = $2
                 AND ($3 = 'sent' OR (status <> 'sent' AND attempt <= $4))"#,
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
        let rows: Vec<Row> = sqlx::query_as(&format!(
            r#"SELECT id, org_id, incident_id, channel_id, provider_receipt,
                      (SELECT count(*) FROM incident_events e
                       WHERE e.incident_id = n.incident_id AND e.org_id = n.org_id
                         AND e.kind IN {EPISODE_EVENT_KINDS}
                         AND e.occurred_at <= COALESCE(n.sent_at, n.created_at)) AS generation
               FROM incident_notifications n
               WHERE provider_receipt IS NOT NULL AND acked_at IS NULL
                 AND status = 'sent' AND channel_id IS NOT NULL
               ORDER BY sent_at ASC LIMIT $1"#
        ))
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
        let rows: Vec<Row> = sqlx::query_as(&format!(
            r#"SELECT id, org_id, incident_id, channel_id, provider_receipt,
                      (SELECT count(*) FROM incident_events e
                       WHERE e.incident_id = n.incident_id AND e.org_id = n.org_id
                         AND e.kind IN {EPISODE_EVENT_KINDS}
                         AND e.occurred_at <= COALESCE(n.sent_at, n.created_at)) AS generation
               FROM incident_notifications n
               WHERE org_id = $1 AND incident_id = $2
                 AND provider_receipt IS NOT NULL AND acked_at IS NULL
                 AND status = 'sent' AND channel_id IS NOT NULL"#
        ))
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

    // `target_id IS NOT NULL OR target_name IS NULL` leaves out an incident whose
    // monitor was deleted. The delete closes those; this keeps one left open by
    // any other path from being picked up every tick with nothing to page.
    // The owed-page and release scans join the monitor, which leaves those out
    // too.
    async fn due_for_escalation(
        &self,
        now: DateTime<Utc>,
        limit: usize,
        lease_secs: i64,
    ) -> Result<Vec<ClaimedEscalation>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            target_id: Option<Uuid>,
            escalation_policy_id: Option<Uuid>,
            escalation_level: i32,
            escalation_round: i32,
            next_escalation_at: DateTime<Utc>,
            episode: i64,
        }
        // Claim-and-lease: lock the due rows with SKIP LOCKED and push their
        // timer forward so a concurrent engine instance never re-selects them
        // before this one pages + records the real next time.
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "UPDATE incidents SET next_escalation_at = $1 + make_interval(secs => $2::double precision) \
             WHERE id IN ( \
                 SELECT id FROM incidents \
                 WHERE state = 'triggered' AND next_escalation_at IS NOT NULL \
                     AND next_escalation_at <= $1 AND recovering_since IS NULL \
                     AND (target_id IS NOT NULL OR target_name IS NULL) \
                 ORDER BY next_escalation_at ASC LIMIT $3 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING id, org_id, target_id, escalation_policy_id, escalation_level, \
                 escalation_round, next_escalation_at, \
                 (SELECT count(*) FROM incident_events e \
                  WHERE e.incident_id = incidents.id AND e.org_id = incidents.org_id \
                      AND e.kind IN {EPISODE_EVENT_KINDS}) AS episode"
        ))
        .bind(now)
        .bind(lease_secs.max(1) as f64)
        .bind(cap)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("due_for_escalation: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| ClaimedEscalation {
                due: DueIncident {
                    id: r.id,
                    org: OrgId(r.org_id),
                    target_id: r.target_id,
                    escalation_policy_id: r.escalation_policy_id,
                    escalation_level: r.escalation_level,
                    escalation_round: r.escalation_round,
                },
                claim: r.next_escalation_at,
                episode: r.episode,
            })
            .collect())
    }

    async fn due_for_owed_page(
        &self,
        cutoff: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<OwedPage>> {
        let cap = (limit as i64).clamp(1, 1000);
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            page_owed_at: DateTime<Utc>,
        }
        let sql = format!(
            "SELECT i.id, i.org_id, i.page_owed_at \
             FROM incidents i \
             LEFT JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id \
             WHERE i.page_owed_at IS NOT NULL AND i.page_owed_at <= $1 \
                 AND i.ended_at IS NULL AND {may_page} \
                 AND CASE WHEN i.origin = 'group' THEN {group_watched} \
                          ELSE t.enabled AND {NOT_HELD_PREDICATE} END \
             ORDER BY i.page_owed_at ASC LIMIT $2",
            may_page = may_page_predicate!("i"),
            group_watched = group_watched_sql("i"),
        );
        let rows: Vec<Row> = sqlx::query_as(&sql)
            .bind(cutoff)
            .bind(cap)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("due_for_owed_page: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| OwedPage {
                id: r.id,
                org: OrgId(r.org_id),
                owed_at: r.page_owed_at,
            })
            .collect())
    }

    async fn requeue_owed_page(&self, org: OrgId, id: Uuid, seen: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE incidents SET page_owed_at = now() \
             WHERE id = $1 AND org_id = $2 AND page_owed_at = $3",
        )
        .bind(id)
        .bind(org.0)
        .bind(seen)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("requeue_owed_page: {e}"))?;
        Ok(())
    }

    // Every closing-notice statement reads the database's clock, the one the
    // close stamped the notice with.
    async fn claim_closing_notice(
        &self,
        org: OrgId,
        id: Uuid,
        lease_secs: i64,
    ) -> Result<Option<DateTime<Utc>>> {
        sqlx::query_scalar(
            "UPDATE incidents \
             SET closing_notice_at = now() + make_interval(secs => $3::double precision) \
             WHERE id = $1 AND org_id = $2 AND state = 'resolved' AND closing_notice_at <= now() \
             RETURNING closing_notice_at",
        )
        .bind(id)
        .bind(org.0)
        .bind(lease_secs.max(1) as f64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("claim_closing_notice: {e}").into())
    }

    async fn due_closing_notices(
        &self,
        grace_secs: i64,
        window_secs: i64,
        lease_secs: i64,
        limit: usize,
    ) -> Result<DueClosingNotices> {
        let cap = (limit as i64).clamp(1, 1000);
        // One statement claims what is owed and withdraws what went stale, so
        // a notice nobody will send does not stay in the index.
        let rows: Vec<(Uuid, Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
            "WITH due AS ( \
                 SELECT id, org_id, state = 'resolved' \
                     AND ended_at >= now() - make_interval(secs => $2::double precision) AS owed \
                 FROM incidents \
                 WHERE closing_notice_at <= now() - make_interval(secs => $1::double precision) \
                 ORDER BY closing_notice_at ASC LIMIT $4 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             UPDATE incidents i \
             SET closing_notice_at = CASE WHEN due.owed \
                 THEN now() + make_interval(secs => $3::double precision) END \
             FROM due WHERE i.id = due.id AND i.org_id = due.org_id \
             RETURNING i.id, i.org_id, i.closing_notice_at",
        )
        .bind(grace_secs.max(0) as f64)
        .bind(window_secs.max(1) as f64)
        .bind(lease_secs.max(1) as f64)
        .bind(cap)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("due_closing_notices: {e}"))?;
        let mut due = DueClosingNotices::default();
        for (id, org_id, claim) in rows {
            match claim {
                Some(claim) => due.claimed.push(ClosingNotice {
                    id,
                    org: OrgId(org_id),
                    claim,
                }),
                None => due.expired += 1,
            }
        }
        Ok(due)
    }

    async fn record_closing_notification(
        &self,
        n: NewIncidentNotification,
        claim: DateTime<Utc>,
        lease_secs: i64,
    ) -> Result<Option<(Uuid, DateTime<Utc>)>> {
        // The renewal locks the incident row, so a sender taking the notice
        // over either waits for this row to commit and finds the lease
        // renewed, or commits first and this inserts nothing.
        sqlx::query_as(
            r#"WITH held AS (
                   UPDATE incidents
                   SET closing_notice_at = now() + make_interval(secs => $13::double precision)
                   WHERE id = $2 AND org_id = $1 AND closing_notice_at = $14
                   RETURNING id, org_id, closing_notice_at
               ), recorded AS (
                   INSERT INTO incident_notifications
                     (org_id, incident_id, escalation_level, target_user_id, channel_id,
                      transport, reason, status, attempt, error, sent_at, episode)
                   SELECT held.org_id, held.id, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
                   FROM held
                   RETURNING id
               )
               SELECT recorded.id, held.closing_notice_at FROM recorded, held"#,
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
        .bind(n.episode)
        .bind(lease_secs.max(1) as f64)
        .bind(claim)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("record_closing_notification: {e}").into())
    }

    async fn settle_closing_notice(
        &self,
        org: OrgId,
        id: Uuid,
        claim: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE incidents SET closing_notice_at = NULL \
             WHERE id = $1 AND org_id = $2 AND closing_notice_at = $3",
        )
        .bind(id)
        .bind(org.0)
        .bind(claim)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("settle_closing_notice: {e}"))?;
        Ok(())
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
        // yet) fails the `<=`, leaving an unpaged incident to its first page.
        // Each reminder doubles the next gap, capped at a day but never shorter
        // than the interval the monitor asked for. Each one writes a fresh row,
        // advancing the gate past the widened gap; within-interval delivery
        // retries are the retry sweep's job. The
        // `(org_id, incident_id, created_at)` index serves the max as an
        // index-only scan.
        // A group incident takes its interval from its group, and is watched
        // while one of its members still down is.
        let sql = format!(
            "SELECT i.id, i.org_id, i.target_id, i.escalation_policy_id, \
                    i.escalation_level, i.escalation_round \
             FROM incidents i \
             LEFT JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id \
             LEFT JOIN monitor_groups g \
                 ON i.origin = 'group' AND g.id = i.group_id AND g.org_id = i.org_id \
             CROSS JOIN LATERAL (SELECT COALESCE(t.renotify_interval_secs, g.renotify_interval_secs) \
                                     AS secs) r \
             WHERE i.state = 'triggered' AND i.ended_at IS NULL \
                 AND i.recovering_since IS NULL \
                 AND i.next_escalation_at IS NULL \
                 AND i.page_owed_at IS NULL \
                 AND i.group_incident_id IS NULL \
                 AND r.secs > 0 \
                 AND CASE WHEN g.id IS NOT NULL THEN {group_watched} \
                          ELSE t.id IS NOT NULL AND {not_held} AND NOT {window} END \
                 AND ( \
                     SELECT max(n.created_at) FROM incident_notifications n \
                     WHERE n.incident_id = i.id AND n.org_id = i.org_id \
                       AND n.channel_id IS NOT NULL \
                 ) <= $1 - make_interval(secs => GREATEST( \
                     r.secs::double precision, \
                     LEAST(r.secs::double precision \
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
            group_watched = group_watched_sql("i"),
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
        // the LATERAL nor the window join. A paused or held monitor's hold
        // waits for it to come back rather than being let go into nothing.
        let sql = format!(
            "SELECT i.id, i.org_id, i.target_id, i.escalation_policy_id, i.escalation_level, \
                    i.escalation_round \
             FROM incidents i \
             JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id \
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
               AND i.recovering_since IS NULL \
               AND i.group_incident_id IS NULL \
               AND t.enabled AND {not_held} \
               AND held.held_at IS NOT NULL \
               AND NOT EXISTS ( \
                   SELECT 1 FROM incident_notifications n \
                   WHERE n.incident_id = i.id AND n.org_id = i.org_id \
                     AND n.created_at > held.held_at \
               ) \
               AND NOT {window} \
             ORDER BY held.held_at ASC LIMIT $1",
            not_held = crate::storage::admin::NOT_HELD_PREDICATE,
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
        // held and released again on its own merits. A paused or held monitor's
        // hold waits for it to come back rather than being let go into nothing.
        let sql = format!(
            "SELECT i.id, i.org_id, i.target_id, i.escalation_policy_id, i.escalation_level, \
                    i.escalation_round \
             FROM incidents i \
             JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id \
             JOIN LATERAL ( \
                 SELECT max(created_at) AS held_at FROM incident_notifications h \
                 WHERE h.incident_id = i.id AND h.org_id = i.org_id \
                   AND h.channel_id IS NULL AND h.transport = 'damped' \
             ) held ON TRUE \
             WHERE i.state = 'triggered' AND i.recovering_since IS NULL \
               AND i.group_incident_id IS NULL \
               AND t.enabled AND {not_held} \
               AND held.held_at IS NOT NULL \
               AND held.held_at <= $1 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM incident_notifications n \
                   WHERE n.incident_id = i.id AND n.org_id = i.org_id \
                     AND n.created_at > held.held_at \
               ) \
             ORDER BY held.held_at ASC LIMIT $2",
            not_held = crate::storage::admin::NOT_HELD_PREDICATE,
        );
        let rows: Vec<Row> = sqlx::query_as(&sql)
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
