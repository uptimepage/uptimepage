//! Storage for `status_pages` and their `status_page_components` curation.
//!
//! Per-org, owner-managed. Mirrors the `notification_channels` cap-enforcing
//! create pattern (advisory lock + count subquery) for `max_status_pages`, and
//! the `maintenance_window_components` join idioms for component curation. Page
//! branding + logo writes live here (page-keyed), not on `organizations`.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// The plan a page's account is on, for a query that aliases the page `sp`.
pub(crate) const PAGE_PLAN_JOIN: &str = "JOIN organizations sp_org ON sp_org.id = sp.org_id \
     JOIN accounts sp_acct ON sp_acct.id = sp_org.account_id \
     JOIN plans sp_plan ON sp_plan.id = sp_acct.plan_id";

/// A custom domain is served only while verified and while the plan sells one,
/// so a downgrade sends readers back to the page's own subdomain.
pub(crate) const PAGE_CUSTOM_DOMAIN_LIVE: &str =
    "(sp.custom_domain_verified_at IS NOT NULL AND sp_plan.custom_domain_enabled)";

/// Whether the plan still covers this page. Over cap, the excess pages carry a
/// `plan_hold_at`: they 404 publicly and their subscribers hear nothing, while
/// the console keeps them whole so the operator can see what is waiting and
/// choose differently. For a query that aliases the page `sp`.
pub(crate) const PAGE_NOT_HELD: &str = "sp.plan_hold_at IS NULL";

use crate::api::error::codes;
use crate::domain::{
    MonitorShareId, NewStatusPage, NewStatusPageComponent, OrgId, PublicOrgBranding, PublicStyle,
    StatusPage, StatusPageComponent, StatusPageComponentUpdate, StatusPageId, StatusPageUpdate,
    UserId, WriteSource,
};
use crate::error::{AppError, Result};
use crate::storage::accounts;
use crate::storage::locks::{account_lock_key, advisory_xact_lock};

/// Outcome of [`StatusPageStore::add_component`]. The store stays free of
/// plan/HTTP concerns; the handler maps `OverCap` to a quota 422 with the real
/// plan id. Not-found (page/target outside the org) surfaces as an `Err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddComponentOutcome {
    Added,
    /// Already on this page — idempotent no-op.
    AlreadyOnPage,
    /// Adding a new distinct target would exceed `max_public_components`.
    OverCap {
        used: i64,
    },
}

#[derive(sqlx::FromRow)]
struct PreInsertGuard {
    page_ok: bool,
    target_ok: bool,
    already_on_page: bool,
    already_counted: bool,
    used: i64,
}

#[async_trait]
pub trait StatusPageStore: Send + Sync {
    /// Create a page, capped at `max_pages` per org (race-safe under the
    /// per-org advisory lock). `Ok(None)` = the cap was hit (the caller owns the
    /// quota error so it can name the real plan); a taken slug → `SLUG_TAKEN`.
    async fn create(
        &self,
        org: OrgId,
        new: NewStatusPage,
        source: WriteSource,
        max_pages: i64,
        actor: Option<UserId>,
    ) -> Result<Option<StatusPage>>;
    async fn list(&self, org: OrgId) -> Result<Vec<StatusPage>>;
    async fn get(&self, org: OrgId, id: StatusPageId) -> Result<Option<StatusPage>>;
    /// Update identity (name/slug/enabled) and/or branding. A taken slug →
    /// `SLUG_TAKEN`. The logo is a `page_assets` row written via
    /// `PageAssetStore`, not here. Returns the updated page, or `None` if it
    /// doesn't exist in this org.
    async fn update(
        &self,
        org: OrgId,
        id: StatusPageId,
        upd: StatusPageUpdate,
        source: WriteSource,
    ) -> Result<Option<StatusPage>>;
    async fn delete(&self, org: OrgId, id: StatusPageId, actor: Option<UserId>) -> Result<bool>;

