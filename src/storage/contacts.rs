//! Per-user contact channels: which org notification channels page a member
//! when a `user`/`schedule` escalation target resolves to them.
//!
//! Org-scoped like every store. A member edits only their own set; the engine
//! reads a resolved on-call user's set at page time to turn "page Alice" into
//! concrete channel deliveries. Channels are validated to belong to `org` on
//! write (the org-match trigger guards it too; the friendly error is here).

use async_trait::async_trait;
use parking_lot::Mutex;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{OrgId, UserId};
use crate::error::codes;
use crate::error::{AppError, Result};

#[async_trait]
pub trait ContactStore: Send + Sync {
    /// The channel ids that page this user, in stable insertion order.
    async fn for_user(&self, org: OrgId, user: UserId) -> Result<Vec<Uuid>>;
    /// Replace a user's contact channels wholesale. Every id must be a channel
    /// in `org`; an unknown id yields `CONTACT_CHANNEL_INVALID`.
    async fn replace_for_user(&self, org: OrgId, user: UserId, channels: Vec<Uuid>) -> Result<()>;
}

fn invalid_channel() -> AppError {
    AppError::unprocessable(
        codes::CONTACT_CHANNEL_INVALID,
        "contact channel references an unknown notification channel",
    )
}

// ── Postgres impl ────────────────────────────────────────────────────────

pub struct PgContactStore {
    pool: PgPool,
}

impl PgContactStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ContactStore for PgContactStore {
    async fn for_user(&self, org: OrgId, user: UserId) -> Result<Vec<Uuid>> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT channel_id FROM user_contact_channels \
             WHERE org_id = $1 AND user_id = $2 ORDER BY created_at, channel_id",
        )
        .bind(org.0)
        .bind(user.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("contact for_user: {e}")))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn replace_for_user(&self, org: OrgId, user: UserId, channels: Vec<Uuid>) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        sqlx::query("DELETE FROM user_contact_channels WHERE org_id = $1 AND user_id = $2")
            .bind(org.0)
            .bind(user.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("clear contacts: {e}")))?;
        for cid in channels {
            let owned: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM notification_channels WHERE id = $1 AND org_id = $2",
            )
            .bind(cid)
            .bind(org.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("validate channel: {e}")))?;
            if owned.is_none() {
                return Err(invalid_channel());
            }
            sqlx::query(
                "INSERT INTO user_contact_channels (org_id, user_id, channel_id) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(org.0)
            .bind(user.0)
            .bind(cid)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("insert contact: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        Ok(())
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────

#[derive(Default)]
pub struct InMemoryContactStore {
    inner: Mutex<Vec<(OrgId, UserId, Uuid)>>,
    /// Known channels per org; an empty set means "don't validate" so simple
    /// engine tests can seed contacts without standing up a channel store.
    channels: Mutex<Vec<(OrgId, Uuid)>>,
}

impl InMemoryContactStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test seam: register a channel so `replace_for_user` validation passes.
    pub fn add_channel(&self, org: OrgId, channel: Uuid) {
        self.channels.lock().push((org, channel));
    }
}

#[async_trait]
impl ContactStore for InMemoryContactStore {
    async fn for_user(&self, org: OrgId, user: UserId) -> Result<Vec<Uuid>> {
        Ok(self
            .inner
            .lock()
            .iter()
            .filter(|(o, u, _)| *o == org && *u == user)
            .map(|(_, _, c)| *c)
            .collect())
    }

    async fn replace_for_user(&self, org: OrgId, user: UserId, channels: Vec<Uuid>) -> Result<()> {
        let known = self.channels.lock();
        let validate = known.iter().any(|(o, _)| *o == org);
        if validate {
            for cid in &channels {
                if !known.iter().any(|(o, c)| *o == org && c == cid) {
                    return Err(invalid_channel());
                }
            }
        }
        drop(known);
        let mut g = self.inner.lock();
        g.retain(|(o, u, _)| !(*o == org && *u == user));
        for cid in channels {
            g.push((org, user, cid));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org() -> OrgId {
        OrgId(Uuid::from_u128(1))
    }
    fn user() -> UserId {
        UserId(Uuid::from_u128(2))
    }

    #[tokio::test]
    async fn replace_then_read_roundtrips() {
        let store = InMemoryContactStore::new();
        let c1 = Uuid::now_v7();
        let c2 = Uuid::now_v7();
        store.add_channel(org(), c1);
        store.add_channel(org(), c2);
        store
            .replace_for_user(org(), user(), vec![c1, c2])
            .await
            .unwrap();
        assert_eq!(store.for_user(org(), user()).await.unwrap().len(), 2);
        store
            .replace_for_user(org(), user(), vec![c1])
            .await
            .unwrap();
        assert_eq!(store.for_user(org(), user()).await.unwrap(), vec![c1]);
    }

    #[tokio::test]
    async fn rejects_unknown_channel() {
        let store = InMemoryContactStore::new();
        store.add_channel(org(), Uuid::now_v7());
        let err = store
            .replace_for_user(org(), user(), vec![Uuid::now_v7()])
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Unprocessable { .. }));
    }
}
