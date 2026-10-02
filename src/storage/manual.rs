//! Storage for `manual_monitors`: the state an operator last set on a
//! manual-kind target, written with its audit row in one transaction.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{ManualState, ManualStatus, OrgId, UserId};
use crate::error::{AppError, Result};

/// What a set did. `prev == state` when it changed nothing.
#[derive(Debug, Clone)]
pub struct ManualChange {
    pub prev: ManualState,
    pub state: ManualState,
}

impl ManualChange {
    pub fn changed(&self) -> bool {
        !self.prev.matches(self.state.status, &self.state.note)
    }
}

#[async_trait]
pub trait ManualStore: Send + Sync {
    /// `None` until the first set.
    async fn get(&self, org: OrgId, target_id: Uuid) -> Result<Option<ManualState>>;
    /// `None` when the target is not a manual monitor in `org`. A set that
    /// changes nothing writes nothing, audit included.
    async fn set(
        &self,
        org: OrgId,
        target_id: Uuid,
        status: ManualStatus,
        note: Option<String>,
        actor: Option<UserId>,
    ) -> Result<Option<ManualChange>>;
}

pub struct PgManualStore {
    pool: PgPool,
}

impl PgManualStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
pub(crate) struct ManualRow {
    status: String,
    note: Option<String>,
    set_at: DateTime<Utc>,
    set_by: Option<Uuid>,
}

impl ManualRow {
    pub(crate) fn into_state(self) -> Result<ManualState> {
        let status = ManualStatus::from_db_str(&self.status).ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "manual_monitors: unknown status {}",
                self.status
            ))
        })?;
        Ok(ManualState {
            status,
            note: self.note,
            set_at: self.set_at,
            set_by: self.set_by.map(UserId),
        })
    }
}

#[async_trait]
impl ManualStore for PgManualStore {
    async fn get(&self, org: OrgId, target_id: Uuid) -> Result<Option<ManualState>> {
        let row: Option<ManualRow> = sqlx::query_as(
            "SELECT status, note, set_at, set_by FROM manual_monitors \
             WHERE org_id = $1 AND target_id = $2",
        )
        .bind(org.0)
        .bind(target_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(ManualRow::into_state).transpose()
    }

    async fn set(
        &self,
        org: OrgId,
        target_id: Uuid,
        status: ManualStatus,
        note: Option<String>,
        actor: Option<UserId>,
    ) -> Result<Option<ManualChange>> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Locking the target serialises sets on it, so `set_at` taken under the
        // lock orders them as they committed, and holds off a delete.
        let created: Option<(DateTime<Utc>,)> = sqlx::query_as(
            "SELECT t.created_at FROM targets t \
             JOIN organizations o ON o.id = t.org_id AND o.deleted_at IS NULL \
             WHERE t.id = $2 AND t.org_id = $1 AND t.kind = 'manual' \
             FOR NO KEY UPDATE OF t",
        )
        .bind(org.0)
        .bind(target_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some((created_at,)) = created else {
            return Ok(None);
        };
        let stored: Option<ManualRow> = sqlx::query_as(
            "SELECT status, note, set_at, set_by FROM manual_monitors \
             WHERE org_id = $1 AND target_id = $2",
        )
        .bind(org.0)
        .bind(target_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let prev = match stored {
            Some(row) => row.into_state()?,
            None => ManualState::initial(created_at),
        };
        if prev.matches(status, &note) {
            return Ok(Some(ManualChange {
                state: prev.clone(),
                prev,
            }));
        }
        // Never at or before the state it replaces, so a clock stepping back
        // cannot make every node keep the superseded one.
        let row: ManualRow = sqlx::query_as(
            "INSERT INTO manual_monitors (target_id, org_id, status, note, set_at, set_by) \
             VALUES ($1, $2, $3, $4, GREATEST(clock_timestamp(), $5 + interval '1 microsecond'), $6) \
             ON CONFLICT (target_id) DO UPDATE \
                 SET status = EXCLUDED.status, note = EXCLUDED.note, set_at = EXCLUDED.set_at, \
                     set_by = EXCLUDED.set_by \
             RETURNING status, note, set_at, set_by",
        )
        .bind(target_id)
        .bind(org.0)
        .bind(status.as_str())
        .bind(note.as_deref())
        .bind(prev.set_at)
        .bind(actor.map(|u| u.0))
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let state = row.into_state()?;
        // The open incident's cause follows the operator's latest word; the
        // writer only stamps it when the incident opens.
        if let Some(cause) = state.error() {
            sqlx::query(
                "UPDATE incidents SET error_sample = $3, updated_at = now() \
                 WHERE org_id = $1 AND target_id = $2 AND origin = 'monitor' \
                   AND ended_at IS NULL",
            )
            .bind(org.0)
            .bind(target_id)
            .bind(cause)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            actor,
            "target.state_set",
            serde_json::json!({
                "target_id": target_id,
                "from": prev.status.as_str(),
                "to": status.as_str(),
                "note": note,
            }),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(Some(ManualChange { prev, state }))
    }
}

fn db_err(e: sqlx::Error) -> AppError {
    AppError::Other(anyhow::anyhow!("manual_monitors: {e}"))
}

/// No-DB harnesses. There is no targets table behind it, so every target
/// counts as a manual monitor created at the epoch.
#[derive(Default)]
pub struct InMemoryManualStore {
    inner: std::sync::Mutex<HashMap<(OrgId, Uuid), ManualState>>,
}

impl InMemoryManualStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ManualStore for InMemoryManualStore {
    async fn get(&self, org: OrgId, target_id: Uuid) -> Result<Option<ManualState>> {
        Ok(self.inner.lock().unwrap().get(&(org, target_id)).cloned())
    }

    async fn set(
        &self,
        org: OrgId,
        target_id: Uuid,
        status: ManualStatus,
        note: Option<String>,
        actor: Option<UserId>,
    ) -> Result<Option<ManualChange>> {
        let mut st = self.inner.lock().unwrap();
        let prev = st
            .get(&(org, target_id))
            .cloned()
            .unwrap_or_else(|| ManualState::initial(DateTime::UNIX_EPOCH));
        if prev.matches(status, &note) {
            return Ok(Some(ManualChange {
                state: prev.clone(),
                prev,
            }));
        }
        let state = ManualState {
            status,
            note,
            set_at: Utc::now().max(prev.set_at + chrono::Duration::microseconds(1)),
            set_by: actor,
        };
        st.insert((org, target_id), state.clone());
        Ok(Some(ManualChange { prev, state }))
    }
}
