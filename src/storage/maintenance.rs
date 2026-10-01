//! Storage layer for `maintenance_windows` and `maintenance_window_components`.
//!
//! Operator-side CRUD: the public aggregator reads its own filtered slice from
//! the same tables, but never goes through this trait so its hot path stays
//! independent.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    MaintenanceFilter, MaintenanceWindow, MaintenanceWindowUpdate, NewMaintenanceWindow, OrgId,
    UserId, WindowPhase, WriteSource,
};
use crate::error::Result;

#[derive(Debug, Clone, Copy)]
pub struct MaintenancePage {
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Default, Clone)]
pub struct MaintenanceListQuery {
    pub filter: MaintenanceFilter,
    pub limit: u32,
    pub offset: u32,
}

/// Operator-facing maintenance repository. Every method takes the caller's
/// `org` (resolved from `CurrentOrg`) so cross-tenant access is a type error,
/// not a runtime check.
#[async_trait]
pub trait MaintenanceStore: Send + Sync {
    async fn create(
        &self,
        org: OrgId,
        new: NewMaintenanceWindow,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MaintenanceWindow>;
    async fn list(&self, org: OrgId, q: MaintenanceListQuery) -> Result<Vec<MaintenanceWindow>>;
    /// A cancelled window is returned too, with `deleted_at` set.
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<MaintenanceWindow>>;
    /// `None` when the window is unknown, cancelled or already over.
    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: MaintenanceWindowUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<MaintenanceWindow>>;
    /// Cancels a window that has not ended, keeping it as history. `false` when
    /// it is unknown, already cancelled or already over.
    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<bool>;
    /// Subset of `ids` that exist in `targets` for the caller's org. Used to
    /// validate `component_ids` on create/update without requiring callers to
    /// plumb in a `TargetStore`.
    async fn existing_target_ids(&self, org: OrgId, ids: &[Uuid]) -> Result<Vec<Uuid>>;
    /// Whether a window that silences paging currently covers this target.
    /// A window with `suppress_alerts` off never matches, so it stays a purely
    /// public announcement.
    async fn alerts_suppressed(&self, org: OrgId, target_id: Uuid) -> Result<bool>;
}

// ── Postgres impl ────────────────────────────────────────────────────────

pub struct PgMaintenanceStore {
    pool: PgPool,
}

impl PgMaintenanceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct MaintenanceRow {
    id: Uuid,
    title: String,
    description: Option<String>,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    suppress_alerts: bool,
    write_source: String,
    created_by: Option<Uuid>,
    updated_by: Option<Uuid>,
    deleted_at: Option<DateTime<Utc>>,
    deleted_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, title, description, starts_at, ends_at, suppress_alerts, write_source, \
                       created_by, updated_by, deleted_at, deleted_by, created_at, updated_at";

impl MaintenanceRow {
    fn into_window(self, component_ids: Vec<Uuid>) -> MaintenanceWindow {
        MaintenanceWindow {
            id: self.id,
            title: self.title,
            description: self.description,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            suppress_alerts: self.suppress_alerts,
            component_ids,
            write_source: WriteSource::from_db(&self.write_source),
            created_by: self.created_by.map(UserId),
            updated_by: self.updated_by.map(UserId),
            deleted_at: self.deleted_at,
            deleted_by: self.deleted_by.map(UserId),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

async fn load_components(
    pool: &PgPool,
    maintenance_ids: &[Uuid],
    org_id: Uuid,
) -> Result<HashMap<Uuid, Vec<Uuid>>> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        r#"SELECT maintenance_id, target_id FROM maintenance_window_components
           WHERE maintenance_id = ANY($1::uuid[]) AND org_id = $2 ORDER BY target_id"#,
    )
    .bind(maintenance_ids)
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("load_components: {e}"))?;
    let mut by_window: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (maintenance_id, target_id) in rows {
        by_window.entry(maintenance_id).or_default().push(target_id);
    }
    Ok(by_window)
}

async fn load_components_of(
    pool: &PgPool,
    maintenance_id: Uuid,
    org_id: Uuid,
) -> Result<Vec<Uuid>> {
    Ok(load_components(pool, &[maintenance_id], org_id)
        .await?
        .remove(&maintenance_id)
        .unwrap_or_default())
}

#[async_trait]
impl MaintenanceStore for PgMaintenanceStore {
    async fn create(
        &self,
        org: OrgId,
        new: NewMaintenanceWindow,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MaintenanceWindow> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        let row: MaintenanceRow = sqlx::query_as(&format!(
            r#"INSERT INTO maintenance_windows
                   (org_id, title, description, starts_at, ends_at, suppress_alerts, write_source,
                    created_by, updated_by)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
               RETURNING {COLUMNS}"#,
        ))
        .bind(org.0)
        .bind(&new.title)
        .bind(&new.description)
        .bind(new.starts_at)
        .bind(new.ends_at)
        .bind(new.suppress_alerts)
        .bind(source.as_str())
        .bind(actor.map(|u| u.0))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("insert maintenance: {e}"))?;
        if !new.component_ids.is_empty() {
            // The org-match trigger only validates child.org_id == parent.org_id,
            // not that the referenced target belongs to the parent's org. The
            // join on `targets t` filters out any UUID that belongs to a
            // different tenant, so a caller passing another org's target id
            // silently no-ops instead of inserting a cross-tenant reference.
            sqlx::query(
                r#"INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
                   SELECT mw.org_id, mw.id, t.id
                   FROM maintenance_windows mw
                   CROSS JOIN UNNEST($2::uuid[]) AS u(target_id)
                   JOIN targets t ON t.id = u.target_id AND t.org_id = mw.org_id
                   WHERE mw.id = $1"#,
            )
            .bind(row.id)
            .bind(&new.component_ids)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("insert components: {e}"))?;
        }
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            actor,
            "maintenance.created",
            serde_json::json!({ "maintenance_id": row.id, "title": row.title }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(row.into_window(new.component_ids))
    }

