//! Alert sources: the tools that send alerts in, their channels in
//! `alert_source_channels`, their tokens, and their delete, which closes the
//! incidents they hold open.
//!
//! LOCK ORDER: the source's advisory lock, the account lock, channels, the
//! escalation policy, the source row, then incidents (each by its advisory
//! lock, then its row). A policy delete takes the policy row before any
//! source, so a write here holds the policy before the source row.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    AlertGrouping, AlertSource, AlertSourceKind, AlertSourceUpdate, MAX_ALERT_SOURCE_NAME_LEN,
    MAX_SIGNING_SECRET_LEN, MIN_SIGNING_SECRET_LEN, NewAlertSource, OrgId, TargetAlerts, UserId,
    WriteSource, normalize_group_name,
};
use crate::error::{AppError, Result, codes, validation};
use crate::security::Cipher;
use crate::storage::accounts;
use crate::storage::capability_token;
use crate::storage::channel_bindings::{
    ALERT_CHANNEL_FIELD, BindingOwner, insert_bindings, lock_bound_channels, set_bindings,
    source_alerts_column,
};
use crate::storage::db;
use crate::storage::heartbeats::PREV_TOKEN_OVERLAP;
use crate::storage::incident_ops::{Actor, LifecycleOutcome, resolve_in_tx};
use crate::storage::locks::{account_lock_key, advisory_xact_lock, alert_source_lock_key};
use crate::storage::orgs::record_audit_tx;

/// No live policy of its own, no live org default and no enabled bound
/// channel: an alert would open and page nobody. For the alert_sources row the
/// query names `$s`.
macro_rules! routes_to_nobody_column {
    ($s:literal) => {
        concat!(
            "NOT (EXISTS (SELECT 1 FROM escalation_policies p WHERE p.id = ",
            $s,
            ".escalation_policy_id AND p.org_id = ",
            $s,
            ".org_id AND p.deleted_at IS NULL) \
             OR EXISTS (SELECT 1 FROM organizations o \
                 JOIN escalation_policies d ON d.id = o.default_escalation_policy_id \
                     AND d.org_id = o.id AND d.deleted_at IS NULL \
                 WHERE o.id = ",
            $s,
            ".org_id) \
             OR EXISTS (SELECT 1 FROM alert_source_channels sc \
                 JOIN notification_channels c ON c.id = sc.channel_id AND c.org_id = sc.org_id \
                     AND c.enabled \
                 WHERE sc.source_id = ",
            $s,
            ".id)) AS routes_to_nobody"
        )
    };
}

const SOURCE_COLUMNS: &str = concat!(
    "s.id, s.org_id, s.name, s.kind, s.enabled, s.plan_hold_at, s.token_enc, \
     s.token_rotated_at, s.prev_token_expires_at, s.prev_token_last_used_at, \
     s.signing_secret_enc, s.grouping, s.escalation_policy_id, s.notify_recovery, \
     s.renotify_interval_secs, s.last_event_at, s.write_source, s.created_at, s.created_by, \
     s.updated_at, s.updated_by, ",
    source_alerts_column!("s"),
    ", ",
    routes_to_nobody_column!("s")
);

#[derive(sqlx::FromRow)]
struct SourceRow {
    id: Uuid,
    org_id: Uuid,
    name: String,
    kind: String,
    enabled: bool,
    plan_hold_at: Option<DateTime<Utc>>,
    token_enc: String,
    token_rotated_at: Option<DateTime<Utc>>,
    prev_token_expires_at: Option<DateTime<Utc>>,
    prev_token_last_used_at: Option<DateTime<Utc>>,
    signing_secret_enc: Option<String>,
    grouping: String,
    escalation_policy_id: Option<Uuid>,
    notify_recovery: bool,
    renotify_interval_secs: i32,
    last_event_at: Option<DateTime<Utc>>,
    write_source: String,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    updated_at: DateTime<Utc>,
    updated_by: Option<Uuid>,
    alerts: serde_json::Value,
    routes_to_nobody: bool,
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    #[sqlx(flatten)]
    row: SourceRow,
    previous: bool,
}

