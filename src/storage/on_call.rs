//! Storage for on-call schedules — the rotation layers + participants the
//! resolver walks, and the one-off overrides that swap coverage.
//!
//! Every method is org-scoped (`org: OrgId`), mirroring [`super::TargetStore`].
//! Schedules are soft-deleted (`deleted_at`) so a deleted schedule stops paging
//! ([`OnCallStore::get`] hides it) while in-flight references resolve to no one
//! rather than a foreign row. Participants and override users are validated to
//! be members of `org` on write (the parent-chain org-match trigger guards the
//! schedule↔layer chain, not the user reference, so this closes the IDOR).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    NewOnCallLayer, NewOnCallOverride, NewOnCallSchedule, OnCallLayer, OnCallOverride,
    OnCallParticipant, OnCallSchedule, OnCallScheduleDetail, OnCallScheduleSummary, OrgId,
    RotationType, UserId, resolve_on_call,
};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::storage::accounts;
use crate::storage::locks::{account_lock_key, advisory_xact_lock};

#[async_trait]
pub trait OnCallStore: Send + Sync {
    /// Lightweight index (no layers loaded), newest first.
    async fn list(&self, org: OrgId) -> Result<Vec<OnCallScheduleSummary>>;
    /// Full schedule with ordered layers + participants + overrides. `None`
    /// for a missing or soft-deleted schedule.
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<OnCallScheduleDetail>>;
    /// Create one schedule with its layer stack. Atomically capped at
    /// `max_schedules`; a duplicate name yields `ON_CALL_SCHEDULE_NAME_TAKEN`.
    async fn create(
        &self,
        org: OrgId,
        new: NewOnCallSchedule,
        max_schedules: i64,
    ) -> Result<OnCallScheduleDetail>;
    /// Replace a schedule's metadata + entire layer stack (overrides untouched).
    async fn replace(
        &self,
        org: OrgId,
        id: Uuid,
        new: NewOnCallSchedule,
    ) -> Result<Option<OnCallScheduleDetail>>;
    /// Soft-delete. Returns `false` when nothing live matched.
    async fn delete(&self, org: OrgId, id: Uuid) -> Result<bool>;
    /// Add a coverage override. `None` when the schedule is missing/deleted.
    async fn add_override(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        created_by: Option<UserId>,
        new: NewOnCallOverride,
    ) -> Result<Option<OnCallOverride>>;
    /// Remove an override. `false` when nothing matched.
    async fn delete_override(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        override_id: Uuid,
    ) -> Result<bool>;
    /// Who is on call for a schedule at `at` — the pure resolver over the live
    /// rows. Empty for a missing/deleted schedule.
    async fn resolve_now(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<Vec<UserId>> {
        match self.get(org, schedule_id).await? {
            Some(d) => Ok(resolve_on_call(&d.schedule, &d.layers, &d.overrides, at)),
            None => Ok(vec![]),
        }
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.is_unique_violation())
}

fn name_taken() -> AppError {
    AppError::unprocessable(
        codes::ON_CALL_SCHEDULE_NAME_TAKEN,
        "an on-call schedule with this name already exists",
    )
}

fn not_member() -> AppError {
    AppError::unprocessable(
        codes::ON_CALL_SCHEDULE_INVALID,
        "schedule references a user who is not a member of this organization",
    )
}

// ── Postgres impl ────────────────────────────────────────────────────────

pub struct PgOnCallStore {
    pool: PgPool,
}

impl PgOnCallStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct ScheduleRow {
    id: Uuid,
    name: String,
    timezone: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct LayerRow {
    id: Uuid,
    name: Option<String>,
    rotation_type: String,
    rotation_length_secs: i32,
    handoff_at: DateTime<Utc>,
    layer_order: i32,
    created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct ParticipantRow {
    id: Uuid,
    layer_id: Uuid,
    user_id: Uuid,
    position: i32,
}

#[derive(sqlx::FromRow)]
struct OverrideRow {
    id: Uuid,
    user_id: Uuid,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
}

/// Validate `user_id` is an active member of `org` inside an open transaction.
async fn assert_member_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    user_id: UserId,
) -> Result<()> {
    let ok: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM memberships WHERE user_id = $1 AND org_id = $2")
            .bind(user_id.0)
            .bind(org.0)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("validate member: {e}")))?;
    if ok.is_none() {
        return Err(not_member());
    }
    Ok(())
}