    async fn list(&self, org: OrgId, q: MaintenanceListQuery) -> Result<Vec<MaintenanceWindow>> {
        let now = Utc::now();
        let rows: Vec<MaintenanceRow> = sqlx::query_as(&list_sql(q.filter))
            .bind(now)
            .bind(q.limit as i64)
            .bind(q.offset as i64)
            .bind(org.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("list maintenance: {e}"))?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let window_ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let mut components = load_components(&self.pool, &window_ids, org.0).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let targets = components.remove(&row.id).unwrap_or_default();
                row.into_window(targets)
            })
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<MaintenanceWindow>> {
        let row: Option<MaintenanceRow> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM maintenance_windows WHERE id = $1 AND org_id = $2",
        ))
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("get maintenance: {e}"))?;
        match row {
            Some(r) => {
                let components = load_components_of(&self.pool, r.id, org.0).await?;
                Ok(Some(r.into_window(components)))
            }
            None => Ok(None),
        }
    }

    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: MaintenanceWindowUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<MaintenanceWindow>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        // `description` uses single-Option semantics (per the addendum wire type):
        // `Some("…")` overwrites, `None` leaves the stored value untouched.
        // To distinguish leave-alone from clear-to-null, the field would have
        // to be `Option<Option<String>>` like `IncidentNarrationUpdate`.
        let row: Option<MaintenanceRow> = sqlx::query_as(&format!(
            r#"UPDATE maintenance_windows
               SET title        = COALESCE($2, title),
                   description  = COALESCE($3, description),
                   starts_at    = COALESCE($4, starts_at),
                   ends_at      = COALESCE($5, ends_at),
                   suppress_alerts = COALESCE($8, suppress_alerts),
                   write_source = $7,
                   updated_by   = $9,
                   updated_at   = now()
               WHERE id = $1 AND org_id = $6 AND {unfinished}
               RETURNING {COLUMNS}"#,
            unfinished = unfinished_sql("", "now()"),
        ))
        .bind(id)
        .bind(update.title.as_ref())
        .bind(update.description.clone())
        .bind(update.starts_at)
        .bind(update.ends_at)
        .bind(org.0)
        .bind(source.as_str())
        .bind(update.suppress_alerts)
        .bind(actor.map(|u| u.0))
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("update maintenance: {e}"))?;
        let Some(row) = row else {
            tx.rollback().await.ok();
            return Ok(None);
        };
        if let Some(ids) = update.component_ids.as_ref() {
            sqlx::query(
                r#"DELETE FROM maintenance_window_components
                   WHERE maintenance_id = $1 AND org_id = $2"#,
            )
            .bind(row.id)
            .bind(org.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("delete components: {e}"))?;
            if !ids.is_empty() {
                // Drops any input UUID that doesn't belong to the same org as
                // the parent window — see the matching comment in `create`.
                sqlx::query(
                    r#"INSERT INTO maintenance_window_components (org_id, maintenance_id, target_id)
                       SELECT mw.org_id, mw.id, t.id
                       FROM maintenance_windows mw
                       CROSS JOIN UNNEST($2::uuid[]) AS u(target_id)
                       JOIN targets t ON t.id = u.target_id AND t.org_id = mw.org_id
                       WHERE mw.id = $1"#,
                )
                .bind(row.id)
                .bind(ids)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("insert components: {e}"))?;
            }
        }
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            actor,
            "maintenance.updated",
            serde_json::json!({
                "maintenance_id": row.id,
                "title": row.title,
                "changed": update.changed_fields(),
            }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        let components = load_components_of(&self.pool, row.id, org.0).await?;
        Ok(Some(row.into_window(components)))
    }

    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        let cancelled: Option<(String,)> = sqlx::query_as(&format!(
            r#"UPDATE maintenance_windows
               SET deleted_at   = now(),
                   deleted_by   = $3,
                   updated_by   = $3,
                   write_source = $4,
                   updated_at   = now()
               WHERE id = $1 AND org_id = $2 AND {unfinished}
               RETURNING title"#,
            unfinished = unfinished_sql("", "now()"),
        ))
        .bind(id)
        .bind(org.0)
        .bind(actor.map(|u| u.0))
        .bind(source.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("cancel maintenance: {e}"))?;
        let Some((title,)) = cancelled else {
            tx.rollback().await.ok();
            return Ok(false);
        };
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            actor,
            "maintenance.cancelled",
            serde_json::json!({ "maintenance_id": id, "title": title }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(true)
    }

    async fn existing_target_ids(&self, org: OrgId, ids: &[Uuid]) -> Result<Vec<Uuid>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Uuid,)> =
            sqlx::query_as(r#"SELECT id FROM targets WHERE id = ANY($1::uuid[]) AND org_id = $2"#)
                .bind(ids)
                .bind(org.0)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| anyhow::anyhow!("existing_target_ids: {e}"))?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    async fn alerts_suppressed(&self, org: OrgId, target_id: Uuid) -> Result<bool> {
        let sql = format!("SELECT {}", suppressing_window_sql("$1", "$2"));
        let (suppressed,): (bool,) = sqlx::query_as(&sql)
            .bind(target_id)
            .bind(org.0)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("alerts_suppressed: {e}"))?;
        Ok(suppressed)
    }
}