    async fn list_components(
        &self,
        org: OrgId,
        page: StatusPageId,
    ) -> Result<Vec<StatusPageComponent>>;
    /// Add a monitor to a page. Enforces the per-org distinct-target
    /// public-component cap under the advisory lock (a target already on
    /// another page costs 0). The target must belong to `org`.
    async fn add_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        new: NewStatusPageComponent,
        max_public_components: i64,
        actor: Option<UserId>,
    ) -> Result<AddComponentOutcome>;
    async fn update_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        upd: StatusPageComponentUpdate,
    ) -> Result<bool>;
    /// Separate from [`update_component`](Self::update_component): the share is
    /// minted by the handler, not supplied by the caller. Compare-and-swap on
    /// `expected`, so two concurrent mints can't both claim the component —
    /// the loser gets `false` and revokes what it minted.
    async fn attach_share(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        expected: Option<MonitorShareId>,
        share: MonitorShareId,
    ) -> Result<bool>;
    async fn remove_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        actor: Option<UserId>,
    ) -> Result<bool>;
    /// Rewrite `sort_order` to match the given target_id order (0-based).
    async fn reorder_components(
        &self,
        org: OrgId,
        page: StatusPageId,
        ordered_target_ids: &[Uuid],
    ) -> Result<()>;

    /// Distinct ids of every page in `org` that curates any of `target_ids`.
    /// Lets a monitor write (rename, delete, enable flip) invalidate exactly
    /// the public pages it appears on. Query it *before* a delete so the
    /// cascade hasn't yet dropped the join rows.
    async fn pages_for_targets(&self, org: OrgId, target_ids: &[Uuid])
    -> Result<Vec<StatusPageId>>;
}

pub struct PgStatusPageStore {
    pool: PgPool,
}

impl PgStatusPageStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct PageRow {
    id: Uuid,
    org_id: Uuid,
    slug: String,
    name: String,
    enabled: bool,
    public_display_name: Option<String>,
    public_about: Option<String>,
    public_brand_color: Option<String>,
    logo_hash: Option<String>,
    public_show_powered_by: Option<bool>,
    public_style: String,
    public_hide_from_search: bool,
    write_source: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    plan_hold_at: Option<DateTime<Utc>>,
}

impl PageRow {
    fn into_page(self) -> StatusPage {
        StatusPage {
            id: StatusPageId(self.id),
            org_id: OrgId(self.org_id),
            slug: self.slug,
            name: self.name,
            enabled: self.enabled,
            branding: PublicOrgBranding {
                public_display_name: self.public_display_name,
                public_about: self.public_about,
                public_brand_color: self.public_brand_color,
                logo_hash: self.logo_hash,
                public_show_powered_by: self.public_show_powered_by,
                public_style: PublicStyle::from_db(&self.public_style),
                public_hide_from_search: self.public_hide_from_search,
            },
            write_source: WriteSource::from_db(&self.write_source),
            created_at: self.created_at,
            updated_at: self.updated_at,
            plan_hold_at: self.plan_hold_at,
        }
    }
}

const PAGE_COLUMNS: &str = "id, org_id, slug::text AS slug, name, enabled, \
     public_display_name, public_about, public_brand_color, \
     (SELECT pa.content_hash FROM page_assets pa \
      WHERE pa.status_page_id = status_pages.id AND pa.slot = 'logo') AS logo_hash, \
     public_show_powered_by, public_style, public_hide_from_search, \
     write_source, created_at, updated_at, plan_hold_at";

/// Groups render as one contiguous block, so a group's earliest component
/// places the whole group.
pub(crate) const COMPONENT_ORDER: &str = "MIN(spc.sort_order) OVER (PARTITION BY spc.public_group), \
     spc.public_group NULLS LAST, spc.sort_order";

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.is_unique_violation())
}

fn slug_taken() -> AppError {
    AppError::unprocessable(codes::SLUG_TAKEN, "that status-page slug is already taken")
}

#[derive(sqlx::FromRow)]
struct ComponentRow {
    target_id: Uuid,
    monitor_name: String,
    public_name: Option<String>,
    public_description: Option<String>,
    public_group: Option<String>,
    sort_order: i32,
    detail_link_enabled: bool,
    share_id: Option<MonitorShareId>,
    share_live: bool,
}