impl SourceRow {
    fn decode(self, cipher: Option<&Cipher>) -> Result<AlertSource> {
        let alerts: TargetAlerts = serde_json::from_value(self.alerts)
            .map_err(|e| AppError::Other(anyhow!("decoding alert source channels: {e}")))?;
        Ok(AlertSource {
            id: self.id,
            name: self.name,
            kind: AlertSourceKind::from_db(&self.kind),
            enabled: self.enabled,
            plan_hold_at: self.plan_hold_at,
            token: capability_token::open(&self.token_enc, cipher),
            token_rotated_at: self.token_rotated_at,
            prev_token_expires_at: self.prev_token_expires_at,
            prev_token_last_used_at: self.prev_token_last_used_at,
            signed: self.signing_secret_enc.is_some(),
            grouping: AlertGrouping::from_db(&self.grouping),
            escalation_policy_id: self.escalation_policy_id,
            alerts,
            notify_recovery: self.notify_recovery,
            renotify_interval_secs: self.renotify_interval_secs.max(0) as u32,
            last_event_at: self.last_event_at,
            routes_to_nobody: self.routes_to_nobody,
            write_source: WriteSource::from_db(&self.write_source),
            created_at: self.created_at,
            created_by: self.created_by.map(UserId),
            updated_at: self.updated_at,
            updated_by: self.updated_by.map(UserId),
        })
    }
}

/// The source a presented token belongs to, with what ingest needs and the
/// API never shows.
#[derive(Debug, Clone)]
pub struct TokenMatch {
    pub org: OrgId,
    pub source: AlertSource,
    /// Opened, for checking a signature. `None` when unset, or when the KEK
    /// rotated out, which ingest must treat as a failed check.
    pub signing_secret: Option<String>,
    /// Matched the superseded token inside its overlap.
    pub previous: bool,
}

/// What deleting a source did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedSource {
    pub name: String,
    /// Incidents it held open, resolved by the delete.
    pub closed: Vec<Uuid>,
}

/// A source's name and tool, without its token or route: what a filter or a
/// label needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceName {
    pub id: Uuid,
    pub name: String,
    pub kind: AlertSourceKind,
}

#[async_trait]
pub trait AlertSourceStore: Send + Sync {
    async fn list(&self, org: OrgId) -> Result<Vec<AlertSource>>;
    /// Every source's name and tool, by name. Opens no token.
    async fn names(&self, org: OrgId) -> Result<Vec<SourceName>>;
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<AlertSource>>;
    /// Refused past `cap` sources across the account's orgs, and when the
    /// source would page nobody.
    async fn create(
        &self,
        org: OrgId,
        new: NewAlertSource,
        source: WriteSource,
        actor: Option<UserId>,
        cap: i64,
    ) -> Result<AlertSource>;
    /// Refused when the result would page nobody, and a grouping change while
    /// the source holds an incident open.
    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: AlertSourceUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<AlertSource>>;
    /// Resolves the incidents it holds open, then deletes it, all or nothing.
    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Option<UserId>,
    ) -> Result<Option<DeletedSource>>;
    /// New token live at once; the old one is accepted for
    /// [`PREV_TOKEN_OVERLAP`] unless `revoke_previous`.
    async fn rotate(
        &self,
        org: OrgId,
        id: Uuid,
        revoke_previous: bool,
        actor: Option<UserId>,
    ) -> Result<Option<AlertSource>>;
    /// Ends the overlap now. False when there is none.
    async fn revoke_previous(&self, org: OrgId, id: Uuid, actor: Option<UserId>) -> Result<bool>;
    /// The source `raw` opens, in a live org. A superseded token inside its
    /// overlap matches and has its last use stamped.
    async fn find_by_token(&self, raw: &str) -> Result<Option<TokenMatch>>;
}

fn name_taken(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.constraint() == Some("alert_sources_org_id_name_key"))
}

fn name_taken_error() -> AppError {
    AppError::conflict(
        codes::ALERT_SOURCE_NAME_TAKEN,
        "an alert source with this name already exists",
    )
}

fn required_name(raw: &str) -> Result<String> {
    let name = normalize_group_name(raw).ok_or_else(|| {
        AppError::bad_request_field(
            codes::INVALID_ALERT_SOURCE_NAME,
            "name must not be blank",
            "name",
        )
    })?;
    validation::check_length(
        name,
        "name",
        MAX_ALERT_SOURCE_NAME_LEN,
        codes::ALERT_SOURCE_NAME_TOO_LONG,
    )?;
    Ok(name.to_owned())
}

