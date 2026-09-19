//! Storage for `monitor_shares` — per-monitor capability links (`/m/{token}`).
//!
//! The raw token is a 256-bit URL-safe random; its SHA-256 hex is the lookup key
//! (same discipline as `sessions.id_hash`), and a reversible encrypted copy lets
//! the owner re-copy the link. Create takes the per-org advisory lock to enforce
//! the per-monitor and per-org share caps race-safely (mirrors `status_pages`).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    CreatedShare, MonitorShare, MonitorShareId, NewMonitorShare, OrgId, ResolvedShare,
    SharePageUse, UserId,
};
use crate::error::{AppError, Result};
use crate::security::Cipher;
use crate::security::token_hash::generate_raw_token;
use crate::storage::accounts;
use crate::storage::capability_token;
use crate::storage::locks::{account_lock_key, advisory_xact_lock};

/// Result of [`MonitorShareStore::create`]. The store stays free of HTTP/plan
/// concerns; the handler maps each limit outcome to the quota error with the
/// real plan id.
#[derive(Debug)]
pub enum CreateShareOutcome {
    Created(CreatedShare),
    /// Target absent from the org → 404.
    TargetNotFound,
    /// This monitor already holds `max_share_links_per_monitor` active links.
    PerMonitorLimit,
    /// This would be a new shared monitor and the org is at `max_shared_monitors`.
    OrgMonitorLimit,
}

#[async_trait]
pub trait MonitorShareStore: Send + Sync {
    /// Mint a share for a monitor, enforcing the per-monitor link cap and the
    /// account-wide shared-monitor cap (both from the account's plan). `None` for either
    /// cap exempts it — the status-page detail link mints that way, since the
    /// page already publishes the monitor and its own component cap applies.
    /// See [`CreateShareOutcome`].
    async fn create(
        &self,
        org: OrgId,
        target_id: Uuid,
        new: NewMonitorShare,
        created_by: Option<UserId>,
        max_links_per_monitor: Option<i64>,
        max_shared_monitors: Option<i64>,
    ) -> Result<CreateShareOutcome>;
    /// Non-revoked shares for one monitor, newest first.
    async fn list_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Vec<MonitorShare>>;
    /// Count of non-revoked shares for one monitor. Cheaper than
    /// [`list_for_target`](Self::list_for_target) for the header "shared" chip —
    /// no row fetch, no token decrypt.
    async fn count_active_for_target(&self, org: OrgId, target_id: Uuid) -> Result<i64>;
    /// Revoke a share belonging to `(org, target_id)`. `false` = not found under
    /// that monitor in this org, or already revoked. Scoping to `target_id`
    /// keeps the REST path honest: a share is only revocable via its own
    /// monitor's URL.
    async fn revoke(
        &self,
        org: OrgId,
        target_id: Uuid,
        id: MonitorShareId,
        actor: Option<UserId>,
    ) -> Result<bool>;
    /// Bump the view counter + `last_viewed_at` for a resolved share. Called
    /// fire-and-forget on a page view; a failure must never break the render.
    async fn record_view(&self, id: MonitorShareId) -> Result<()>;
    /// Resolve a presented raw token to its monitor + org. `None` for anything
    /// not live: unknown, revoked, expired, or org soft-deleted — the caller
    /// must not distinguish them (uniform 404, anti-enumeration). This is the
    /// one cross-tenant-by-design lookup; every read past it uses the returned
    /// `(org, target_id)`.
    async fn resolve_active(&self, raw_token: &str) -> Result<Option<ResolvedShare>>;
}

pub struct PgMonitorShareStore {
    pool: PgPool,
    cipher: Option<Arc<Cipher>>,
}

impl PgMonitorShareStore {
    pub fn new(pool: PgPool, cipher: Option<Arc<Cipher>>) -> Self {
        Self { pool, cipher }
    }
}

#[derive(sqlx::FromRow)]
struct ShareRow {
    id: Uuid,
    org_id: Uuid,
    target_id: Uuid,
    label: Option<String>,
    token_enc: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    view_count: i64,
    last_viewed_at: Option<DateTime<Utc>>,
}

