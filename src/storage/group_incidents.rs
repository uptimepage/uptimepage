//! A group that alerts as one: its members' incidents attach to one group
//! incident, which closes with the last of them.
//!
//! LOCK ORDER: the group row (`FOR NO KEY UPDATE`), then the group incident,
//! then the member incident, each incident by its advisory lock and then its
//! row. Attach and close both start at the group row, so neither can slip
//! past the other.

use anyhow::anyhow;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{IncidentEventKind, OrgId};
use crate::error::{AppError, Result};
use crate::storage::locks::{advisory_xact_lock, incident_lock_key};
use crate::storage::monitor_groups::lock_group;

/// What attaching a member incident did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// Joined the group's open incident, or opened it (`opened`), as member
    /// number `seq`.
    Joined {
        group_incident: Uuid,
        seq: i32,
        opened: bool,
    },
    /// Not routed to a group, or its group was deleted: it pages on its own.
    Standalone,
    /// Closed or attached already: nothing is owed.
    Settled,
}

#[derive(sqlx::FromRow)]
struct PendingMember {
    started_at: chrono::DateTime<chrono::Utc>,
    status_at_start: String,
    target_id: Option<Uuid>,
    target_name: Option<String>,
}

fn db(context: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |e| AppError::Other(anyhow!("{context}: {e}"))
}

pub(crate) async fn lock_incident(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<bool> {
    advisory_xact_lock(&mut **tx, &incident_lock_key(id))
        .await
        .map_err(db("incident lock"))?;
    let open: Option<bool> = sqlx::query_scalar(
        "SELECT ended_at IS NULL FROM incidents WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("lock incident"))?;
    Ok(open == Some(true))
}

/// Attaches a routed member incident to its group's open incident, opening
/// one owed its first page when there is none. The member's own page is
/// settled: the group pages for it.
pub async fn attach_member(pool: &PgPool, org: OrgId, member: Uuid) -> Result<Attach> {
    let mut tx = pool.begin().await.map_err(db("attach: begin"))?;
    let route: Option<(Option<Uuid>, bool)> = sqlx::query_as(
        "SELECT routed_group_id, ended_at IS NULL AND group_incident_id IS NULL \
         FROM incidents WHERE id = $1 AND org_id = $2 AND origin = 'monitor'",
    )
    .bind(member)
    .bind(org.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db("attach: read route"))?;
    let Some((route, pending)) = route else {
        return Ok(Attach::Settled);
    };
    if !pending {
        return Ok(Attach::Settled);
    }
    let Some(group) = route else {
        return Ok(Attach::Standalone);
    };
    if !lock_group(&mut tx, org, group).await? {
        return Ok(Attach::Standalone);
    }
    let open: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM incidents \
         WHERE org_id = $1 AND group_id = $2 AND origin = 'group' AND ended_at IS NULL",
    )
    .bind(org.0)
    .bind(group)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db("attach: open group incident"))?;
    let mut group_incident = None;
    if let Some(id) = open
        && lock_incident(&mut tx, org, id).await?
    {
        group_incident = Some(id);
    }

    advisory_xact_lock(&mut *tx, &incident_lock_key(member))
        .await
        .map_err(db("attach: member lock"))?;
    let still: Option<PendingMember> = sqlx::query_as(
        "SELECT started_at, status_at_start, target_id, target_name FROM incidents \
             WHERE id = $1 AND org_id = $2 AND routed_group_id = $3 \
                 AND ended_at IS NULL AND group_incident_id IS NULL \
             FOR UPDATE",
    )
    .bind(member)
    .bind(org.0)
    .bind(group)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db("attach: lock member"))?;
    let Some(PendingMember {
        started_at,
        status_at_start,
        target_id,
        target_name,
    }) = still
    else {
        return Ok(Attach::Settled);
    };

    let opened = group_incident.is_none();
    let group_incident = match group_incident {
        Some(id) => id,
        None => sqlx::query_scalar(
            "INSERT INTO incidents \
                 (org_id, group_id, origin, started_at, status_at_start, visibility, page_owed_at) \
             VALUES ($1, $2, 'group', $3, $4, 'internal', now()) RETURNING id",
        )
        .bind(org.0)
        .bind(group)
        .bind(started_at)
        .bind(&status_at_start)
        .fetch_one(&mut *tx)
        .await
        .map_err(db("attach: open group incident"))?,
    };
    // The group is down while any member is: a down member raises a group
    // incident a degraded one opened.
    let (seq, raised): (i32, bool) = sqlx::query_as(
        "WITH before AS (SELECT status_at_start FROM incidents WHERE id = $1 AND org_id = $2) \
         UPDATE incidents SET member_seq = member_seq + 1, \
             status_at_start = CASE WHEN $3 = 'down' THEN 'down' ELSE status_at_start END, \
             updated_at = now() \
         WHERE id = $1 AND org_id = $2 \
         RETURNING member_seq, $3 = 'down' AND (SELECT status_at_start FROM before) <> 'down'",
    )
    .bind(group_incident)
    .bind(org.0)
    .bind(&status_at_start)
    .fetch_one(&mut *tx)
    .await
    .map_err(db("attach: number member"))?;
    if raised {
        note_group_raised(&mut tx, org, group_incident).await?;
    }
    sqlx::query(
        "UPDATE incidents SET group_incident_id = $3, group_member_seq = $4, \
             page_owed_at = NULL, updated_at = now() \
         WHERE id = $1 AND org_id = $2",
    )
    .bind(member)
    .bind(org.0)
    .bind(group_incident)
    .bind(seq)
    .execute(&mut *tx)
    .await
    .map_err(db("attach: join"))?;
    log_member_event(
        &mut tx,
        org,
        group_incident,
        IncidentEventKind::MemberDown,
        member,
        target_id,
        target_name,
        Some(seq),
    )
    .await?;
    tx.commit().await.map_err(db("attach: commit"))?;
    Ok(Attach::Joined {
        group_incident,
        seq,
        opened,
    })
}