fn checked_secret(kind: AlertSourceKind, secret: &str) -> Result<()> {
    let refuse = |message: String| {
        Err(AppError::bad_request_field(
            codes::INVALID_SIGNING_SECRET,
            message,
            "signing_secret",
        ))
    };
    if !kind.signs() {
        return refuse(format!(
            "a {} source does not sign what it sends",
            kind.as_db_str()
        ));
    }
    let len = secret.chars().count();
    if !(MIN_SIGNING_SECRET_LEN..=MAX_SIGNING_SECRET_LEN).contains(&len) {
        return refuse(format!(
            "signing_secret must be {MIN_SIGNING_SECRET_LEN} to {MAX_SIGNING_SECRET_LEN} characters"
        ));
    }
    Ok(())
}

/// What every write checks before it takes a lock.
fn checked_shape(renotify: Option<u32>, alerts: Option<&TargetAlerts>) -> Result<()> {
    crate::targets::validate::validate_renotify_interval(renotify)?;
    alerts.map_or(Ok(()), crate::targets::validate::validate_alerts)
}

fn no_route() -> AppError {
    AppError::unprocessable(
        codes::ALERT_SOURCE_NO_ROUTE,
        "this source would page nobody: give it an escalation policy, set an org default \
         policy, or bind an enabled channel",
    )
}

fn touches_route(update: &AlertSourceUpdate) -> bool {
    update.escalation_policy_id.is_some() || update.alerts.is_some()
}

fn grouping_locked() -> AppError {
    AppError::conflict(
        codes::ALERT_SOURCE_GROUPING_LOCKED,
        "grouping can change only while the source holds no open incident",
    )
}

fn policy_not_found(id: Uuid) -> AppError {
    AppError::bad_request_field(
        codes::ESCALATION_POLICY_NOT_FOUND,
        format!("escalation policy {id} does not exist"),
        "escalation_policy_id",
    )
}

fn seal(raw: &str, cipher: Option<&Cipher>) -> Result<String> {
    crate::security::seal_str(raw, cipher)
        .map_err(|e| AppError::Other(anyhow!("signing secret encryption: {e}")))
}

pub struct PgAlertSourceStore {
    pool: PgPool,
    cipher: Option<Arc<Cipher>>,
}

impl PgAlertSourceStore {
    pub fn new(pool: PgPool, cipher: Option<Arc<Cipher>>) -> Self {
        Self { pool, cipher }
    }

    fn decode(&self, row: SourceRow) -> Result<AlertSource> {
        row.decode(self.cipher.as_deref())
    }
}

/// Holds a live policy of the org against a delete until `tx` ends.
async fn lock_policy(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<()> {
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM escalation_policies \
         WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL FOR SHARE",
    )
    .bind(id)
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("lock escalation policy"))?;
    found.map(|_| ()).ok_or_else(|| policy_not_found(id))
}

async fn reread(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<SourceRow> {
    sqlx::query_as(&format!(
        "SELECT {SOURCE_COLUMNS} FROM alert_sources s WHERE s.id = $1 AND s.org_id = $2"
    ))
    .bind(id)
    .bind(org.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(db("read alert source"))
}

async fn open_incidents(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT id FROM incidents \
         WHERE org_id = $1 AND alert_source_id = $2 AND origin = 'alert' AND ended_at IS NULL \
         ORDER BY id",
    )
    .bind(org.0)
    .bind(id)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("open alert incidents"))
}

/// `secret_set` because a replaced secret leaves `signed` as it was; a cleared
/// one shows as `signed` changing.
fn changes(before: &AlertSource, after: &AlertSource, secret_set: bool) -> serde_json::Value {
    let mut changed = Vec::new();
    if before.name != after.name {
        changed.push("name");
    }
    if before.grouping != after.grouping {
        changed.push("grouping");
    }
    if before.escalation_policy_id != after.escalation_policy_id {
        changed.push("escalation_policy_id");
    }
    if before.alerts != after.alerts {
        changed.push("alerts");
    }
    if before.notify_recovery != after.notify_recovery {
        changed.push("notify_recovery");
    }
    if before.renotify_interval_secs != after.renotify_interval_secs {
        changed.push("renotify_interval_secs");
    }
    if secret_set || before.signed != after.signed {
        changed.push("signing_secret");
    }
    serde_json::json!({ "source_id": after.id, "name": after.name, "changed": changed })
}