/// Insert a schedule's layers + participants inside an open transaction.
async fn insert_layers_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    schedule_id: Uuid,
    layers: &[NewOnCallLayer],
) -> Result<()> {
    for layer in layers {
        let layer_id: Uuid = sqlx::query_scalar(
            r#"INSERT INTO on_call_layers
                   (org_id, schedule_id, name, rotation_type, rotation_length_secs,
                    handoff_at, layer_order)
               VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id"#,
        )
        .bind(org.0)
        .bind(schedule_id)
        .bind(&layer.name)
        .bind(layer.rotation_type.as_db_str())
        .bind(layer.rotation_length_secs)
        .bind(layer.handoff_at)
        .bind(layer.layer_order)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("insert on_call_layer: {e}")))?;
        for (position, p) in layer.participants.iter().enumerate() {
            assert_member_tx(tx, org, p.user_id).await?;
            sqlx::query(
                "INSERT INTO on_call_participants (org_id, layer_id, user_id, position) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(org.0)
            .bind(layer_id)
            .bind(p.user_id.0)
            .bind(position as i32)
            .execute(&mut **tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("insert on_call_participant: {e}")))?;
        }
    }
    Ok(())
}

fn assemble(
    schedule: ScheduleRow,
    layers: Vec<LayerRow>,
    participants: Vec<ParticipantRow>,
    overrides: Vec<OverrideRow>,
) -> OnCallScheduleDetail {
    let mut out: Vec<OnCallLayer> = layers
        .into_iter()
        .map(|l| OnCallLayer {
            id: l.id,
            name: l.name,
            rotation_type: RotationType::from_db_str(&l.rotation_type),
            rotation_length_secs: l.rotation_length_secs,
            handoff_at: l.handoff_at,
            layer_order: l.layer_order,
            created_at: l.created_at,
            participants: vec![],
        })
        .collect();
    for p in participants {
        if let Some(layer) = out.iter_mut().find(|l| l.id == p.layer_id) {
            layer.participants.push(OnCallParticipant {
                id: p.id,
                user_id: UserId(p.user_id),
                position: p.position,
            });
        }
    }
    OnCallScheduleDetail {
        schedule: OnCallSchedule {
            id: schedule.id,
            name: schedule.name,
            timezone: schedule.timezone,
            created_at: schedule.created_at,
            updated_at: schedule.updated_at,
        },
        layers: out,
        overrides: overrides
            .into_iter()
            .map(|o| OnCallOverride {
                id: o.id,
                user_id: UserId(o.user_id),
                starts_at: o.starts_at,
                ends_at: o.ends_at,
                created_by: o.created_by.map(UserId),
                created_at: o.created_at,
            })
            .collect(),
    }
}

impl PgOnCallStore {
    async fn hydrate(&self, org: OrgId, schedule: ScheduleRow) -> Result<OnCallScheduleDetail> {
        let id = schedule.id;
        let layers: Vec<LayerRow> = sqlx::query_as(
            "SELECT id, name, rotation_type, rotation_length_secs, handoff_at, layer_order, \
                created_at \
             FROM on_call_layers WHERE org_id = $1 AND schedule_id = $2 ORDER BY layer_order, created_at",
        )
        .bind(org.0)
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("load on_call_layers: {e}")))?;
        let participants: Vec<ParticipantRow> = sqlx::query_as(
            "SELECT p.id, p.layer_id, p.user_id, p.position \
             FROM on_call_participants p JOIN on_call_layers l ON l.id = p.layer_id \
             WHERE p.org_id = $1 AND l.schedule_id = $2 ORDER BY p.layer_id, p.position",
        )
        .bind(org.0)
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("load on_call_participants: {e}")))?;
        let overrides: Vec<OverrideRow> = sqlx::query_as(
            "SELECT id, user_id, starts_at, ends_at, created_by, created_at \
             FROM on_call_overrides WHERE org_id = $1 AND schedule_id = $2 ORDER BY starts_at",
        )
        .bind(org.0)
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("load on_call_overrides: {e}")))?;
        Ok(assemble(schedule, layers, participants, overrides))
    }

    async fn schedule_row(&self, org: OrgId, id: Uuid) -> Result<Option<ScheduleRow>> {
        sqlx::query_as(
            "SELECT id, name, timezone, created_at, updated_at FROM on_call_schedules \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("get on_call_schedule: {e}")))
    }
}