/// What [`lock_group_of`] took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupLock {
    /// Not a group incident, or its group already deleted: nothing to take.
    NotGroup,
    /// The group incident's group row, held until the transaction ends.
    Held,
    /// Its group was deleted since the incident was read.
    Gone,
}

/// Takes a group incident's group row, which attach and close take before any
/// incident's, ahead of the incident's own locks.
pub(crate) async fn lock_group_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    incident: Uuid,
) -> Result<GroupLock> {
    let group: Option<Uuid> = sqlx::query_scalar(
        "SELECT group_id FROM incidents WHERE id = $1 AND org_id = $2 AND origin = 'group'",
    )
    .bind(incident)
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("group of incident"))?
    .flatten();
    let Some(group) = group else {
        return Ok(GroupLock::NotGroup);
    };
    Ok(match lock_group(tx, org, group).await? {
        true => GroupLock::Held,
        false => GroupLock::Gone,
    })
}

/// Where a routed member's close stands once [`lock_for_member_close`] holds
/// its locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberClose {
    /// No longer open.
    Gone,
    /// Its group was deleted since the route was read, which cleared the
    /// route: it closes as a monitor of its own.
    Unrouted,
    /// Still routed, with the group's locks held.
    Routed,
}

/// Takes the locks a routed member's close needs ahead of the member row: its
/// group's row and, when attached, the group incident.
pub(crate) async fn lock_for_member_close(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    member: Uuid,
    group: Uuid,
) -> Result<MemberClose> {
    if !lock_group(tx, org, group).await? {
        return Ok(MemberClose::Unrouted);
    }
    let attached: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT group_incident_id FROM incidents \
         WHERE id = $1 AND org_id = $2 AND routed_group_id = $3 AND ended_at IS NULL",
    )
    .bind(member)
    .bind(org.0)
    .bind(group)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("close: read attachment"))?;
    let Some(attached) = attached else {
        return Ok(MemberClose::Gone);
    };
    if let Some(group_incident) = attached {
        lock_incident(tx, org, group_incident).await?;
    }
    advisory_xact_lock(&mut **tx, &incident_lock_key(member))
        .await
        .map_err(db("close: member lock"))?;
    Ok(MemberClose::Routed)
}