/// The phase boundaries as SQL, for a query whose `maintenance_windows` carries
/// `alias` (`""` or `"mw."`) and compares against `now` (an expression or a bind
/// placeholder). The half-open range matches [`WindowPhase`]. The quota count
/// (`count_sql`) states `unfinished` itself, and the public read and subscriber
/// fan-out filter on a range or a status rather than a phase.
fn running_sql(alias: &str, now: &str) -> String {
    format!("{alias}deleted_at IS NULL AND {alias}starts_at <= {now} AND {alias}ends_at > {now}")
}

fn upcoming_sql(alias: &str, now: &str) -> String {
    format!("{alias}deleted_at IS NULL AND {alias}starts_at > {now}")
}

fn unfinished_sql(alias: &str, now: &str) -> String {
    format!("{alias}deleted_at IS NULL AND {alias}ends_at > {now}")
}

fn past_sql(alias: &str, now: &str) -> String {
    format!("({alias}deleted_at IS NOT NULL OR {alias}ends_at <= {now})")
}

/// `EXISTS` clause for "a window is silencing this target right now", against
/// whichever target/org columns the caller's query already has in scope.
pub fn suppressing_window_sql(target_col: &str, org_col: &str) -> String {
    format!(
        "EXISTS ( \
             SELECT 1 FROM maintenance_window_components mwc \
             JOIN maintenance_windows mw ON mw.id = mwc.maintenance_id \
             WHERE mwc.target_id = {target_col} AND mwc.org_id = {org_col} \
               AND mw.suppress_alerts AND {running} \
         )",
        running = running_sql("mw.", "now()"),
    )
}

fn list_sql(filter: MaintenanceFilter) -> String {
    format!(
        r#"SELECT {COLUMNS}
           FROM maintenance_windows
           WHERE org_id = $4 AND ({clause})
           ORDER BY {order}
           LIMIT $2 OFFSET $3"#,
        clause = filter_clause(filter),
        order = match filter {
            MaintenanceFilter::Past => "COALESCE(deleted_at, ends_at) DESC, id DESC",
            MaintenanceFilter::Upcoming => "starts_at ASC, id ASC",
            MaintenanceFilter::Active | MaintenanceFilter::All => "starts_at DESC, id DESC",
        },
    )
}

