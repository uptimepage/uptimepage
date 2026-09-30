//! Storage for `channel_link_codes` — single-use codes that attach exactly
//! one notification channel to an org from outside the dashboard: the
//! `telegram` purpose binds a chat through the central bot, the `delegate`
//! purpose powers the public `/c/<code>` connect page.
//!
//! Mint is org-scoped and capped per purpose. Consume is deliberately *not*
//! org-scoped: the consuming surface has no tenant context — the code row
//! IS the org authority — and the guarded `UPDATE … RETURNING` makes it
//! race-safe single-use.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{OrgId, UserId};
use crate::error::{AppError, Result};
use crate::storage::locks::{advisory_xact_lock, org_lock_key};

/// Closed list mirrored by the `purpose` CHECK in migration 024; the
/// enum-drift test ties the two together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPurpose {
    Telegram,
    Delegate,
    Whatsapp,
}

impl LinkPurpose {
    pub const ALL: &'static [Self] = &[Self::Telegram, Self::Delegate, Self::Whatsapp];

    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Delegate => "delegate",
            Self::Whatsapp => "whatsapp",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LinkCode {
    pub id: Uuid,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum MintOutcome {
    Created(LinkCode),
    /// The org already holds the maximum of outstanding unconsumed codes
    /// for this purpose.
    LimitReached,
}

/// What the status poll reports. A consumed row whose channel was deleted (or
/// whose channel create failed) reads as `Expired`: the code can never link
/// again, so the form should mint a fresh one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkCodeStatus {
    Pending,
    Consumed { channel_id: Uuid },
    Expired,
}

/// The org authority recovered from a consumed code — the only place an
/// unauthenticated consume surface may take a tenant identity from.
#[derive(Debug, Clone)]
pub struct ConsumedLink {
    pub id: Uuid,
    pub org_id: OrgId,
    pub channel_name: Option<String>,
    pub kind_hint: Option<String>,
}

impl ConsumedLink {
    /// Whether the link may make a channel of `kind` (db string): any kind
    /// when unpinned, else only its pinned one.
    pub fn accepts(&self, kind: &str) -> bool {
        self.kind_hint.as_deref().is_none_or(|h| h == kind)
    }
}

/// A live (or spent) delegate link as listed on the channels page.
#[derive(Debug, Clone)]
pub struct DelegateRow {
    pub id: Uuid,
    pub channel_name: Option<String>,
    pub kind_hint: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: LinkCodeStatus,
}

#[async_trait]
pub trait ChannelLinkCodeStore: Send + Sync {
    /// Capped at `max_outstanding` live codes per org and purpose,
    /// race-safe under the per-org advisory lock.
    #[allow(clippy::too_many_arguments)]
    async fn mint(
        &self,
        org: OrgId,
        purpose: LinkPurpose,
        created_by: Option<UserId>,
        code_hash: &str,
        channel_name: Option<&str>,
        kind_hint: Option<&str>,
        expires_at: DateTime<Utc>,
        max_outstanding: i64,
    ) -> Result<MintOutcome>;

    /// `None` = no such code in this org.
    async fn status(&self, org: OrgId, id: Uuid) -> Result<Option<LinkCodeStatus>>;

    /// Atomically claim a live code of `purpose` by hash; `None` = unknown,
    /// expired, revoked, or already claimed.
    async fn consume(&self, purpose: LinkPurpose, code_hash: &str) -> Result<Option<ConsumedLink>>;

    /// Un-claim a consumed code whose channel create failed, so the link
    /// survives a transient error (quota, validation) instead of burning.
    async fn restore(&self, id: Uuid) -> Result<()>;

    /// Record the channel a consume produced; flips the poll to `Consumed`.
    async fn attach_channel(&self, id: Uuid, channel_id: Uuid) -> Result<()>;

    /// Atomically claim a live code by row id — the OAuth-callback variant
    /// of [`ChannelLinkCodeStore::consume`], where only the id travelled
    /// through the dance.
    async fn consume_by_id(&self, id: Uuid) -> Result<Option<ConsumedLink>>;

    /// Non-mutating lookup of a live code, for rendering the public connect
    /// page; `None` = unknown, expired, revoked, or consumed.
    async fn peek(&self, purpose: LinkPurpose, code_hash: &str) -> Result<Option<ConsumedLink>>;