/// Why an attached member's incident closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberLeft {
    Recovered,
    /// Its monitor was deleted, by this user when one is named.
    Deleted(Option<crate::domain::UserId>),
}

/// Records an attached member's close on its group incident, and closes the
/// group incident with the last member down: as a recovery, or as the
/// monitor-deleted close when the last one left by its monitor's delete. The
/// group's notice then rides `closing_notice_at` like any closed incident's.
/// True when it closed.
pub(crate) async fn member_closed(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    group_incident: Uuid,
    member: Uuid,
    left: MemberLeft,
) -> Result<bool> {
    let (target_id, target_name): (Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT target_id, target_name FROM incidents WHERE id = $1 AND org_id = $2",
    )
    .bind(member)
    .bind(org.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(db("close: read member"))?;
    let (kind, deleted_by) = match left {
        MemberLeft::Recovered => (IncidentEventKind::MemberUp, None),
        MemberLeft::Deleted(by) => (IncidentEventKind::MonitorDeleted, by),
    };
    log_member_event(
        tx,
        org,
        group_incident,
        kind,
        member,
        target_id,
        target_name,
        None,
    )
    .await?;
    let closed = sqlx::query(
        "UPDATE incidents i SET ended_at = m.ended_at, \
             duration_secs = GREATEST(0, EXTRACT(EPOCH FROM (m.ended_at - i.started_at))::int), \
             state = 'resolved', resolved_by = $4, next_escalation_at = NULL, \
             recovering_since = NULL, closing_notice_at = now(), resolved_at = now(), \
             closed_by_monitor_delete = $3, updated_at = now() \
         FROM (SELECT max(ended_at) AS ended_at, bool_and(ended_at IS NOT NULL) AS all_up \
                 FROM incidents WHERE org_id = $2 AND group_incident_id = $1) m \
         WHERE i.id = $1 AND i.org_id = $2 AND i.ended_at IS NULL AND m.all_up",
    )
    .bind(group_incident)
    .bind(org.0)
    .bind(matches!(left, MemberLeft::Deleted(_)))
    .bind(deleted_by.map(|u| u.0))
    .execute(&mut **tx)
    .await
    .map_err(db("close: close group incident"))?;
    Ok(closed.rows_affected() > 0)
}

/// The timeline line a group incident gets when a member takes it to down.
pub(crate) const GROUP_RAISED_NOTE: &str = "raised to down: a member is confirmed down";

async fn note_group_raised(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    group_incident: Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO incident_events (org_id, incident_id, kind, actor_type, message) \
         VALUES ($1, $2, $3, 'system', $4)",
    )
    .bind(org.0)
    .bind(group_incident)
    .bind(IncidentEventKind::Note.as_db_str())
    .bind(GROUP_RAISED_NOTE)
    .execute(&mut **tx)
    .await
    .map_err(db("note group raised"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn log_member_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    group_incident: Uuid,
    kind: IncidentEventKind,
    member: Uuid,
    target_id: Option<Uuid>,
    target_name: Option<String>,
    seq: Option<i32>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO incident_events (org_id, incident_id, kind, actor_type, detail, message) \
         VALUES ($1, $2, $3, 'system', \
                 jsonb_strip_nulls(jsonb_build_object('incident_id', $4::uuid, \
                     'target_id', $5::uuid, 'seq', $6::int)), $7)",
    )
    .bind(org.0)
    .bind(group_incident)
    .bind(kind.as_db_str())
    .bind(member)
    .bind(target_id)
    .bind(seq)
    .bind(target_name)
    .execute(&mut **tx)
    .await
    .map_err(db("log member event"))?;
    Ok(())
}