#[async_trait]
impl StatusPageStore for PgStatusPageStore {
    async fn create(
        &self,
        org: OrgId,
        new: NewStatusPage,
        source: WriteSource,
        max_pages: i64,
        actor: Option<UserId>,
    ) -> Result<Option<StatusPage>> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Pages are capped per account, so the lock and the count both take the
        // account: two orgs of one account must contend, not race.
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(db_err)?;
        let pool_orgs = accounts::live_orgs("$7");
        let row: Option<PageRow> = sqlx::query_as(&format!(
            r#"INSERT INTO status_pages (org_id, slug, name, enabled, write_source)
               SELECT $1, $2, $3, $4, $5
               WHERE (SELECT count(*) FROM status_pages WHERE org_id IN ({pool_orgs})) < $6
               RETURNING {PAGE_COLUMNS}"#
        ))
        .bind(org.0)
        .bind(&new.slug)
        .bind(&new.name)
        .bind(new.enabled)
        .bind(source.as_str())
        .bind(max_pages)
        .bind(account.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                slug_taken()
            } else {
                db_err(e)
            }
        })?;
        let Some(row) = row else {
            tx.rollback().await.ok();
            return Ok(None); // cap hit — caller maps to a plan-named quota error
        };
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            actor,
            "status_page.created",
            serde_json::json!({ "page_id": row.id, "slug": row.slug, "name": row.name }),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(Some(row.into_page()))
    }

    async fn list(&self, org: OrgId) -> Result<Vec<StatusPage>> {
        let rows: Vec<PageRow> = sqlx::query_as(&format!(
            "SELECT {PAGE_COLUMNS} FROM status_pages WHERE org_id = $1 ORDER BY created_at"
        ))
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(PageRow::into_page).collect())
    }

    async fn get(&self, org: OrgId, id: StatusPageId) -> Result<Option<StatusPage>> {
        let row: Option<PageRow> = sqlx::query_as(&format!(
            "SELECT {PAGE_COLUMNS} FROM status_pages WHERE id = $1 AND org_id = $2"
        ))
        .bind(id.0)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(PageRow::into_page))
    }

    async fn update(
        &self,
        org: OrgId,
        id: StatusPageId,
        upd: StatusPageUpdate,
        source: WriteSource,
    ) -> Result<Option<StatusPage>> {
        // Branding `Some` replaces the display fields wholesale; `None` leaves
        // them. name/slug/enabled use COALESCE so a partial update is honest.
        let b = upd.branding;
        let row: Option<PageRow> = sqlx::query_as(&format!(
            r#"UPDATE status_pages SET
                 name = COALESCE($3, name),
                 slug = COALESCE($4, slug),
                 enabled = COALESCE($5, enabled),
                 public_display_name = CASE WHEN $6::bool THEN $7 ELSE public_display_name END,
                 public_about        = CASE WHEN $6::bool THEN $8 ELSE public_about END,
                 public_brand_color  = CASE WHEN $6::bool THEN $9 ELSE public_brand_color END,
                 public_show_powered_by = CASE WHEN $6::bool THEN $10 ELSE public_show_powered_by END,
                 public_style        = CASE WHEN $6::bool THEN $11 ELSE public_style END,
                 public_hide_from_search =
                   CASE WHEN $6::bool THEN $12 ELSE public_hide_from_search END,
                 write_source = $13,
                 updated_at = now()
               WHERE id = $1 AND org_id = $2
               RETURNING {PAGE_COLUMNS}"#
        ))
        .bind(id.0)
        .bind(org.0)
        .bind(upd.name)
        .bind(upd.slug)
        .bind(upd.enabled)
        .bind(b.is_some())
        .bind(b.as_ref().and_then(|x| x.public_display_name.clone()))
        .bind(b.as_ref().and_then(|x| x.public_about.clone()))
        .bind(b.as_ref().and_then(|x| x.public_brand_color.clone()))
        .bind(b.as_ref().and_then(|x| x.public_show_powered_by))
        .bind(b.as_ref().map(|x| x.public_style.as_str()))
        .bind(b.as_ref().is_some_and(|x| x.public_hide_from_search))
        .bind(source.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| if is_unique_violation(&e) { slug_taken() } else { db_err(e) })?;
        Ok(row.map(PageRow::into_page))
    }

    async fn delete(&self, org: OrgId, id: StatusPageId, actor: Option<UserId>) -> Result<bool> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let removed: Option<(String,)> =
            sqlx::query_as("DELETE FROM status_pages WHERE id = $1 AND org_id = $2 RETURNING slug")
                .bind(id.0)
                .bind(org.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        if let Some((slug,)) = &removed {
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                "status_page.deleted",
                serde_json::json!({ "page_id": id.0, "slug": slug }),
            )
            .await?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(removed.is_some())
    }

    async fn list_components(
        &self,
        org: OrgId,
        page: StatusPageId,
    ) -> Result<Vec<StatusPageComponent>> {
        let rows: Vec<ComponentRow> = sqlx::query_as(&format!(
            r#"SELECT spc.target_id, t.name AS monitor_name,
                      spc.public_name, spc.public_description, spc.public_group, spc.sort_order,
                      spc.detail_link_enabled, spc.share_id, ms.id IS NOT NULL AS share_live
               FROM status_page_components spc
               JOIN targets t ON t.id = spc.target_id AND t.org_id = spc.org_id
               LEFT JOIN monitor_shares ms
                      ON ms.id = spc.share_id
                     AND ms.revoked_at IS NULL
                     AND (ms.expires_at IS NULL OR ms.expires_at > now())
               WHERE spc.status_page_id = $1 AND spc.org_id = $2
               ORDER BY {COMPONENT_ORDER}, t.name"#
        ))
        .bind(page.0)
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| StatusPageComponent {
                target_id: r.target_id,
                monitor_name: r.monitor_name,
                public_name: r.public_name,
                public_description: r.public_description,
                public_group: r.public_group,
                sort_order: r.sort_order,
                detail_link_enabled: r.detail_link_enabled,
                share_id: r.share_id,
                share_live: r.share_live,
            })
            .collect())
    }

    async fn add_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        new: NewStatusPageComponent,
        max_public_components: i64,
        actor: Option<UserId>,
    ) -> Result<AddComponentOutcome> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(db_err)?;
        // One snapshot under the lock distinguishes every outcome: org-membership
        // of the page and target (404, not a quota error), idempotent re-add, and
        // the distinct-target cap. A monitor already published by *any* org of
        // the account is free; a brand-new one costs 1.
        let pool_orgs = accounts::live_orgs("$4");
        let g: PreInsertGuard = sqlx::query_as(&format!(
            r#"SELECT
                 EXISTS (SELECT 1 FROM status_pages WHERE id = $2 AND org_id = $1) AS page_ok,
                 EXISTS (SELECT 1 FROM targets WHERE id = $3 AND org_id = $1) AS target_ok,
                 EXISTS (SELECT 1 FROM status_page_components
                         WHERE status_page_id = $2 AND target_id = $3) AS already_on_page,
                 EXISTS (SELECT 1 FROM status_page_components
                         WHERE org_id IN ({pool_orgs}) AND target_id = $3) AS already_counted,
                 (SELECT count(DISTINCT target_id) FROM status_page_components
                  WHERE org_id IN ({pool_orgs})) AS used"#
        ))
        .bind(org.0)
        .bind(page.0)
        .bind(new.target_id)
        .bind(account.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;

        if !g.page_ok {
            return Err(AppError::not_found(
                codes::STATUS_PAGE_NOT_FOUND,
                "status page not found",
            ));
        }
        if !g.target_ok {
            return Err(AppError::not_found(
                codes::TARGET_NOT_FOUND,
                "target not found",
            ));
        }
        if g.already_on_page {
            return Ok(AddComponentOutcome::AlreadyOnPage);
        }
        let cost = i64::from(!g.already_counted);
        if g.used + cost > max_public_components {
            return Ok(AddComponentOutcome::OverCap { used: g.used });
        }

        // ON CONFLICT is a race backstop only — already_on_page handled above.
        sqlx::query(
            r#"INSERT INTO status_page_components
                   (org_id, status_page_id, target_id, public_name, public_description,
                    public_group, sort_order, detail_link_enabled)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
               ON CONFLICT (status_page_id, target_id) DO NOTHING"#,
        )
        .bind(org.0)
        .bind(page.0)
        .bind(new.target_id)
        .bind(new.public_name)
        .bind(new.public_description)
        .bind(new.public_group)
        .bind(new.sort_order)
        .bind(new.detail_link_enabled)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            actor,
            "status_page.component_added",
            serde_json::json!({ "page_id": page.0, "target_id": new.target_id }),
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(AddComponentOutcome::Added)
    }

    async fn update_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        upd: StatusPageComponentUpdate,
    ) -> Result<bool> {
        let res = sqlx::query(
            r#"UPDATE status_page_components SET
                 public_name = CASE WHEN $4::bool THEN $5 ELSE public_name END,
                 public_description = CASE WHEN $6::bool THEN $7 ELSE public_description END,
                 public_group = CASE WHEN $8::bool THEN $9 ELSE public_group END,
                 sort_order = COALESCE($10, sort_order),
                 detail_link_enabled = COALESCE($11, detail_link_enabled),
                 updated_at = now()
               WHERE status_page_id = $1 AND target_id = $2 AND org_id = $3"#,
        )
        .bind(page.0)
        .bind(target_id)
        .bind(org.0)
        .bind(upd.public_name.is_some())
        .bind(upd.public_name.flatten())
        .bind(upd.public_description.is_some())
        .bind(upd.public_description.flatten())
        .bind(upd.public_group.is_some())
        .bind(upd.public_group.flatten())
        .bind(upd.sort_order)
        .bind(upd.detail_link_enabled)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn attach_share(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        expected: Option<MonitorShareId>,
        share: MonitorShareId,
    ) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE status_page_components SET share_id = $4, updated_at = now() \
             WHERE status_page_id = $1 AND target_id = $2 AND org_id = $3 \
               AND share_id IS NOT DISTINCT FROM $5",
        )
        .bind(page.0)
        .bind(target_id)
        .bind(org.0)
        .bind(share.0)
        .bind(expected.map(|s| s.0))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn remove_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        actor: Option<UserId>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let removed: Option<(Uuid,)> = sqlx::query_as(
            "DELETE FROM status_page_components \
             WHERE status_page_id = $1 AND target_id = $2 AND org_id = $3 \
             RETURNING target_id",
        )
        .bind(page.0)
        .bind(target_id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if removed.is_some() {
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                "status_page.component_removed",
                serde_json::json!({ "page_id": page.0, "target_id": target_id }),
            )
            .await?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(removed.is_some())
    }

    async fn reorder_components(
        &self,
        org: OrgId,
        page: StatusPageId,
        ordered_target_ids: &[Uuid],
    ) -> Result<()> {
        if ordered_target_ids.is_empty() {
            return Ok(());
        }
        let indices: Vec<i32> = (0..ordered_target_ids.len() as i32).collect();
        // Renumbered group-first, so the stored order is the rendered order and
        // a row dropped outside its group settles once instead of on next load.
        sqlx::query(
            r#"WITH incoming AS (
                   SELECT u.target_id, u.ord, spc.public_group
                   FROM UNNEST($3::uuid[], $4::int4[]) AS u(target_id, ord)
                   JOIN status_page_components spc
                     ON spc.target_id = u.target_id
                    AND spc.status_page_id = $1
                    AND spc.org_id = $2
               ),
               grouped AS (
                   SELECT target_id, ord, public_group,
                          MIN(ord) OVER (PARTITION BY public_group) AS group_ord
                   FROM incoming
               ),
               settled AS (
                   SELECT target_id,
                          ROW_NUMBER() OVER (
                              ORDER BY group_ord, public_group NULLS LAST, ord
                          ) - 1 AS ord
                   FROM grouped
               )
               UPDATE status_page_components spc
               SET sort_order = settled.ord, updated_at = now()
               FROM settled
               WHERE spc.status_page_id = $1 AND spc.org_id = $2
                 AND spc.target_id = settled.target_id"#,
        )
        .bind(page.0)
        .bind(org.0)
        .bind(ordered_target_ids)
        .bind(&indices)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn pages_for_targets(
        &self,
        org: OrgId,
        target_ids: &[Uuid],
    ) -> Result<Vec<StatusPageId>> {
        if target_ids.is_empty() {
            return Ok(vec![]);
        }
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"SELECT DISTINCT status_page_id FROM status_page_components
               WHERE org_id = $1 AND target_id = ANY($2)"#,
        )
        .bind(org.0)
        .bind(target_ids)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|(id,)| StatusPageId(id)).collect())
    }
}