fn filter_clause(filter: MaintenanceFilter) -> String {
    match filter {
        MaintenanceFilter::Active => running_sql("", "$1"),
        MaintenanceFilter::Upcoming => upcoming_sql("", "$1"),
        MaintenanceFilter::Past => past_sql("", "$1"),
        MaintenanceFilter::All => "$1 IS NOT NULL OR $1 IS NULL".to_owned(),
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────

#[derive(Default)]
pub struct InMemoryMaintenanceStore {
    inner: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    windows: Vec<MaintenanceWindow>,
    known_targets: std::collections::HashSet<Uuid>,
}

impl InMemoryMaintenanceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-populate the set of target ids that `existing_target_ids` will
    /// consider valid. Tests use this to drive `INVALID_COMPONENT_ID` cases
    /// without spinning up a target store.
    pub fn with_targets(targets: impl IntoIterator<Item = Uuid>) -> Self {
        let mut state = InMemoryState::default();
        state.known_targets.extend(targets);
        Self {
            inner: Mutex::new(state),
        }
    }

    pub fn register_target(&self, id: Uuid) {
        self.inner.lock().known_targets.insert(id);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().windows.is_empty()
    }
}

#[async_trait]
impl MaintenanceStore for InMemoryMaintenanceStore {
    async fn create(
        &self,
        _org: OrgId,
        new: NewMaintenanceWindow,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MaintenanceWindow> {
        let now = Utc::now();
        let id = Uuid::now_v7();
        let mw = MaintenanceWindow {
            id,
            title: new.title,
            description: new.description,
            starts_at: new.starts_at,
            ends_at: new.ends_at,
            suppress_alerts: new.suppress_alerts,
            component_ids: new.component_ids,
            write_source: source,
            created_by: actor,
            updated_by: actor,
            deleted_at: None,
            deleted_by: None,
            created_at: now,
            updated_at: now,
        };
        self.inner.lock().windows.push(mw.clone());
        Ok(mw)
    }

    async fn list(&self, _org: OrgId, q: MaintenanceListQuery) -> Result<Vec<MaintenanceWindow>> {
        let now = Utc::now();
        let g = self.inner.lock();
        let mut filtered: Vec<MaintenanceWindow> = g
            .windows
            .iter()
            .filter(|w| match_filter(w, q.filter, now))
            .cloned()
            .collect();
        match q.filter {
            MaintenanceFilter::Upcoming => filtered.sort_by_key(|w| w.starts_at),
            MaintenanceFilter::Past => {
                filtered.sort_by_key(|w| std::cmp::Reverse(w.deleted_at.unwrap_or(w.ends_at)));
            }
            MaintenanceFilter::Active | MaintenanceFilter::All => {
                filtered.sort_by_key(|w| std::cmp::Reverse(w.starts_at));
            }
        }
        let start = q.offset as usize;
        let end = (start + q.limit as usize).min(filtered.len());
        if start >= filtered.len() {
            return Ok(Vec::new());
        }
        Ok(filtered[start..end].to_vec())
    }

    async fn get(&self, _org: OrgId, id: Uuid) -> Result<Option<MaintenanceWindow>> {
        Ok(self
            .inner
            .lock()
            .windows
            .iter()
            .find(|w| w.id == id)
            .cloned())
    }

    async fn update(
        &self,
        _org: OrgId,
        id: Uuid,
        update: MaintenanceWindowUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<MaintenanceWindow>> {
        let now = Utc::now();
        let mut g = self.inner.lock();
        let Some(w) = g
            .windows
            .iter_mut()
            .find(|w| w.id == id && !w.phase(now).is_closed())
        else {
            return Ok(None);
        };
        if let Some(t) = update.title {
            w.title = t;
        }
        if let Some(d) = update.description {
            w.description = Some(d);
        }
        if let Some(s) = update.starts_at {
            w.starts_at = s;
        }
        if let Some(e) = update.ends_at {
            w.ends_at = e;
        }
        if let Some(s) = update.suppress_alerts {
            w.suppress_alerts = s;
        }
        if let Some(c) = update.component_ids {
            w.component_ids = c;
        }
        w.write_source = source;
        w.updated_by = actor;
        w.updated_at = now;
        Ok(Some(w.clone()))
    }

    async fn delete(
        &self,
        _org: OrgId,
        id: Uuid,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<bool> {
        let now = Utc::now();
        let mut g = self.inner.lock();
        let Some(w) = g
            .windows
            .iter_mut()
            .find(|w| w.id == id && w.deleted_at.is_none() && w.ends_at > now)
        else {
            return Ok(false);
        };
        w.deleted_at = Some(now);
        w.deleted_by = actor;
        w.updated_by = actor;
        w.write_source = source;
        w.updated_at = now;
        Ok(true)
    }

    async fn existing_target_ids(&self, _org: OrgId, ids: &[Uuid]) -> Result<Vec<Uuid>> {
        let g = self.inner.lock();
        Ok(ids
            .iter()
            .filter(|id| g.known_targets.contains(id))
            .copied()
            .collect())
    }

    async fn alerts_suppressed(&self, _org: OrgId, target_id: Uuid) -> Result<bool> {
        let now = Utc::now();
        Ok(self.inner.lock().windows.iter().any(|w| {
            w.suppress_alerts
                && w.component_ids.contains(&target_id)
                && w.phase(now) == WindowPhase::Active
        }))
    }
}

fn match_filter(w: &MaintenanceWindow, filter: MaintenanceFilter, now: DateTime<Utc>) -> bool {
    let phase = w.phase(now);
    match filter {
        MaintenanceFilter::Active => phase == WindowPhase::Active,
        MaintenanceFilter::Upcoming => phase == WindowPhase::Upcoming,
        MaintenanceFilter::Past => phase.is_closed(),
        MaintenanceFilter::All => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn upcoming() -> NewMaintenanceWindow {
        NewMaintenanceWindow {
            title: "Upgrade".into(),
            description: None,
            starts_at: Utc::now() + ChronoDuration::hours(1),
            ends_at: Utc::now() + ChronoDuration::hours(2),
            component_ids: vec![],
            suppress_alerts: true,
        }
    }

    fn org() -> OrgId {
        OrgId(Uuid::nil())
    }

    #[tokio::test]
    async fn create_and_list_roundtrip() {
        let store = InMemoryMaintenanceStore::new();
        let mw = store
            .create(org(), upcoming(), WriteSource::Ui, None)
            .await
            .unwrap();
        let list = store
            .list(
                org(),
                MaintenanceListQuery {
                    filter: MaintenanceFilter::All,
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, mw.id);
    }

    #[tokio::test]
    async fn filter_active_excludes_upcoming() {
        let store = InMemoryMaintenanceStore::new();
        store
            .create(org(), upcoming(), WriteSource::Ui, None)
            .await
            .unwrap();
        let active = store
            .list(
                org(),
                MaintenanceListQuery {
                    filter: MaintenanceFilter::Active,
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn update_replaces_components() {
        let store = InMemoryMaintenanceStore::new();
        let mw = store
            .create(org(), upcoming(), WriteSource::Ui, None)
            .await
            .unwrap();
        let new_id = Uuid::now_v7();
        let patched = store
            .update(
                org(),
                mw.id,
                MaintenanceWindowUpdate {
                    component_ids: Some(vec![new_id]),
                    ..Default::default()
                },
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(patched.component_ids, vec![new_id]);
    }

    #[tokio::test]
    async fn update_toggles_alert_suppression() {
        let store = InMemoryMaintenanceStore::new();
        let mw = store
            .create(org(), upcoming(), WriteSource::Ui, None)
            .await
            .unwrap();
        assert!(mw.suppress_alerts);
        let patched = store
            .update(
                org(),
                mw.id,
                MaintenanceWindowUpdate {
                    suppress_alerts: Some(false),
                    ..Default::default()
                },
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(!patched.suppress_alerts);
    }

    #[tokio::test]
    async fn only_a_running_window_suppresses() {
        let target = Uuid::now_v7();
        let store = InMemoryMaintenanceStore::new();
        let window = |from_hours: i64, to_hours: i64| NewMaintenanceWindow {
            title: "w".into(),
            description: None,
            starts_at: Utc::now() + ChronoDuration::hours(from_hours),
            ends_at: Utc::now() + ChronoDuration::hours(to_hours),
            component_ids: vec![target],
            suppress_alerts: true,
        };
        for (from, to, expected) in [(-2, -1, false), (1, 2, false), (-1, 1, true)] {
            let s = InMemoryMaintenanceStore::new();
            s.create(org(), window(from, to), WriteSource::Ui, None)
                .await
                .unwrap();
            assert_eq!(
                s.alerts_suppressed(org(), target).await.unwrap(),
                expected,
                "window {from}h..{to}h"
            );
        }
        assert!(!store.alerts_suppressed(org(), target).await.unwrap());
    }

    #[tokio::test]
    async fn existing_target_ids_filters_unknown() {
        let known = Uuid::now_v7();
        let store = InMemoryMaintenanceStore::with_targets([known]);
        let unknown = Uuid::now_v7();
        let got = store
            .existing_target_ids(org(), &[known, unknown])
            .await
            .unwrap();
        assert_eq!(got, vec![known]);
    }

    #[tokio::test]
    async fn delete_returns_false_for_unknown() {
        let store = InMemoryMaintenanceStore::new();
        assert!(
            !store
                .delete(org(), Uuid::now_v7(), WriteSource::Ui, None)
                .await
                .unwrap()
        );
    }

    fn query(filter: MaintenanceFilter) -> MaintenanceListQuery {
        MaintenanceListQuery {
            filter,
            limit: 10,
            offset: 0,
        }
    }

    #[tokio::test]
    async fn cancelling_keeps_the_window_as_history() {
        let store = InMemoryMaintenanceStore::new();
        let who = UserId(Uuid::now_v7());
        let mw = store
            .create(org(), upcoming(), WriteSource::Ui, None)
            .await
            .unwrap();

        assert!(
            store
                .delete(org(), mw.id, WriteSource::Api, Some(who))
                .await
                .unwrap()
        );

        let upcoming_now = store
            .list(org(), query(MaintenanceFilter::Upcoming))
            .await
            .unwrap();
        assert!(
            upcoming_now.is_empty(),
            "a cancelled window is not upcoming"
        );
        let past = store
            .list(org(), query(MaintenanceFilter::Past))
            .await
            .unwrap();
        assert_eq!(past.len(), 1);
        assert_eq!(past[0].deleted_by, Some(who));
        assert!(past[0].deleted_at.is_some());
        assert_eq!(past[0].write_source, WriteSource::Api);
        assert_eq!(
            store.get(org(), mw.id).await.unwrap().unwrap().deleted_by,
            Some(who)
        );
    }

    #[tokio::test]
    async fn a_cancelled_window_cannot_be_cancelled_or_edited_again() {
        let store = InMemoryMaintenanceStore::new();
        let mw = store
            .create(org(), upcoming(), WriteSource::Ui, None)
            .await
            .unwrap();
        store
            .delete(org(), mw.id, WriteSource::Ui, None)
            .await
            .unwrap();

        assert!(
            !store
                .delete(org(), mw.id, WriteSource::Ui, None)
                .await
                .unwrap()
        );
        let edited = store
            .update(
                org(),
                mw.id,
                MaintenanceWindowUpdate {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap();
        assert!(edited.is_none());
    }

    #[tokio::test]
    async fn a_cancelled_window_stops_suppressing() {
        let target = Uuid::now_v7();
        let store = InMemoryMaintenanceStore::new();
        let mw = store
            .create(
                org(),
                NewMaintenanceWindow {
                    title: "w".into(),
                    description: None,
                    starts_at: Utc::now() - ChronoDuration::hours(1),
                    ends_at: Utc::now() + ChronoDuration::hours(1),
                    component_ids: vec![target],
                    suppress_alerts: true,
                },
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap();
        assert!(store.alerts_suppressed(org(), target).await.unwrap());

        store
            .delete(org(), mw.id, WriteSource::Ui, None)
            .await
            .unwrap();

        assert!(!store.alerts_suppressed(org(), target).await.unwrap());
    }

    #[tokio::test]
    async fn create_and_update_record_who_did_it() {
        let store = InMemoryMaintenanceStore::new();
        let author = UserId(Uuid::now_v7());
        let editor = UserId(Uuid::now_v7());
        let mw = store
            .create(org(), upcoming(), WriteSource::Ui, Some(author))
            .await
            .unwrap();
        assert_eq!(mw.created_by, Some(author));
        assert_eq!(mw.updated_by, Some(author));

        let patched = store
            .update(
                org(),
                mw.id,
                MaintenanceWindowUpdate {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
                WriteSource::Ui,
                Some(editor),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(patched.created_by, Some(author));
        assert_eq!(patched.updated_by, Some(editor));
    }
}