#[async_trait]
impl AlertSourceStore for PgAlertSourceStore {
    async fn list(&self, org: OrgId) -> Result<Vec<AlertSource>> {
        let rows: Vec<SourceRow> = sqlx::query_as(&format!(
            "SELECT {SOURCE_COLUMNS} FROM alert_sources s WHERE s.org_id = $1 ORDER BY s.name"
        ))
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db("list alert sources"))?;
        rows.into_iter().map(|r| self.decode(r)).collect()
    }

    async fn names(&self, org: OrgId) -> Result<Vec<SourceName>> {
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT id, name, kind FROM alert_sources WHERE org_id = $1 ORDER BY name",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db("list alert source names"))?;
        Ok(rows
            .into_iter()
            .map(|(id, name, kind)| SourceName {
                id,
                name,
                kind: AlertSourceKind::from_db(&kind),
            })
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<AlertSource>> {
        let row: Option<SourceRow> = sqlx::query_as(&format!(
            "SELECT {SOURCE_COLUMNS} FROM alert_sources s WHERE s.id = $1 AND s.org_id = $2"
        ))
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(db("get alert source"))?;
        row.map(|r| self.decode(r)).transpose()
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewAlertSource,
        source: WriteSource,
        actor: Option<UserId>,
        cap: i64,
    ) -> Result<AlertSource> {
        let name = required_name(&new.name)?;
        checked_shape(Some(new.renotify_interval_secs), Some(&new.alerts))?;
        let secret = match new.signing_secret.as_deref() {
            Some(raw) => {
                checked_secret(new.kind, raw)?;
                Some(seal(raw, self.cipher.as_deref())?)
            }
            None => None,
        };
        let minted = capability_token::mint(self.cipher.as_deref())?;
        let channels: Vec<Uuid> = new.alerts.iter().map(|b| b.channel_id).collect();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db("create alert source: begin"))?;
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(db("create alert source: account lock"))?;
        accounts::refuse_past_cap(
            &mut tx,
            account,
            &crate::storage::count_sql::alert_sources(),
            crate::domain::quota::usage_keys::ALERT_SOURCES,
            cap,
        )
        .await?;
        lock_bound_channels(&mut tx, org, &channels, ALERT_CHANNEL_FIELD).await?;
        if let Some(policy) = new.escalation_policy_id {
            lock_policy(&mut tx, org, policy).await?;
        }
        let id: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO alert_sources \
                 (org_id, name, kind, token_hash, token_enc, signing_secret_enc, grouping, \
                  escalation_policy_id, notify_recovery, renotify_interval_secs, write_source, \
                  created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12) \
             ON CONFLICT (org_id, name) DO NOTHING \
             RETURNING id",
        )
        .bind(org.0)
        .bind(&name)
        .bind(new.kind.as_db_str())
        .bind(&minted.hash)
        .bind(&minted.sealed)
        .bind(secret)
        .bind(new.grouping.as_db_str())
        .bind(new.escalation_policy_id)
        .bind(new.notify_recovery)
        .bind(new.renotify_interval_secs as i32)
        .bind(source.as_str())
        .bind(actor.map(|u| u.0))
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("create alert source"))?;
        let id = id.ok_or_else(name_taken_error)?;
        let owners = vec![id; channels.len()];
        let positions: Vec<i32> = (0..).take(channels.len()).collect();
        insert_bindings(
            &mut tx,
            org,
            BindingOwner::AlertSource,
            &owners,
            &channels,
            &positions,
        )
        .await?;
        let created = self.decode(reread(&mut tx, org, id).await?)?;
        if created.routes_to_nobody {
            return Err(no_route());
        }
        record_audit_tx(
            &mut tx,
            org,
            actor,
            "alert_source.created",
            serde_json::json!({
                "source_id": id,
                "name": name,
                "kind": new.kind.as_db_str(),
            }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(db("create alert source: commit"))?;
        Ok(created)
    }

    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: AlertSourceUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<AlertSource>> {
        let name = update.name.as_deref().map(required_name).transpose()?;
        checked_shape(update.renotify_interval_secs, update.alerts.as_ref())?;
        let channels: Option<Vec<Uuid>> = update
            .alerts
            .as_ref()
            .map(|a| a.iter().map(|b| b.channel_id).collect());
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db("update alert source: begin"))?;
        advisory_xact_lock(&mut *tx, &alert_source_lock_key(id))
            .await
            .map_err(db("update alert source: source lock"))?;
        if let Some(ids) = &channels {
            lock_bound_channels(&mut tx, org, ids, ALERT_CHANNEL_FIELD).await?;
        }
        if let Some(Some(policy)) = update.escalation_policy_id {
            lock_policy(&mut tx, org, policy).await?;
        }
        let before: Option<SourceRow> = sqlx::query_as(&format!(
            "SELECT {SOURCE_COLUMNS} FROM alert_sources s \
             WHERE s.id = $1 AND s.org_id = $2 FOR UPDATE OF s"
        ))
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("lock alert source"))?;
        let Some(before) = before.map(|r| self.decode(r)).transpose()? else {
            return Ok(None);
        };
        let secret = match update.signing_secret.as_ref() {
            None => None,
            Some(None) => Some(None),
            Some(Some(raw)) => {
                checked_secret(before.kind, raw)?;
                Some(Some(seal(raw, self.cipher.as_deref())?))
            }
        };
        if update.grouping.is_some_and(|g| g != before.grouping)
            && !open_incidents(&mut tx, org, id).await?.is_empty()
        {
            return Err(grouping_locked());
        }
        if let Some(ids) = &channels {
            set_bindings(&mut tx, org, BindingOwner::AlertSource, id, ids).await?;
        }
        sqlx::query(
            "UPDATE alert_sources SET \
                 name = COALESCE($3, name), \
                 enabled = COALESCE($4, enabled), \
                 grouping = COALESCE($5, grouping), \
                 escalation_policy_id = CASE WHEN $6 THEN $7 ELSE escalation_policy_id END, \
                 notify_recovery = COALESCE($8, notify_recovery), \
                 renotify_interval_secs = COALESCE($9, renotify_interval_secs), \
                 signing_secret_enc = CASE WHEN $10 THEN $11 ELSE signing_secret_enc END, \
                 write_source = $12, updated_by = $13, updated_at = now() \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .bind(name)
        .bind(update.enabled)
        .bind(update.grouping.map(AlertGrouping::as_db_str))
        .bind(update.escalation_policy_id.is_some())
        .bind(update.escalation_policy_id.flatten())
        .bind(update.notify_recovery)
        .bind(update.renotify_interval_secs.map(|n| n as i32))
        .bind(secret.is_some())
        .bind(secret.flatten())
        .bind(source.as_str())
        .bind(actor.map(|u| u.0))
        .execute(&mut *tx)
        .await
        .map_err(|e| match name_taken(&e) {
            true => name_taken_error(),
            false => db("update alert source")(e),
        })?;
        let after = self.decode(reread(&mut tx, org, id).await?)?;
        // Only a change to the route is refused for leaving none: a source a
        // change elsewhere left with no route must still pause and rename.
        if touches_route(&update) && after.routes_to_nobody {
            return Err(no_route());
        }
        if before.enabled != after.enabled {
            let action = match after.enabled {
                true => "alert_source.resumed",
                false => "alert_source.paused",
            };
            record_audit_tx(
                &mut tx,
                org,
                actor,
                action,
                serde_json::json!({ "source_id": id, "name": after.name }),
            )
            .await?;
        }
        let metadata = changes(
            &before,
            &after,
            matches!(update.signing_secret, Some(Some(_))),
        );
        if metadata["changed"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
        {
            record_audit_tx(&mut tx, org, actor, "alert_source.updated", metadata).await?;
        }
        tx.commit()
            .await
            .map_err(db("update alert source: commit"))?;
        Ok(Some(after))
    }

    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Option<UserId>,
    ) -> Result<Option<DeletedSource>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db("delete alert source: begin"))?;
        advisory_xact_lock(&mut *tx, &alert_source_lock_key(id))
            .await
            .map_err(db("delete alert source: source lock"))?;
        let name: Option<String> = sqlx::query_scalar(
            "SELECT name FROM alert_sources WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("lock alert source"))?;
        let Some(name) = name else {
            return Ok(None);
        };
        let closed = open_incidents(&mut tx, org, id).await?;
        let by = actor.map_or(Actor::System, Actor::User);
        for &incident in &closed {
            let outcome = resolve_in_tx(
                &mut tx,
                org,
                incident,
                by,
                format!("Closed by deleting the alert source {name}."),
            )
            .await?;
            if !matches!(outcome, LifecycleOutcome::Updated(_)) {
                return Err(AppError::Other(anyhow!(
                    "delete alert source: incident {incident} did not resolve: {outcome:?}"
                )));
            }
        }
        // Nothing was resolved, so these count toward neither MTTR nor who
        // resolved what.
        sqlx::query(
            "UPDATE incidents SET closed_by_source_delete = true \
             WHERE org_id = $1 AND id = ANY($2)",
        )
        .bind(org.0)
        .bind(&closed)
        .execute(&mut *tx)
        .await
        .map_err(db("mark closed by source delete"))?;
        sqlx::query("DELETE FROM alert_sources WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(org.0)
            .execute(&mut *tx)
            .await
            .map_err(db("delete alert source"))?;
        record_audit_tx(
            &mut tx,
            org,
            actor,
            "alert_source.deleted",
            serde_json::json!({ "source_id": id, "name": name, "incidents_closed": closed.len() }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(db("delete alert source: commit"))?;
        Ok(Some(DeletedSource { name, closed }))
    }

    async fn rotate(
        &self,
        org: OrgId,
        id: Uuid,
        revoke_previous: bool,
        actor: Option<UserId>,
    ) -> Result<Option<AlertSource>> {
        let minted = capability_token::mint(self.cipher.as_deref())?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db("rotate alert source: begin"))?;
        // Overwriting prev_* kills a twice-superseded token immediately.
        let rotated: Option<Uuid> = sqlx::query_scalar(
            "UPDATE alert_sources SET \
                 prev_token_hash         = CASE WHEN $3 THEN NULL ELSE token_hash END, \
                 prev_token_expires_at   = CASE WHEN $3 THEN NULL \
                                           ELSE now() + make_interval(secs => $4) END, \
                 prev_token_last_used_at = NULL, \
                 token_hash = $5, token_enc = $6, token_rotated_at = now() \
             WHERE id = $1 AND org_id = $2 \
             RETURNING id",
        )
        .bind(id)
        .bind(org.0)
        .bind(revoke_previous)
        .bind(PREV_TOKEN_OVERLAP.as_secs_f64())
        .bind(&minted.hash)
        .bind(&minted.sealed)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("rotate alert source"))?;
        if rotated.is_none() {
            return Ok(None);
        }
        record_audit_tx(
            &mut tx,
            org,
            actor,
            "alert_source.token_rotated",
            serde_json::json!({ "source_id": id, "revoked_previous": revoke_previous }),
        )
        .await?;
        let row = reread(&mut tx, org, id).await?;
        tx.commit()
            .await
            .map_err(db("rotate alert source: commit"))?;
        self.decode(row).map(Some)
    }

    async fn revoke_previous(&self, org: OrgId, id: Uuid, actor: Option<UserId>) -> Result<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(db("revoke previous token: begin"))?;
        let closed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE alert_sources SET \
                 prev_token_hash = NULL, prev_token_expires_at = NULL, \
                 prev_token_last_used_at = NULL \
             WHERE id = $1 AND org_id = $2 AND prev_token_expires_at > now() \
             RETURNING id",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("revoke previous token"))?;
        if closed.is_some() {
            record_audit_tx(
                &mut tx,
                org,
                actor,
                "alert_source.previous_revoked",
                serde_json::json!({ "source_id": id }),
            )
            .await?;
        }
        tx.commit()
            .await
            .map_err(db("revoke previous token: commit"))?;
        Ok(closed.is_some())
    }

    async fn find_by_token(&self, raw: &str) -> Result<Option<TokenMatch>> {
        let hash = capability_token::hash(raw);
        // The stamp is a side effect the SELECT does not see, so a previous
        // token's last use reads one request behind.
        let found: Option<TokenRow> = sqlx::query_as(&format!(
            "WITH stamp AS ( \
                 /* SAFE: the token is the credential; it names one source and its org */ \
                 UPDATE alert_sources s SET prev_token_last_used_at = now() \
                 FROM organizations o \
                 WHERE o.id = s.org_id AND o.deleted_at IS NULL \
                   AND s.prev_token_hash = $1 AND s.prev_token_expires_at > now() \
                 RETURNING s.id \
             ) \
             SELECT {SOURCE_COLUMNS}, s.token_hash <> $1 AS previous FROM alert_sources s \
             JOIN organizations org ON org.id = s.org_id AND org.deleted_at IS NULL \
             WHERE s.token_hash = $1 \
                OR (s.prev_token_hash = $1 AND s.prev_token_expires_at > now())"
        ))
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(db("find alert source by token"))?;
        let Some(TokenRow { row, previous }) = found else {
            return Ok(None);
        };
        let org = OrgId(row.org_id);
        let signing_secret = row
            .signing_secret_enc
            .as_deref()
            .and_then(|s| crate::security::open_str(s, self.cipher.as_deref()));
        Ok(Some(TokenMatch {
            org,
            source: self.decode(row)?,
            signing_secret,
            previous,
        }))
    }
}