    /// Poll by code hash for the public connect page — possession of the
    /// code is the authority, so no org scoping. `None` = unknown code.
    async fn status_by_hash(
        &self,
        purpose: LinkPurpose,
        code_hash: &str,
    ) -> Result<Option<LinkCodeStatus>>;

    /// Delegate links of the org, newest first.
    async fn list_delegates(&self, org: OrgId) -> Result<Vec<DelegateRow>>;

    /// Revoke an unconsumed delegate link; `false` = unknown, foreign, or
    /// already consumed/revoked.
    async fn revoke(&self, org: OrgId, id: Uuid) -> Result<bool>;
}

// ── Postgres impl ────────────────────────────────────────────────────────

pub struct PgChannelLinkCodeStore {
    pool: PgPool,
}

impl PgChannelLinkCodeStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ChannelLinkCodeStore for PgChannelLinkCodeStore {
    async fn mint(
        &self,
        org: OrgId,
        purpose: LinkPurpose,
        created_by: Option<UserId>,
        code_hash: &str,
        channel_name: Option<&str>,
        kind_hint: Option<&str>,
        expires_at: DateTime<Utc>,
        max_outstanding: i64,
    ) -> Result<MintOutcome> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        // Serialises the count-subquery + INSERT cap check, like every
        // other org-cap writer.
        advisory_xact_lock(&mut *tx, &org_lock_key(org))
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("advisory lock: {e}")))?;
        // Opportunistic purge of dead codes instead of a janitor.
        sqlx::query(
            r#"DELETE FROM channel_link_codes
               WHERE org_id = $1 AND expires_at <= now() AND channel_id IS NULL"#,
        )
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("purge channel link codes: {e}")))?;
        let row: Option<(Uuid, DateTime<Utc>)> = sqlx::query_as(
            r#"INSERT INTO channel_link_codes
                   (org_id, purpose, created_by, code_hash, channel_name, kind_hint, expires_at)
               SELECT $1, $2, $3, $4, $5, $6, $7
               WHERE (SELECT count(*) FROM channel_link_codes
                      WHERE org_id = $1 AND purpose = $2
                        AND consumed_at IS NULL AND revoked_at IS NULL
                        AND expires_at > now()) < $8
               RETURNING id, expires_at"#,
        )
        .bind(org.0)
        .bind(purpose.as_db_str())
        .bind(created_by.map(|u| u.0))
        .bind(code_hash)
        .bind(channel_name)
        .bind(kind_hint)
        .bind(expires_at)
        .bind(max_outstanding)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("insert channel link code: {e}")))?;
        let Some((id, expires_at)) = row else {
            tx.rollback().await.ok();
            return Ok(MintOutcome::LimitReached);
        };
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        Ok(MintOutcome::Created(LinkCode { id, expires_at }))
    }

    async fn status(&self, org: OrgId, id: Uuid) -> Result<Option<LinkCodeStatus>> {
        let row: Option<(
            Option<DateTime<Utc>>,
            DateTime<Utc>,
            Option<Uuid>,
            Option<DateTime<Utc>>,
        )> = sqlx::query_as(
            r#"SELECT consumed_at, expires_at, channel_id, revoked_at
               FROM channel_link_codes WHERE id = $1 AND org_id = $2"#,
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("channel link code status: {e}")))?;
        Ok(
            row.map(|(consumed_at, expires_at, channel_id, revoked_at)| {
                link_status(consumed_at, expires_at, channel_id, revoked_at, Utc::now())
            }),
        )
    }

    async fn consume(&self, purpose: LinkPurpose, code_hash: &str) -> Result<Option<ConsumedLink>> {
        let row: Option<(Uuid, Uuid, Option<String>, Option<String>)> = sqlx::query_as(
            r#"UPDATE channel_link_codes
               SET consumed_at = now()
               WHERE code_hash = $1 AND purpose = $2
                 AND consumed_at IS NULL AND revoked_at IS NULL AND expires_at > now()
               RETURNING id, org_id, channel_name, kind_hint"#,
        )
        .bind(code_hash)
        .bind(purpose.as_db_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("consume channel link code: {e}")))?;
        Ok(
            row.map(|(id, org_id, channel_name, kind_hint)| ConsumedLink {
                id,
                org_id: OrgId(org_id),
                channel_name,
                kind_hint,
            }),
        )
    }

    async fn restore(&self, id: Uuid) -> Result<()> {
        sqlx::query(
            r#"UPDATE channel_link_codes SET consumed_at = NULL
               WHERE id = $1 AND channel_id IS NULL"#,
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("restore channel link code: {e}")))?;
        Ok(())
    }

    async fn consume_by_id(&self, id: Uuid) -> Result<Option<ConsumedLink>> {
        let row: Option<(Uuid, Uuid, Option<String>, Option<String>)> = sqlx::query_as(
            r#"UPDATE channel_link_codes
               SET consumed_at = now()
               WHERE id = $1
                 AND consumed_at IS NULL AND revoked_at IS NULL AND expires_at > now()
               RETURNING id, org_id, channel_name, kind_hint"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("consume channel link code by id: {e}")))?;
        Ok(
            row.map(|(id, org_id, channel_name, kind_hint)| ConsumedLink {
                id,
                org_id: OrgId(org_id),
                channel_name,
                kind_hint,
            }),
        )
    }

    async fn attach_channel(&self, id: Uuid, channel_id: Uuid) -> Result<()> {
        sqlx::query(r#"UPDATE channel_link_codes SET channel_id = $2 WHERE id = $1"#)
            .bind(id)
            .bind(channel_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("attach channel link: {e}")))?;
        Ok(())
    }

    async fn peek(&self, purpose: LinkPurpose, code_hash: &str) -> Result<Option<ConsumedLink>> {
        let row: Option<(Uuid, Uuid, Option<String>, Option<String>)> = sqlx::query_as(
            r#"SELECT id, org_id, channel_name, kind_hint
               FROM channel_link_codes
               WHERE code_hash = $1 AND purpose = $2
                 AND consumed_at IS NULL AND revoked_at IS NULL AND expires_at > now()"#,
        )
        .bind(code_hash)
        .bind(purpose.as_db_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("peek channel link code: {e}")))?;
        Ok(
            row.map(|(id, org_id, channel_name, kind_hint)| ConsumedLink {
                id,
                org_id: OrgId(org_id),
                channel_name,
                kind_hint,
            }),
        )
    }

    async fn status_by_hash(
        &self,
        purpose: LinkPurpose,
        code_hash: &str,
    ) -> Result<Option<LinkCodeStatus>> {
        let row: Option<(
            Option<DateTime<Utc>>,
            DateTime<Utc>,
            Option<Uuid>,
            Option<DateTime<Utc>>,
        )> = sqlx::query_as(
            r#"SELECT consumed_at, expires_at, channel_id, revoked_at
               FROM channel_link_codes WHERE code_hash = $1 AND purpose = $2"#,
        )
        .bind(code_hash)
        .bind(purpose.as_db_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("channel link code status by hash: {e}")))?;
        Ok(
            row.map(|(consumed_at, expires_at, channel_id, revoked_at)| {
                link_status(consumed_at, expires_at, channel_id, revoked_at, Utc::now())
            }),
        )
    }

    async fn list_delegates(&self, org: OrgId) -> Result<Vec<DelegateRow>> {
        type Row = (
            Uuid,
            Option<String>,
            Option<String>,
            DateTime<Utc>,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
            Option<Uuid>,
            Option<DateTime<Utc>>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            r#"SELECT id, channel_name, kind_hint, created_at, expires_at,
                      consumed_at, channel_id, revoked_at
               FROM channel_link_codes
               WHERE org_id = $1 AND purpose = 'delegate'
               ORDER BY created_at DESC"#,
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("list delegate links: {e}")))?;
        let now = Utc::now();
        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    channel_name,
                    kind_hint,
                    created_at,
                    expires_at,
                    consumed_at,
                    channel_id,
                    revoked_at,
                )| {
                    DelegateRow {
                        id,
                        channel_name,
                        kind_hint,
                        created_at,
                        expires_at,
                        status: link_status(consumed_at, expires_at, channel_id, revoked_at, now),
                    }
                },
            )
            .collect())
    }

    async fn revoke(&self, org: OrgId, id: Uuid) -> Result<bool> {
        let n = sqlx::query(
            r#"UPDATE channel_link_codes SET revoked_at = now()
               WHERE id = $1 AND org_id = $2 AND purpose = 'delegate'
                 AND consumed_at IS NULL AND revoked_at IS NULL"#,
        )
        .bind(id)
        .bind(org.0)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("revoke delegate link: {e}")))?
        .rows_affected();
        Ok(n == 1)
    }
}