fn db_err(e: sqlx::Error) -> AppError {
    AppError::Other(anyhow::anyhow!("status_pages: {e}"))
}

// ── In-memory store (tests / no-DB harnesses) ────────────────────────────────

struct MemComponent {
    page: StatusPageId,
    org: OrgId,
    target_id: Uuid,
    public_name: Option<String>,
    public_description: Option<String>,
    public_group: Option<String>,
    sort_order: i32,
    detail_link_enabled: bool,
    share_id: Option<MonitorShareId>,
}

/// Rust mirror of [`COMPONENT_ORDER`]. `key` yields (group, sort_order, target_id).
fn settle_order<T>(rows: Vec<T>, key: impl Fn(&T) -> (Option<String>, i32, Uuid)) -> Vec<T> {
    let mut mins: HashMap<Option<String>, i32> = HashMap::new();
    for row in &rows {
        let (group, ord, _) = key(row);
        mins.entry(group)
            .and_modify(|m| *m = (*m).min(ord))
            .or_insert(ord);
    }
    let mut keyed: Vec<_> = rows
        .into_iter()
        .map(|row| {
            let (group, ord, target) = key(&row);
            let group_ord = mins.get(&group).copied().unwrap_or(ord);
            ((group_ord, group.is_none(), group, ord, target), row)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    keyed.into_iter().map(|(_, row)| row).collect()
}

#[derive(Default)]
struct MemState {
    pages: Vec<StatusPage>,
    components: Vec<MemComponent>,
}

/// In-memory [`StatusPageStore`] mirroring [`PgStatusPageStore`] semantics
/// (per-org page cap, global slug uniqueness, distinct-target component cap).
/// `monitor_name` is left empty — it has no target store to resolve names from;
/// DB-backed tests use [`PgStatusPageStore`] for that.
#[derive(Default)]
pub struct InMemoryStatusPageStore {
    inner: std::sync::Mutex<MemState>,
}

impl InMemoryStatusPageStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StatusPageStore for InMemoryStatusPageStore {
    async fn create(
        &self,
        org: OrgId,
        new: NewStatusPage,
        source: WriteSource,
        max_pages: i64,
        _actor: Option<UserId>,
    ) -> Result<Option<StatusPage>> {
        let mut st = self.inner.lock().unwrap();
        if st.pages.iter().filter(|p| p.org_id == org).count() as i64 >= max_pages {
            return Ok(None); // cap hit — caller maps to a plan-named quota error
        }
        if st.pages.iter().any(|p| p.slug == new.slug) {
            return Err(slug_taken());
        }
        let now = Utc::now();
        let page = StatusPage {
            id: StatusPageId(Uuid::new_v4()),
            org_id: org,
            slug: new.slug,
            name: new.name,
            enabled: new.enabled,
            branding: PublicOrgBranding::default(),
            write_source: source,
            created_at: now,
            updated_at: now,
            plan_hold_at: None,
        };
        st.pages.push(page.clone());
        Ok(Some(page))
    }

    async fn list(&self, org: OrgId) -> Result<Vec<StatusPage>> {
        let st = self.inner.lock().unwrap();
        let mut out: Vec<StatusPage> = st
            .pages
            .iter()
            .filter(|p| p.org_id == org)
            .cloned()
            .collect();
        out.sort_by_key(|p| p.created_at);
        Ok(out)
    }

    async fn get(&self, org: OrgId, id: StatusPageId) -> Result<Option<StatusPage>> {
        let st = self.inner.lock().unwrap();
        Ok(st
            .pages
            .iter()
            .find(|p| p.id == id && p.org_id == org)
            .cloned())
    }

    async fn update(
        &self,
        org: OrgId,
        id: StatusPageId,
        upd: StatusPageUpdate,
        source: WriteSource,
    ) -> Result<Option<StatusPage>> {
        let mut st = self.inner.lock().unwrap();
        // Existence (in this org) before the slug check, so a foreign/missing
        // page is a 404, not a SLUG_TAKEN leak — matching the Pg UPDATE which
        // only collides on the row it actually matches.
        if !st.pages.iter().any(|p| p.id == id && p.org_id == org) {
            return Ok(None);
        }
        if let Some(ref s) = upd.slug
            && st.pages.iter().any(|p| p.slug == *s && p.id != id)
        {
            return Err(slug_taken());
        }
        let p = st
            .pages
            .iter_mut()
            .find(|p| p.id == id && p.org_id == org)
            .expect("checked above");
        if let Some(n) = upd.name {
            p.name = n;
        }
        if let Some(s) = upd.slug {
            p.slug = s;
        }
        if let Some(e) = upd.enabled {
            p.enabled = e;
        }
        if let Some(b) = upd.branding {
            // The logo is a separate `page_assets` row; a branding replace
            // leaves it (and its hash) alone.
            let logo = p.branding.logo_hash.clone();
            p.branding = b;
            p.branding.logo_hash = logo;
        }
        p.write_source = source;
        p.updated_at = Utc::now();
        Ok(Some(p.clone()))
    }

    async fn delete(&self, org: OrgId, id: StatusPageId, _actor: Option<UserId>) -> Result<bool> {
        let mut st = self.inner.lock().unwrap();
        let before = st.pages.len();
        st.pages.retain(|p| !(p.id == id && p.org_id == org));
        let removed = st.pages.len() != before;
        if removed {
            st.components.retain(|c| c.page != id);
        }
        Ok(removed)
    }

    async fn list_components(
        &self,
        org: OrgId,
        page: StatusPageId,
    ) -> Result<Vec<StatusPageComponent>> {
        let st = self.inner.lock().unwrap();
        let out: Vec<StatusPageComponent> = st
            .components
            .iter()
            .filter(|c| c.page == page && c.org == org)
            .map(|c| StatusPageComponent {
                target_id: c.target_id,
                monitor_name: String::new(),
                public_name: c.public_name.clone(),
                public_description: c.public_description.clone(),
                public_group: c.public_group.clone(),
                sort_order: c.sort_order,
                detail_link_enabled: c.detail_link_enabled,
                share_id: c.share_id,
                share_live: c.share_id.is_some(),
            })
            .collect();
        Ok(settle_order(out, |c| {
            (c.public_group.clone(), c.sort_order, c.target_id)
        }))
    }

    async fn add_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        new: NewStatusPageComponent,
        max_public_components: i64,
        _actor: Option<UserId>,
    ) -> Result<AddComponentOutcome> {
        // Unlike the Pg store, this has no target table, so it can't verify the
        // target belongs to `org` (no `TARGET_NOT_FOUND` path). Harnesses that
        // need that guard use `PgStatusPageStore`.
        let mut st = self.inner.lock().unwrap();
        if !st.pages.iter().any(|p| p.id == page && p.org_id == org) {
            return Err(AppError::not_found(
                codes::STATUS_PAGE_NOT_FOUND,
                "status page not found",
            ));
        }
        if st
            .components
            .iter()
            .any(|c| c.page == page && c.target_id == new.target_id)
        {
            return Ok(AddComponentOutcome::AlreadyOnPage);
        }
        let used = st
            .components
            .iter()
            .filter(|c| c.org == org)
            .map(|c| c.target_id)
            .collect::<HashSet<_>>()
            .len() as i64;
        let already_counted = st
            .components
            .iter()
            .any(|c| c.org == org && c.target_id == new.target_id);
        let cost = i64::from(!already_counted);
        if used + cost > max_public_components {
            return Ok(AddComponentOutcome::OverCap { used });
        }
        st.components.push(MemComponent {
            page,
            org,
            target_id: new.target_id,
            public_name: new.public_name,
            public_description: new.public_description,
            public_group: new.public_group,
            sort_order: new.sort_order,
            detail_link_enabled: new.detail_link_enabled,
            share_id: None,
        });
        Ok(AddComponentOutcome::Added)
    }

    async fn update_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        upd: StatusPageComponentUpdate,
    ) -> Result<bool> {
        let mut st = self.inner.lock().unwrap();
        let Some(c) = st
            .components
            .iter_mut()
            .find(|c| c.page == page && c.org == org && c.target_id == target_id)
        else {
            return Ok(false);
        };
        if let Some(v) = upd.public_name {
            c.public_name = v;
        }
        if let Some(v) = upd.public_description {
            c.public_description = v;
        }
        if let Some(v) = upd.public_group {
            c.public_group = v;
        }
        if let Some(v) = upd.sort_order {
            c.sort_order = v;
        }
        if let Some(v) = upd.detail_link_enabled {
            c.detail_link_enabled = v;
        }
        Ok(true)
    }

    async fn attach_share(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        expected: Option<MonitorShareId>,
        share: MonitorShareId,
    ) -> Result<bool> {
        let mut st = self.inner.lock().unwrap();
        let Some(c) = st
            .components
            .iter_mut()
            .find(|c| c.page == page && c.org == org && c.target_id == target_id)
        else {
            return Ok(false);
        };
        if c.share_id != expected {
            return Ok(false);
        }
        c.share_id = Some(share);
        Ok(true)
    }

    async fn remove_component(
        &self,
        org: OrgId,
        page: StatusPageId,
        target_id: Uuid,
        _actor: Option<UserId>,
    ) -> Result<bool> {
        let mut st = self.inner.lock().unwrap();
        let before = st.components.len();
        st.components
            .retain(|c| !(c.page == page && c.org == org && c.target_id == target_id));
        Ok(st.components.len() != before)
    }

    async fn reorder_components(
        &self,
        org: OrgId,
        page: StatusPageId,
        ordered_target_ids: &[Uuid],
    ) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        for (i, tid) in ordered_target_ids.iter().enumerate() {
            if let Some(c) = st
                .components
                .iter_mut()
                .find(|c| c.page == page && c.org == org && c.target_id == *tid)
            {
                c.sort_order = i as i32;
            }
        }
        let rows: Vec<&mut MemComponent> = st
            .components
            .iter_mut()
            .filter(|c| c.page == page && c.org == org)
            .collect();
        for (i, c) in settle_order(rows, |c| {
            (c.public_group.clone(), c.sort_order, c.target_id)
        })
        .into_iter()
        .enumerate()
        {
            c.sort_order = i as i32;
        }
        Ok(())
    }

    async fn pages_for_targets(
        &self,
        org: OrgId,
        target_ids: &[Uuid],
    ) -> Result<Vec<StatusPageId>> {
        let st = self.inner.lock().unwrap();
        let mut pages: Vec<StatusPageId> = st
            .components
            .iter()
            .filter(|c| c.org == org && target_ids.contains(&c.target_id))
            .map(|c| c.page)
            .collect();
        pages.sort_by_key(|p| p.0);
        pages.dedup();
        Ok(pages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NewStatusPageComponent;

    fn grouped(target_id: Uuid, group: &str, sort_order: i32) -> NewStatusPageComponent {
        NewStatusPageComponent {
            target_id,
            public_name: None,
            public_description: None,
            public_group: Some(group.into()),
            sort_order,
            detail_link_enabled: false,
        }
    }

    async fn page_with(
        components: &[(Uuid, &str)],
    ) -> (InMemoryStatusPageStore, OrgId, StatusPageId) {
        let store = InMemoryStatusPageStore::new();
        let org = OrgId(Uuid::new_v4());
        let page = store
            .create(
                org,
                NewStatusPage {
                    slug: "p".into(),
                    name: "P".into(),
                    enabled: true,
                },
                WriteSource::Api,
                10,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        for (i, (target, group)) in components.iter().enumerate() {
            store
                .add_component(org, page.id, grouped(*target, group, i as i32), 100, None)
                .await
                .unwrap();
        }
        (store, org, page.id)
    }

    async fn order(store: &InMemoryStatusPageStore, org: OrgId, page: StatusPageId) -> Vec<Uuid> {
        store
            .list_components(org, page)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.target_id)
            .collect()
    }

    #[tokio::test]
    async fn dragging_a_group_to_the_top_outranks_the_group_name() {
        let (ns1, ns2, site, clients) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let (store, org, page) = page_with(&[
            (ns1, "DNS"),
            (ns2, "DNS"),
            (site, "NQUARE"),
            (clients, "NQUARE"),
        ])
        .await;
        assert_eq!(
            order(&store, org, page).await,
            vec![ns1, ns2, site, clients]
        );

        store
            .reorder_components(org, page, &[site, clients, ns1, ns2])
            .await
            .unwrap();

        assert_eq!(
            order(&store, org, page).await,
            vec![site, clients, ns1, ns2]
        );
    }

    #[tokio::test]
    async fn a_row_dropped_into_another_group_rejoins_its_own() {
        let (a1, a2, b1) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (store, org, page) = page_with(&[(a1, "A"), (a2, "A"), (b1, "B")]).await;

        store
            .reorder_components(org, page, &[a1, b1, a2])
            .await
            .unwrap();

        assert_eq!(order(&store, org, page).await, vec![a1, a2, b1]);
    }
}