/// In-memory double for tests run without Postgres. It knows no org default
/// policy, so a source routes only by its own policy or its channels.
#[derive(Default)]
pub struct InMemoryAlertSourceStore {
    inner: Mutex<HashMap<Uuid, MemSource>>,
}

struct MemSource {
    org: OrgId,
    source: AlertSource,
    token_hash: String,
    prev_token_hash: Option<String>,
    signing_secret: Option<String>,
}

impl InMemoryAlertSourceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn routes_to_nobody(source: &AlertSource) -> bool {
    source.escalation_policy_id.is_none() && source.alerts.is_empty()
}

#[async_trait]
impl AlertSourceStore for InMemoryAlertSourceStore {
    async fn list(&self, org: OrgId) -> Result<Vec<AlertSource>> {
        let g = self.inner.lock().expect("alert sources lock");
        let mut out: Vec<AlertSource> = g
            .values()
            .filter(|m| m.org == org)
            .map(|m| m.source.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn names(&self, org: OrgId) -> Result<Vec<SourceName>> {
        Ok(self
            .list(org)
            .await?
            .into_iter()
            .map(|s| SourceName {
                id: s.id,
                name: s.name,
                kind: s.kind,
            })
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<AlertSource>> {
        let g = self.inner.lock().expect("alert sources lock");
        Ok(g.get(&id)
            .filter(|m| m.org == org)
            .map(|m| m.source.clone()))
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewAlertSource,
        source: WriteSource,
        actor: Option<UserId>,
        cap: i64,
    ) -> Result<AlertSource> {
        let name = required_name(&new.name)?;
        checked_shape(Some(new.renotify_interval_secs), Some(&new.alerts))?;
        if let Some(raw) = new.signing_secret.as_deref() {
            checked_secret(new.kind, raw)?;
        }
        let mut g = self.inner.lock().expect("alert sources lock");
        let current = g.len() as i64;
        if current + 1 > cap {
            return Err(AppError::quota_exceeded(
                crate::domain::quota::usage_keys::ALERT_SOURCES,
                current,
                cap,
                "current",
            ));
        }
        if g.values().any(|m| m.org == org && m.source.name == name) {
            return Err(name_taken_error());
        }
        let minted = capability_token::mint(None)?;
        let now = Utc::now();
        let created = AlertSource {
            id: Uuid::now_v7(),
            name,
            kind: new.kind,
            enabled: true,
            plan_hold_at: None,
            token: Some(minted.raw),
            token_rotated_at: None,
            prev_token_expires_at: None,
            prev_token_last_used_at: None,
            signed: new.signing_secret.is_some(),
            grouping: new.grouping,
            escalation_policy_id: new.escalation_policy_id,
            alerts: new.alerts,
            notify_recovery: new.notify_recovery,
            renotify_interval_secs: new.renotify_interval_secs,
            last_event_at: None,
            routes_to_nobody: false,
            write_source: source,
            created_at: now,
            created_by: actor,
            updated_at: now,
            updated_by: actor,
        };
        if routes_to_nobody(&created) {
            return Err(no_route());
        }
        g.insert(
            created.id,
            MemSource {
                org,
                source: created.clone(),
                token_hash: minted.hash,
                prev_token_hash: None,
                signing_secret: new.signing_secret,
            },
        );
        Ok(created)
    }

    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: AlertSourceUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<AlertSource>> {
        let name = update.name.as_deref().map(required_name).transpose()?;
        checked_shape(update.renotify_interval_secs, update.alerts.as_ref())?;
        let mut g = self.inner.lock().expect("alert sources lock");
        if let Some(name) = &name
            && g.values()
                .any(|m| m.org == org && m.source.id != id && &m.source.name == name)
        {
            return Err(name_taken_error());
        }
        let Some(mem) = g.get_mut(&id).filter(|m| m.org == org) else {
            return Ok(None);
        };
        if let Some(Some(raw)) = update.signing_secret.as_ref() {
            checked_secret(mem.source.kind, raw)?;
        }
        let route_changed = touches_route(&update);
        let mut next = mem.source.clone();
        if let Some(name) = name {
            next.name = name;
        }
        if let Some(enabled) = update.enabled {
            next.enabled = enabled;
        }
        if let Some(grouping) = update.grouping {
            next.grouping = grouping;
        }
        if let Some(policy) = update.escalation_policy_id {
            next.escalation_policy_id = policy;
        }
        if let Some(alerts) = update.alerts {
            next.alerts = alerts;
        }
        if let Some(notify) = update.notify_recovery {
            next.notify_recovery = notify;
        }
        if let Some(secs) = update.renotify_interval_secs {
            next.renotify_interval_secs = secs;
        }
        if let Some(secret) = &update.signing_secret {
            next.signed = secret.is_some();
        }
        if route_changed && routes_to_nobody(&next) {
            return Err(no_route());
        }
        next.write_source = source;
        next.updated_by = actor;
        next.updated_at = Utc::now();
        if let Some(secret) = update.signing_secret {
            mem.signing_secret = secret;
        }
        mem.source = next.clone();
        Ok(Some(next))
    }

    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        _actor: Option<UserId>,
    ) -> Result<Option<DeletedSource>> {
        let mut g = self.inner.lock().expect("alert sources lock");
        if !g.get(&id).is_some_and(|m| m.org == org) {
            return Ok(None);
        }
        let mem = g.remove(&id).expect("checked above");
        Ok(Some(DeletedSource {
            name: mem.source.name,
            closed: Vec::new(),
        }))
    }

    async fn rotate(
        &self,
        org: OrgId,
        id: Uuid,
        revoke_previous: bool,
        _actor: Option<UserId>,
    ) -> Result<Option<AlertSource>> {
        let mut g = self.inner.lock().expect("alert sources lock");
        let Some(mem) = g.get_mut(&id).filter(|m| m.org == org) else {
            return Ok(None);
        };
        let minted = capability_token::mint(None)?;
        let now = Utc::now();
        let overlap = chrono::Duration::from_std(PREV_TOKEN_OVERLAP).expect("overlap fits");
        mem.prev_token_hash = (!revoke_previous).then(|| mem.token_hash.clone());
        mem.source.prev_token_expires_at = (!revoke_previous).then(|| now + overlap);
        mem.source.prev_token_last_used_at = None;
        mem.token_hash = minted.hash;
        mem.source.token = Some(minted.raw);
        mem.source.token_rotated_at = Some(now);
        Ok(Some(mem.source.clone()))
    }

    async fn revoke_previous(&self, org: OrgId, id: Uuid, _actor: Option<UserId>) -> Result<bool> {
        let mut g = self.inner.lock().expect("alert sources lock");
        let Some(mem) = g.get_mut(&id).filter(|m| m.org == org) else {
            return Ok(false);
        };
        if mem.source.open_overlap().is_none() {
            return Ok(false);
        }
        mem.prev_token_hash = None;
        mem.source.prev_token_expires_at = None;
        mem.source.prev_token_last_used_at = None;
        Ok(true)
    }

    async fn find_by_token(&self, raw: &str) -> Result<Option<TokenMatch>> {
        let hash = capability_token::hash(raw);
        let mut g = self.inner.lock().expect("alert sources lock");
        let Some(mem) = g.values_mut().find(|m| {
            m.token_hash == hash
                || (m.prev_token_hash.as_deref() == Some(&hash)
                    && m.source.open_overlap().is_some())
        }) else {
            return Ok(None);
        };
        let previous = mem.token_hash != hash;
        if previous {
            mem.source.prev_token_last_used_at = Some(Utc::now());
        }
        Ok(Some(TokenMatch {
            org: mem.org,
            source: mem.source.clone(),
            signing_secret: mem.signing_secret.clone(),
            previous,
        }))
    }
}