/// Shared by the Pg and in-memory stores so they can't drift. A revoked
/// link reads as `Expired`: dead either way, no enumeration signal.
fn link_status(
    consumed_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
    channel_id: Option<Uuid>,
    revoked_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> LinkCodeStatus {
    match (consumed_at, channel_id) {
        (Some(_), Some(channel_id)) => LinkCodeStatus::Consumed { channel_id },
        // Claimed but no channel materialised: dead either way.
        (Some(_), None) => LinkCodeStatus::Expired,
        (None, _) if revoked_at.is_some() || expires_at <= now => LinkCodeStatus::Expired,
        (None, _) => LinkCodeStatus::Pending,
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────

#[derive(Debug, Clone)]
struct MemRow {
    id: Uuid,
    org: OrgId,
    purpose: LinkPurpose,
    code_hash: String,
    channel_name: Option<String>,
    kind_hint: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    channel_id: Option<Uuid>,
}

impl MemRow {
    fn live(&self, now: DateTime<Utc>) -> bool {
        self.consumed_at.is_none() && self.revoked_at.is_none() && self.expires_at > now
    }
}

#[derive(Default)]
pub struct InMemoryChannelLinkCodeStore {
    inner: Mutex<Vec<MemRow>>,
}

impl InMemoryChannelLinkCodeStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ChannelLinkCodeStore for InMemoryChannelLinkCodeStore {
    async fn mint(
        &self,
        org: OrgId,
        purpose: LinkPurpose,
        _created_by: Option<UserId>,
        code_hash: &str,
        channel_name: Option<&str>,
        kind_hint: Option<&str>,
        expires_at: DateTime<Utc>,
        max_outstanding: i64,
    ) -> Result<MintOutcome> {
        let mut g = self.inner.lock();
        let now = Utc::now();
        g.retain(|r| !(r.org == org && r.expires_at <= now && r.channel_id.is_none()));
        let outstanding = g
            .iter()
            .filter(|r| r.org == org && r.purpose == purpose && r.live(now))
            .count() as i64;
        if outstanding >= max_outstanding {
            return Ok(MintOutcome::LimitReached);
        }
        let row = MemRow {
            id: Uuid::now_v7(),
            org,
            purpose,
            code_hash: code_hash.to_string(),
            channel_name: channel_name.map(str::to_string),
            kind_hint: kind_hint.map(str::to_string),
            created_at: now,
            expires_at,
            consumed_at: None,
            revoked_at: None,
            channel_id: None,
        };
        let code = LinkCode {
            id: row.id,
            expires_at: row.expires_at,
        };
        g.push(row);
        Ok(MintOutcome::Created(code))
    }

    async fn status(&self, org: OrgId, id: Uuid) -> Result<Option<LinkCodeStatus>> {
        Ok(self
            .inner
            .lock()
            .iter()
            .find(|r| r.org == org && r.id == id)
            .map(|r| {
                link_status(
                    r.consumed_at,
                    r.expires_at,
                    r.channel_id,
                    r.revoked_at,
                    Utc::now(),
                )
            }))
    }

    async fn consume(&self, purpose: LinkPurpose, code_hash: &str) -> Result<Option<ConsumedLink>> {
        let mut g = self.inner.lock();
        let now = Utc::now();
        let Some(row) = g
            .iter_mut()
            .find(|r| r.code_hash == code_hash && r.purpose == purpose && r.live(now))
        else {
            return Ok(None);
        };
        row.consumed_at = Some(now);
        Ok(Some(ConsumedLink {
            id: row.id,
            org_id: row.org,
            channel_name: row.channel_name.clone(),
            kind_hint: row.kind_hint.clone(),
        }))
    }

    async fn restore(&self, id: Uuid) -> Result<()> {
        if let Some(row) = self
            .inner
            .lock()
            .iter_mut()
            .find(|r| r.id == id && r.channel_id.is_none())
        {
            row.consumed_at = None;
        }
        Ok(())
    }

    async fn consume_by_id(&self, id: Uuid) -> Result<Option<ConsumedLink>> {
        let mut g = self.inner.lock();
        let now = Utc::now();
        let Some(row) = g.iter_mut().find(|r| r.id == id && r.live(now)) else {
            return Ok(None);
        };
        row.consumed_at = Some(now);
        Ok(Some(ConsumedLink {
            id: row.id,
            org_id: row.org,
            channel_name: row.channel_name.clone(),
            kind_hint: row.kind_hint.clone(),
        }))
    }

    async fn attach_channel(&self, id: Uuid, channel_id: Uuid) -> Result<()> {
        if let Some(row) = self.inner.lock().iter_mut().find(|r| r.id == id) {
            row.channel_id = Some(channel_id);
        }
        Ok(())
    }

    async fn peek(&self, purpose: LinkPurpose, code_hash: &str) -> Result<Option<ConsumedLink>> {
        let now = Utc::now();
        Ok(self
            .inner
            .lock()
            .iter()
            .find(|r| r.code_hash == code_hash && r.purpose == purpose && r.live(now))
            .map(|r| ConsumedLink {
                id: r.id,
                org_id: r.org,
                channel_name: r.channel_name.clone(),
                kind_hint: r.kind_hint.clone(),
            }))
    }

    async fn status_by_hash(
        &self,
        purpose: LinkPurpose,
        code_hash: &str,
    ) -> Result<Option<LinkCodeStatus>> {
        let now = Utc::now();
        Ok(self
            .inner
            .lock()
            .iter()
            .find(|r| r.code_hash == code_hash && r.purpose == purpose)
            .map(|r| link_status(r.consumed_at, r.expires_at, r.channel_id, r.revoked_at, now)))
    }

    async fn list_delegates(&self, org: OrgId) -> Result<Vec<DelegateRow>> {
        let now = Utc::now();
        let mut rows: Vec<DelegateRow> = self
            .inner
            .lock()
            .iter()
            .filter(|r| r.org == org && r.purpose == LinkPurpose::Delegate)
            .map(|r| DelegateRow {
                id: r.id,
                channel_name: r.channel_name.clone(),
                kind_hint: r.kind_hint.clone(),
                created_at: r.created_at,
                expires_at: r.expires_at,
                status: link_status(r.consumed_at, r.expires_at, r.channel_id, r.revoked_at, now),
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        Ok(rows)
    }

    async fn revoke(&self, org: OrgId, id: Uuid) -> Result<bool> {
        let mut g = self.inner.lock();
        let Some(row) = g.iter_mut().find(|r| {
            r.id == id
                && r.org == org
                && r.purpose == LinkPurpose::Delegate
                && r.consumed_at.is_none()
                && r.revoked_at.is_none()
        }) else {
            return Ok(false);
        };
        row.revoked_at = Some(Utc::now());
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn org() -> OrgId {
        OrgId(Uuid::from_u128(0xA1))
    }

    fn other_org() -> OrgId {
        OrgId(Uuid::from_u128(0xB2))
    }

    fn in_15_min() -> DateTime<Utc> {
        Utc::now() + Duration::minutes(15)
    }

    async fn mint(
        store: &InMemoryChannelLinkCodeStore,
        org: OrgId,
        purpose: LinkPurpose,
        hash: &str,
    ) -> LinkCode {
        match store
            .mint(org, purpose, None, hash, None, None, in_15_min(), 5)
            .await
            .unwrap()
        {
            MintOutcome::Created(c) => c,
            MintOutcome::LimitReached => panic!("unexpected limit"),
        }
    }

    #[tokio::test]
    async fn consume_is_single_use_and_purpose_scoped() {
        let store = InMemoryChannelLinkCodeStore::new();
        let code = mint(&store, org(), LinkPurpose::Telegram, "h1").await;

        // A delegate consume cannot claim a telegram code.
        assert!(
            store
                .consume(LinkPurpose::Delegate, "h1")
                .await
                .unwrap()
                .is_none()
        );
        let first = store
            .consume(LinkPurpose::Telegram, "h1")
            .await
            .unwrap()
            .expect("first claim");
        assert_eq!(first.org_id, org());
        assert_eq!(first.id, code.id);
        assert!(
            store
                .consume(LinkPurpose::Telegram, "h1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn expired_code_does_not_consume() {
        let store = InMemoryChannelLinkCodeStore::new();
        store
            .mint(
                org(),
                LinkPurpose::Telegram,
                None,
                "h1",
                None,
                None,
                Utc::now() - Duration::minutes(1),
                5,
            )
            .await
            .unwrap();
        assert!(
            store
                .consume(LinkPurpose::Telegram, "h1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn status_transitions_and_org_scoping() {
        let store = InMemoryChannelLinkCodeStore::new();
        let code = mint(&store, org(), LinkPurpose::Telegram, "h1").await;

        assert_eq!(
            store.status(org(), code.id).await.unwrap(),
            Some(LinkCodeStatus::Pending)
        );
        // Another org cannot observe the code at all.
        assert_eq!(store.status(other_org(), code.id).await.unwrap(), None);

        let consumed = store
            .consume(LinkPurpose::Telegram, "h1")
            .await
            .unwrap()
            .unwrap();
        // Claimed but the channel never materialised → dead, not pending.
        assert_eq!(
            store.status(org(), code.id).await.unwrap(),
            Some(LinkCodeStatus::Expired)
        );

        let channel_id = Uuid::now_v7();
        store.attach_channel(consumed.id, channel_id).await.unwrap();
        assert_eq!(
            store.status(org(), code.id).await.unwrap(),
            Some(LinkCodeStatus::Consumed { channel_id })
        );
    }

    #[tokio::test]
    async fn mint_caps_outstanding_codes_per_org_and_purpose() {
        let store = InMemoryChannelLinkCodeStore::new();
        for i in 0..5 {
            mint(&store, org(), LinkPurpose::Telegram, &format!("h{i}")).await;
        }
        assert!(matches!(
            store
                .mint(
                    org(),
                    LinkPurpose::Telegram,
                    None,
                    "h-over",
                    None,
                    None,
                    in_15_min(),
                    5
                )
                .await
                .unwrap(),
            MintOutcome::LimitReached
        ));
        // The cap is per org AND per purpose; consuming a code frees a slot.
        mint(&store, other_org(), LinkPurpose::Telegram, "h-other").await;
        mint(&store, org(), LinkPurpose::Delegate, "h-delegate").await;
        store
            .consume(LinkPurpose::Telegram, "h0")
            .await
            .unwrap()
            .unwrap();
        mint(&store, org(), LinkPurpose::Telegram, "h-freed").await;
    }

    #[tokio::test]
    async fn revoked_delegate_neither_peeks_nor_consumes() {
        let store = InMemoryChannelLinkCodeStore::new();
        let code = mint(&store, org(), LinkPurpose::Delegate, "d1").await;

        assert!(
            store
                .peek(LinkPurpose::Delegate, "d1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(store.revoke(org(), code.id).await.unwrap());
        assert!(
            store
                .peek(LinkPurpose::Delegate, "d1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .consume(LinkPurpose::Delegate, "d1")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.status(org(), code.id).await.unwrap(),
            Some(LinkCodeStatus::Expired)
        );
        // Revoke is single-shot and consumed links can't be revoked.
        assert!(!store.revoke(org(), code.id).await.unwrap());
    }

    #[tokio::test]
    async fn restore_unclaims_a_failed_create() {
        let store = InMemoryChannelLinkCodeStore::new();
        let code = mint(&store, org(), LinkPurpose::Delegate, "d1").await;

        let consumed = store
            .consume(LinkPurpose::Delegate, "d1")
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .consume(LinkPurpose::Delegate, "d1")
                .await
                .unwrap()
                .is_none()
        );
        store.restore(consumed.id).await.unwrap();
        assert_eq!(
            store.status(org(), code.id).await.unwrap(),
            Some(LinkCodeStatus::Pending)
        );
        // A link that already produced a channel cannot be restored.
        let again = store
            .consume(LinkPurpose::Delegate, "d1")
            .await
            .unwrap()
            .unwrap();
        store
            .attach_channel(again.id, Uuid::now_v7())
            .await
            .unwrap();
        store.restore(again.id).await.unwrap();
        assert!(matches!(
            store.status(org(), code.id).await.unwrap(),
            Some(LinkCodeStatus::Consumed { .. })
        ));
    }

    #[tokio::test]
    async fn delegate_listing_is_org_scoped_and_newest_first() {
        let store = InMemoryChannelLinkCodeStore::new();
        mint(&store, org(), LinkPurpose::Delegate, "d1").await;
        mint(&store, org(), LinkPurpose::Delegate, "d2").await;
        mint(&store, org(), LinkPurpose::Telegram, "t1").await;
        mint(&store, other_org(), LinkPurpose::Delegate, "d3").await;

        let rows = store.list_delegates(org()).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].created_at >= rows[1].created_at);
    }

    #[test]
    fn unconsumed_past_expiry_or_revoked_reads_expired() {
        let now = Utc::now();
        assert_eq!(
            link_status(None, now - Duration::seconds(1), None, None, now),
            LinkCodeStatus::Expired
        );
        assert_eq!(
            link_status(None, now + Duration::seconds(1), None, Some(now), now),
            LinkCodeStatus::Expired
        );
        assert_eq!(
            link_status(None, now + Duration::seconds(1), None, None, now),
            LinkCodeStatus::Pending
        );
    }
}