#[async_trait]
impl OnCallStore for PgOnCallStore {
    async fn list(&self, org: OrgId) -> Result<Vec<OnCallScheduleSummary>> {
        let rows: Vec<(Uuid, String, String, i64, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT s.id, s.name, s.timezone, \
                (SELECT count(*) FROM on_call_layers l WHERE l.schedule_id = s.id), \
                s.created_at, s.updated_at \
             FROM on_call_schedules s \
             WHERE s.org_id = $1 AND s.deleted_at IS NULL \
             ORDER BY s.created_at DESC",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("list on_call_schedules: {e}")))?;
        Ok(rows
            .into_iter()
            .map(
                |(id, name, timezone, layer_count, created_at, updated_at)| OnCallScheduleSummary {
                    id,
                    name,
                    timezone,
                    layer_count,
                    created_at,
                    updated_at,
                },
            )
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<OnCallScheduleDetail>> {
        match self.schedule_row(org, id).await? {
            Some(s) => Ok(Some(self.hydrate(org, s).await?)),
            None => Ok(None),
        }
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewOnCallSchedule,
        max_schedules: i64,
    ) -> Result<OnCallScheduleDetail> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("advisory lock: {e}")))?;
        let pool_orgs = accounts::live_orgs("$5");
        let row: Option<(Uuid,)> = sqlx::query_as(&format!(
            r#"INSERT INTO on_call_schedules (org_id, name, timezone)
               SELECT $1, $2, $3
               WHERE (SELECT count(*) FROM on_call_schedules
                      WHERE org_id IN ({pool_orgs}) AND deleted_at IS NULL) < $4
               RETURNING id"#
        ))
        .bind(org.0)
        .bind(&new.name)
        .bind(&new.timezone)
        .bind(max_schedules)
        .bind(account.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                name_taken()
            } else {
                AppError::Other(anyhow::anyhow!("insert on_call_schedule: {e}"))
            }
        })?;
        let Some((id,)) = row else {
            tx.rollback().await.ok();
            return Err(AppError::unprocessable(
                codes::ON_CALL_SCHEDULE_QUOTA_EXCEEDED,
                "on-call schedule limit reached for this plan",
            ));
        };
        insert_layers_tx(&mut tx, org, id, &new.layers).await?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        self.get(org, id).await?.ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "on-call schedule {id} not found immediately after create"
            ))
        })
    }

    async fn replace(
        &self,
        org: OrgId,
        id: Uuid,
        new: NewOnCallSchedule,
    ) -> Result<Option<OnCallScheduleDetail>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        let exists: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM on_call_schedules \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("lock on_call_schedule: {e}")))?;
        if exists.is_none() {
            return Ok(None);
        }
        sqlx::query(
            "UPDATE on_call_schedules SET name = $3, timezone = $4, updated_at = now() \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .bind(&new.name)
        .bind(&new.timezone)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                name_taken()
            } else {
                AppError::Other(anyhow::anyhow!("update on_call_schedule: {e}"))
            }
        })?;
        sqlx::query("DELETE FROM on_call_layers WHERE schedule_id = $1 AND org_id = $2")
            .bind(id)
            .bind(org.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("clear on_call_layers: {e}")))?;
        insert_layers_tx(&mut tx, org, id, &new.layers).await?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        self.get(org, id).await
    }

    async fn delete(&self, org: OrgId, id: Uuid) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE on_call_schedules SET deleted_at = now() \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("delete on_call_schedule: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn add_override(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        created_by: Option<UserId>,
        new: NewOnCallOverride,
    ) -> Result<Option<OnCallOverride>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        let live: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM on_call_schedules \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(schedule_id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("check schedule: {e}")))?;
        if live.is_none() {
            return Ok(None);
        }
        assert_member_tx(&mut tx, org, new.user_id).await?;
        let row: OverrideRow = sqlx::query_as(
            r#"INSERT INTO on_call_overrides
                   (org_id, schedule_id, user_id, starts_at, ends_at, created_by)
               VALUES ($1, $2, $3, $4, $5, $6)
               RETURNING id, user_id, starts_at, ends_at, created_by, created_at"#,
        )
        .bind(org.0)
        .bind(schedule_id)
        .bind(new.user_id.0)
        .bind(new.starts_at)
        .bind(new.ends_at)
        .bind(created_by.map(|u| u.0))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("insert on_call_override: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        Ok(Some(OnCallOverride {
            id: row.id,
            user_id: UserId(row.user_id),
            starts_at: row.starts_at,
            ends_at: row.ends_at,
            created_by: row.created_by.map(UserId),
            created_at: row.created_at,
        }))
    }

    async fn delete_override(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        override_id: Uuid,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM on_call_overrides WHERE id = $1 AND schedule_id = $2 AND org_id = $3",
        )
        .bind(override_id)
        .bind(schedule_id)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("delete on_call_override: {e}")))?;
        Ok(result.rows_affected() > 0)
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────