impl ShareRow {
    /// `token` is populated by the caller (which holds the cipher); the row
    /// mapping leaves it `None`.
    fn into_share(self) -> MonitorShare {
        MonitorShare {
            id: MonitorShareId(self.id),
            org_id: OrgId(self.org_id),
            target_id: self.target_id,
            label: self.label,
            token: None,
            created_at: self.created_at,
            expires_at: self.expires_at,
            view_count: self.view_count,
            last_viewed_at: self.last_viewed_at,
            used_by_pages: Vec::new(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct PageUseRow {
    share_id: Uuid,
    name: String,
    slug: String,
}

const SHARE_COLUMNS: &str =
    "id, org_id, target_id, label, token_enc, created_at, expires_at, view_count, last_viewed_at";

#[async_trait]
impl MonitorShareStore for PgMonitorShareStore {
    async fn create(
        &self,
        org: OrgId,
        target_id: Uuid,
        new: NewMonitorShare,
        created_by: Option<UserId>,
        max_links_per_monitor: Option<i64>,
        max_shared_monitors: Option<i64>,
    ) -> Result<CreateShareOutcome> {
        let capability_token::Minted {
            raw,
            hash: token_hash,
            sealed: token_enc,
        } = capability_token::mint(self.cipher.as_deref())?;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // The shared-monitor cap is pooled, so the lock takes the account
        // (mirrors the status_pages create).
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(db_err)?;
        // One snapshot under the lock: org-membership of the target, this
        // monitor's active-link count, and the account's distinct
        // shared-monitor count (the second cap only bites when this monitor has
        // no links yet). Page-minted shares are excluded from both counts: the
        // operator did not spend quota on them, so they must not block the
        // links they do mint.
        let pool_orgs = accounts::live_orgs("$3");
        let (target_ok, links_here, shared_monitors): (bool, i64, i64) = sqlx::query_as(&format!(
            "SELECT EXISTS (SELECT 1 FROM targets WHERE id = $2 AND org_id = $1), \
             (SELECT count(*) FROM monitor_shares ms \
              WHERE ms.org_id = $1 AND ms.target_id = $2 AND ms.revoked_at IS NULL \
                AND NOT EXISTS (SELECT 1 FROM status_page_components spc \
                                WHERE spc.share_id = ms.id)), \
             (SELECT count(DISTINCT ms.target_id) FROM monitor_shares ms \
              WHERE ms.org_id IN ({pool_orgs}) AND ms.revoked_at IS NULL \
                AND NOT EXISTS (SELECT 1 FROM status_page_components spc \
                                WHERE spc.share_id = ms.id))"
        ))
        .bind(org.0)
        .bind(target_id)
        .bind(account.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        if !target_ok {
            tx.rollback().await.ok();
            return Ok(CreateShareOutcome::TargetNotFound);
        }
        if max_links_per_monitor.is_some_and(|cap| links_here >= cap) {
            tx.rollback().await.ok();
            return Ok(CreateShareOutcome::PerMonitorLimit);
        }
        if links_here == 0 && max_shared_monitors.is_some_and(|cap| shared_monitors >= cap) {
            tx.rollback().await.ok();
            return Ok(CreateShareOutcome::OrgMonitorLimit);
        }
        let row: ShareRow = sqlx::query_as(&format!(
            r#"INSERT INTO monitor_shares (org_id, target_id, token_hash, token_enc, label, expires_at, created_by)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               RETURNING {SHARE_COLUMNS}"#
        ))
        .bind(org.0)
        .bind(target_id)
        .bind(&token_hash)
        .bind(&token_enc)
        .bind(new.label)
        .bind(new.expires_at)
        .bind(created_by.map(|u| u.0))
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            created_by,
            "monitor_share.created",
            serde_json::json!({ "share_id": row.id, "target_id": target_id }),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        let mut share = row.into_share();
        share.token = Some(raw.clone());
        Ok(CreateShareOutcome::Created(CreatedShare {
            share,
            token: raw,
        }))
    }

    async fn list_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Vec<MonitorShare>> {
        let rows: Vec<ShareRow> = sqlx::query_as(&format!(
            "SELECT {SHARE_COLUMNS} FROM monitor_shares \
             WHERE org_id = $1 AND target_id = $2 AND revoked_at IS NULL \
             ORDER BY created_at DESC"
        ))
        .bind(org.0)
        .bind(target_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let uses: Vec<PageUseRow> = sqlx::query_as(
            "SELECT spc.share_id, sp.name, sp.slug::text AS slug \
             FROM status_page_components spc \
             JOIN status_pages sp ON sp.id = spc.status_page_id \
             WHERE spc.org_id = $1 AND spc.target_id = $2 AND spc.share_id IS NOT NULL \
               AND spc.detail_link_enabled \
             ORDER BY sp.name",
        )
        .bind(org.0)
        .bind(target_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let cipher = self.cipher.as_deref();
        Ok(rows
            .into_iter()
            .map(|r| {
                let token = capability_token::open(&r.token_enc, cipher);
                let mut share = r.into_share();
                share.used_by_pages = uses
                    .iter()
                    .filter(|u| u.share_id == share.id.0)
                    .map(|u| SharePageUse {
                        name: u.name.clone(),
                        slug: u.slug.clone(),
                    })
                    .collect();
                share.token = token;
                share
            })
            .collect())
    }

    async fn count_active_for_target(&self, org: OrgId, target_id: Uuid) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM monitor_shares \
             WHERE org_id = $1 AND target_id = $2 AND revoked_at IS NULL",
        )
        .bind(org.0)
        .bind(target_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(n)
    }

    async fn revoke(
        &self,
        org: OrgId,
        target_id: Uuid,
        id: MonitorShareId,
        actor: Option<UserId>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let revoked: Option<(Uuid,)> = sqlx::query_as(
            "UPDATE monitor_shares SET revoked_at = now() \
             WHERE id = $1 AND org_id = $2 AND target_id = $3 AND revoked_at IS NULL \
             RETURNING id",
        )
        .bind(id.0)
        .bind(org.0)
        .bind(target_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if revoked.is_some() {
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                "monitor_share.revoked",
                serde_json::json!({ "share_id": id.0, "target_id": target_id }),
            )
            .await?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(revoked.is_some())
    }

    async fn record_view(&self, id: MonitorShareId) -> Result<()> {
        sqlx::query(
            "UPDATE monitor_shares SET view_count = view_count + 1, last_viewed_at = now() \
             WHERE id = $1",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn resolve_active(&self, raw_token: &str) -> Result<Option<ResolvedShare>> {
        let token_hash = capability_token::hash(raw_token);
        // All-or-nothing: live share, target still present and covered by the
        // plan, org not soft-deleted. A held monitor drops off its status page,
        // so leaving its share link live would keep the same data reachable by
        // whoever already has the URL.
        let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            r#"SELECT ms.id, ms.target_id, ms.org_id
               FROM monitor_shares ms
               JOIN targets t       ON t.id = ms.target_id AND t.org_id = ms.org_id
               JOIN organizations o ON o.id = ms.org_id AND o.deleted_at IS NULL
               WHERE ms.token_hash = $1
                 AND t.plan_hold_at IS NULL
                 AND ms.revoked_at IS NULL
                 AND (ms.expires_at IS NULL OR ms.expires_at > now())"#,
        )
        .bind(&token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(|(share_id, target_id, org_id)| ResolvedShare {
            share_id: MonitorShareId(share_id),
            target_id,
            org: OrgId(org_id),
        }))
    }
}

fn db_err(e: sqlx::Error) -> AppError {
    AppError::Other(anyhow::anyhow!("monitor_shares: {e}"))
}

// ── In-memory store (no-DB harnesses) ─────────────────────────────────────────

struct MemShare {
    share: MonitorShare,
    token_hash: String,
    revoked: bool,
}

/// In-memory [`MonitorShareStore`] mirroring [`PgMonitorShareStore`] semantics.
/// It has no target/org tables, so `create` cannot verify the monitor belongs
/// to `org` and `resolve_active` cannot drop a soft-deleted org; DB-backed
/// tests use [`PgMonitorShareStore`] for those guards.
#[derive(Default)]
pub struct InMemoryMonitorShareStore {
    inner: std::sync::Mutex<Vec<MemShare>>,
}

impl InMemoryMonitorShareStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl MonitorShareStore for InMemoryMonitorShareStore {
    async fn create(
        &self,
        org: OrgId,
        target_id: Uuid,
        new: NewMonitorShare,
        _created_by: Option<UserId>,
        max_links_per_monitor: Option<i64>,
        max_shared_monitors: Option<i64>,
    ) -> Result<CreateShareOutcome> {
        let mut st = self.inner.lock().unwrap();
        let links_here = st
            .iter()
            .filter(|m| !m.revoked && m.share.org_id == org && m.share.target_id == target_id)
            .count() as i64;
        if max_links_per_monitor.is_some_and(|cap| links_here >= cap) {
            return Ok(CreateShareOutcome::PerMonitorLimit);
        }
        if links_here == 0 {
            let shared_monitors = st
                .iter()
                .filter(|m| !m.revoked && m.share.org_id == org)
                .map(|m| m.share.target_id)
                .collect::<std::collections::HashSet<_>>()
                .len() as i64;
            if max_shared_monitors.is_some_and(|cap| shared_monitors >= cap) {
                return Ok(CreateShareOutcome::OrgMonitorLimit);
            }
        }
        let raw = generate_raw_token();
        let share = MonitorShare {
            id: MonitorShareId(Uuid::new_v4()),
            org_id: org,
            target_id,
            label: new.label,
            token: Some(raw.clone()),
            created_at: Utc::now(),
            expires_at: new.expires_at,
            view_count: 0,
            last_viewed_at: None,
            used_by_pages: Vec::new(),
        };
        st.push(MemShare {
            share: share.clone(),
            token_hash: capability_token::hash(&raw),
            revoked: false,
        });
        Ok(CreateShareOutcome::Created(CreatedShare {
            share,
            token: raw,
        }))
    }

    async fn list_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Vec<MonitorShare>> {
        let st = self.inner.lock().unwrap();
        let mut out: Vec<MonitorShare> = st
            .iter()
            .filter(|m| !m.revoked && m.share.org_id == org && m.share.target_id == target_id)
            .map(|m| m.share.clone())
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(out)
    }

    async fn count_active_for_target(&self, org: OrgId, target_id: Uuid) -> Result<i64> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .iter()
            .filter(|m| !m.revoked && m.share.org_id == org && m.share.target_id == target_id)
            .count() as i64)
    }

    async fn revoke(
        &self,
        org: OrgId,
        target_id: Uuid,
        id: MonitorShareId,
        _actor: Option<UserId>,
    ) -> Result<bool> {
        let mut st = self.inner.lock().unwrap();
        match st.iter_mut().find(|m| {
            m.share.id == id
                && m.share.org_id == org
                && m.share.target_id == target_id
                && !m.revoked
        }) {
            Some(m) => {
                m.revoked = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn record_view(&self, id: MonitorShareId) -> Result<()> {
        if let Some(m) = self
            .inner
            .lock()
            .unwrap()
            .iter_mut()
            .find(|m| m.share.id == id)
        {
            m.share.view_count += 1;
            m.share.last_viewed_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn resolve_active(&self, raw_token: &str) -> Result<Option<ResolvedShare>> {
        let token_hash = capability_token::hash(raw_token);
        let now = Utc::now();
        let st = self.inner.lock().unwrap();
        Ok(st
            .iter()
            .find(|m| {
                !m.revoked
                    && m.token_hash == token_hash
                    && m.share.expires_at.is_none_or(|e| e > now)
            })
            .map(|m| ResolvedShare {
                share_id: m.share.id,
                target_id: m.share.target_id,
                org: m.share.org_id,
            }))
    }
}