#[derive(Default)]
pub struct InMemoryOnCallStore {
    inner: Mutex<MemState>,
}

#[derive(Default)]
struct MemState {
    schedules: Vec<(OrgId, OnCallScheduleDetail, bool)>,
    members: Vec<(OrgId, UserId)>,
}

impl InMemoryOnCallStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test seam: register a member so participant/override validation passes.
    pub fn add_member(&self, org: OrgId, user: UserId) {
        self.inner.lock().members.push((org, user));
    }
}

fn materialise(id: Uuid, new: &NewOnCallSchedule, now: DateTime<Utc>) -> OnCallScheduleDetail {
    OnCallScheduleDetail {
        schedule: OnCallSchedule {
            id,
            name: new.name.clone(),
            timezone: new.timezone.clone(),
            created_at: now,
            updated_at: now,
        },
        layers: new
            .layers
            .iter()
            .map(|l| OnCallLayer {
                id: Uuid::now_v7(),
                name: l.name.clone(),
                rotation_type: l.rotation_type,
                rotation_length_secs: l.rotation_length_secs,
                handoff_at: l.handoff_at,
                layer_order: l.layer_order,
                created_at: now,
                participants: l
                    .participants
                    .iter()
                    .enumerate()
                    .map(|(pos, p)| OnCallParticipant {
                        id: Uuid::now_v7(),
                        user_id: p.user_id,
                        position: pos as i32,
                    })
                    .collect(),
            })
            .collect(),
        overrides: vec![],
    }
}

impl InMemoryOnCallStore {
    fn assert_members(g: &MemState, org: OrgId, new: &NewOnCallSchedule) -> Result<()> {
        for l in &new.layers {
            for p in &l.participants {
                if !g.members.iter().any(|(o, u)| *o == org && *u == p.user_id) {
                    return Err(not_member());
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl OnCallStore for InMemoryOnCallStore {
    async fn list(&self, org: OrgId) -> Result<Vec<OnCallScheduleSummary>> {
        Ok(self
            .inner
            .lock()
            .schedules
            .iter()
            .filter(|(o, _, deleted)| *o == org && !*deleted)
            .map(|(_, d, _)| OnCallScheduleSummary {
                id: d.schedule.id,
                name: d.schedule.name.clone(),
                timezone: d.schedule.timezone.clone(),
                layer_count: d.layers.len() as i64,
                created_at: d.schedule.created_at,
                updated_at: d.schedule.updated_at,
            })
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<OnCallScheduleDetail>> {
        Ok(self
            .inner
            .lock()
            .schedules
            .iter()
            .find(|(o, d, deleted)| *o == org && d.schedule.id == id && !*deleted)
            .map(|(_, d, _)| d.clone()))
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewOnCallSchedule,
        max_schedules: i64,
    ) -> Result<OnCallScheduleDetail> {
        let mut g = self.inner.lock();
        if g.schedules
            .iter()
            .any(|(o, d, deleted)| *o == org && !*deleted && d.schedule.name == new.name)
        {
            return Err(name_taken());
        }
        if g.schedules
            .iter()
            .filter(|(o, _, deleted)| *o == org && !*deleted)
            .count() as i64
            >= max_schedules
        {
            return Err(AppError::unprocessable(
                codes::ON_CALL_SCHEDULE_QUOTA_EXCEEDED,
                "on-call schedule limit reached for this plan",
            ));
        }
        Self::assert_members(&g, org, &new)?;
        let detail = materialise(Uuid::now_v7(), &new, Utc::now());
        g.schedules.push((org, detail.clone(), false));
        Ok(detail)
    }

    async fn replace(
        &self,
        org: OrgId,
        id: Uuid,
        new: NewOnCallSchedule,
    ) -> Result<Option<OnCallScheduleDetail>> {
        let mut g = self.inner.lock();
        if g.schedules.iter().any(|(o, d, deleted)| {
            *o == org && !*deleted && d.schedule.id != id && d.schedule.name == new.name
        }) {
            return Err(name_taken());
        }
        let Some((created_at, overrides)) = g
            .schedules
            .iter()
            .find(|(o, d, deleted)| *o == org && d.schedule.id == id && !*deleted)
            .map(|(_, d, _)| (d.schedule.created_at, d.overrides.clone()))
        else {
            return Ok(None);
        };
        Self::assert_members(&g, org, &new)?;
        let mut detail = materialise(id, &new, Utc::now());
        detail.schedule.created_at = created_at;
        detail.overrides = overrides;
        if let Some(slot) = g
            .schedules
            .iter_mut()
            .find(|(o, d, deleted)| *o == org && d.schedule.id == id && !*deleted)
        {
            slot.1 = detail.clone();
        }
        Ok(Some(detail))
    }

    async fn delete(&self, org: OrgId, id: Uuid) -> Result<bool> {
        let mut g = self.inner.lock();
        let found = g
            .schedules
            .iter_mut()
            .find(|(o, d, deleted)| *o == org && d.schedule.id == id && !*deleted);
        match found {
            Some(slot) => {
                slot.2 = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn add_override(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        created_by: Option<UserId>,
        new: NewOnCallOverride,
    ) -> Result<Option<OnCallOverride>> {
        let mut g = self.inner.lock();
        if !g
            .members
            .iter()
            .any(|(o, u)| *o == org && *u == new.user_id)
        {
            return Err(not_member());
        }
        let ov = OnCallOverride {
            id: Uuid::now_v7(),
            user_id: new.user_id,
            starts_at: new.starts_at,
            ends_at: new.ends_at,
            created_by,
            created_at: Utc::now(),
        };
        match g
            .schedules
            .iter_mut()
            .find(|(o, d, deleted)| *o == org && d.schedule.id == schedule_id && !*deleted)
        {
            Some(slot) => {
                slot.1.overrides.push(ov.clone());
                Ok(Some(ov))
            }
            None => Ok(None),
        }
    }

    async fn delete_override(
        &self,
        org: OrgId,
        schedule_id: Uuid,
        override_id: Uuid,
    ) -> Result<bool> {
        let mut g = self.inner.lock();
        let Some(slot) = g
            .schedules
            .iter_mut()
            .find(|(o, d, deleted)| *o == org && d.schedule.id == schedule_id && !*deleted)
        else {
            return Ok(false);
        };
        let before = slot.1.overrides.len();
        slot.1.overrides.retain(|o| o.id != override_id);
        Ok(slot.1.overrides.len() != before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NewOnCallLayer, NewOnCallParticipant};

    fn org() -> OrgId {
        OrgId(Uuid::from_u128(0xA1))
    }
    fn user(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    fn schedule(name: &str, participants: Vec<UserId>) -> NewOnCallSchedule {
        NewOnCallSchedule {
            name: name.into(),
            timezone: "UTC".into(),
            layers: vec![NewOnCallLayer {
                name: None,
                rotation_type: RotationType::Daily,
                rotation_length_secs: 86_400,
                handoff_at: "2026-06-01T00:00:00Z".parse().unwrap(),
                layer_order: 0,
                participants: participants
                    .into_iter()
                    .map(|u| NewOnCallParticipant { user_id: u })
                    .collect(),
            }],
        }
    }

    async fn store_with_members() -> InMemoryOnCallStore {
        let store = InMemoryOnCallStore::new();
        store.add_member(org(), user(1));
        store.add_member(org(), user(2));
        store
    }

    #[tokio::test]
    async fn create_get_list_delete_roundtrip() {
        let store = store_with_members().await;
        let d = store
            .create(org(), schedule("primary", vec![user(1), user(2)]), 10)
            .await
            .unwrap();
        assert_eq!(d.layers.len(), 1);
        assert_eq!(d.layers[0].participants.len(), 2);
        assert_eq!(store.list(org()).await.unwrap().len(), 1);
        assert!(store.get(org(), d.schedule.id).await.unwrap().is_some());
        assert!(store.delete(org(), d.schedule.id).await.unwrap());
        assert!(store.get(org(), d.schedule.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_non_member_participant() {
        let store = store_with_members().await;
        let err = store
            .create(org(), schedule("p", vec![user(1), user(99)]), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Unprocessable { .. }));
    }

    #[tokio::test]
    async fn enforces_cap_and_unique_name() {
        let store = store_with_members().await;
        store
            .create(org(), schedule("a", vec![user(1)]), 1)
            .await
            .unwrap();
        let over = store
            .create(org(), schedule("b", vec![user(1)]), 1)
            .await
            .unwrap_err();
        assert!(matches!(over, AppError::Unprocessable { .. }));
        let dup = store
            .create(org(), schedule("a", vec![user(1)]), 10)
            .await
            .unwrap_err();
        assert!(matches!(dup, AppError::Unprocessable { .. }));
    }

    #[tokio::test]
    async fn resolve_now_walks_the_rotation() {
        let store = store_with_members().await;
        let d = store
            .create(org(), schedule("p", vec![user(1), user(2)]), 10)
            .await
            .unwrap();
        let day0 = "2026-06-01T12:00:00Z".parse().unwrap();
        let day1 = "2026-06-02T12:00:00Z".parse().unwrap();
        assert_eq!(
            store.resolve_now(org(), d.schedule.id, day0).await.unwrap(),
            vec![user(1)]
        );
        assert_eq!(
            store.resolve_now(org(), d.schedule.id, day1).await.unwrap(),
            vec![user(2)]
        );
    }

    #[tokio::test]
    async fn override_replaces_resolution_in_window() {
        let store = store_with_members().await;
        store.add_member(org(), user(9));
        let d = store
            .create(org(), schedule("p", vec![user(1), user(2)]), 10)
            .await
            .unwrap();
        let ov = store
            .add_override(
                org(),
                d.schedule.id,
                None,
                NewOnCallOverride {
                    user_id: user(9),
                    starts_at: "2026-06-01T00:00:00Z".parse().unwrap(),
                    ends_at: "2026-06-01T23:59:59Z".parse().unwrap(),
                },
            )
            .await
            .unwrap()
            .unwrap();
        let at = "2026-06-01T12:00:00Z".parse().unwrap();
        assert_eq!(
            store.resolve_now(org(), d.schedule.id, at).await.unwrap(),
            vec![user(9)]
        );
        assert!(
            store
                .delete_override(org(), d.schedule.id, ov.id)
                .await
                .unwrap()
        );
        assert_eq!(
            store.resolve_now(org(), d.schedule.id, at).await.unwrap(),
            vec![user(1)]
        );
    }
}
