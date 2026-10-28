//! Monitor groups: the row a monitor's `group_id` points at, its channels in
//! `group_channels`, and its delete, which closes the group's incident and
//! hands the members still down back to their own channels.
//!
//! LOCK ORDER: the account advisory lock, channels, groups, monitor rows,
//! incidents (each by its advisory lock, then its row; a group incident before
//! its members, members in id order).

use std::collections::{BTreeSet, HashMap};

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    ActorType, IncidentEventKind, MAX_GROUP_NAME_LEN, MonitorGroup, MonitorGroupUpdate,
    NewMonitorGroup, OrgId, TargetAlerts, UserId, WriteSource, group_deleted_message,
    normalize_group_name,
};
use crate::error::{AppError, Result, codes, validation};
use crate::storage::accounts;
use crate::storage::channel_bindings::{
    ALERT_CHANNEL_FIELD, BindingOwner, group_alerts_column, insert_bindings, lock_bound_channels,
    set_bindings,
};
use crate::storage::incident_ops::may_page_predicate;
use crate::storage::locks::{account_lock_key, advisory_xact_lock, incident_lock_key};
use crate::storage::orgs::record_audit_tx;

const GROUP_COLUMNS: &str = concat!(
    "g.id, g.name, g.alert_as_one, g.renotify_interval_secs, g.alert_again, g.tags, \
     g.escalation_policy_id, g.write_source, g.created_at, g.created_by, g.updated_at, \
     g.updated_by, ",
    group_alerts_column!("g")
);

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    name: String,
    alert_as_one: bool,
    renotify_interval_secs: i32,
    alert_again: bool,
    tags: Vec<String>,
    escalation_policy_id: Option<Uuid>,
    write_source: String,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    updated_at: DateTime<Utc>,
    updated_by: Option<Uuid>,
    alerts: serde_json::Value,
}

impl GroupRow {
    fn decode(self) -> Result<MonitorGroup> {
        let alerts: TargetAlerts = serde_json::from_value(self.alerts)
            .map_err(|e| AppError::Other(anyhow!("decoding group alerts: {e}")))?;
        Ok(MonitorGroup {
            id: self.id,
            name: self.name,
            alert_as_one: self.alert_as_one,
            alerts,
            renotify_interval_secs: self.renotify_interval_secs.max(0) as u32,
            alert_again: self.alert_again,
            tags: self.tags,
            escalation_policy_id: self.escalation_policy_id,
            write_source: WriteSource::from_db(&self.write_source),
            created_at: self.created_at,
            created_by: self.created_by.map(UserId),
            updated_at: self.updated_at,
            updated_by: self.updated_by.map(UserId),
        })
    }
}

/// What deleting a group did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedGroup {
    pub name: String,
    pub members: usize,
    /// The group incident the delete closed.
    pub closed_incident: Option<Uuid>,
    /// Member incidents still down, now paging on their own as a new episode.
    pub detached: Vec<Uuid>,
}

#[async_trait]
pub trait MonitorGroupStore: Send + Sync {
    async fn list(&self, org: OrgId) -> Result<Vec<MonitorGroup>>;
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<MonitorGroup>>;
    async fn create(
        &self,
        org: OrgId,
        new: NewMonitorGroup,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MonitorGroup>;
    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: MonitorGroupUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<MonitorGroup>>;
    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Option<UserId>,
    ) -> Result<Option<DeletedGroup>>;
}

pub struct PgMonitorGroupStore {
    pool: PgPool,
}

impl PgMonitorGroupStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn db(context: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |e| AppError::Other(anyhow!("{context}: {e}"))
}

fn name_taken(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.constraint() == Some("monitor_groups_org_id_name_key"))
}

fn name_taken_error() -> AppError {
    AppError::conflict(
        codes::GROUP_NAME_TAKEN,
        "a group with this name already exists",
    )
}

/// The stored form of a group's own name, which must name something.
fn required_name(raw: &str) -> Result<String> {
    let name = normalize_group_name(raw).ok_or_else(|| {
        AppError::bad_request_field(codes::INVALID_GROUP_NAME, "name must not be blank", "name")
    })?;
    validation::check_length(name, "name", MAX_GROUP_NAME_LEN, codes::GROUP_TOO_LONG)?;
    Ok(name.to_owned())
}

fn actor_type(actor: Option<UserId>) -> ActorType {
    match actor {
        Some(_) => ActorType::User,
        None => ActorType::System,
    }
}

/// Takes the group row that attach and close serialise on. False when the
/// group is gone.
pub(crate) async fn lock_group(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    id: Uuid,
) -> Result<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM monitor_groups WHERE id = $1 AND org_id = $2 FOR NO KEY UPDATE",
    )
    .bind(id)
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("lock group"))?;
    Ok(found.is_some())
}

/// The group a monitor write names by `name`, created as a label group when
/// none has it, and held against a rename, delete or attach until the write
/// commits. `None` for no name or a blank one.
pub(crate) async fn resolve_group(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    name: Option<&str>,
    source: WriteSource,
    actor: Option<UserId>,
) -> Result<Option<Uuid>> {
    let Some(name) = name.and_then(normalize_group_name) else {
        return Ok(None);
    };
    validation::check_length(
        name,
        "group_name",
        MAX_GROUP_NAME_LEN,
        codes::GROUP_TOO_LONG,
    )?;
    // A delete or rename committing between the two statements leaves nothing
    // under the name to lock, and the next insert can take it.
    for _ in 0..3 {
        let created: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO monitor_groups (org_id, name, write_source, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $4) ON CONFLICT (org_id, name) DO NOTHING RETURNING id",
        )
        .bind(org.0)
        .bind(name)
        .bind(source.as_str())
        .bind(actor.map(|u| u.0))
        .fetch_optional(&mut **tx)
        .await
        .map_err(db("create group"))?;
        if let Some(id) = created {
            record_audit_tx(
                tx,
                org,
                actor,
                "group.created",
                serde_json::json!({ "group_id": id, "name": name, "alert_as_one": false }),
            )
            .await?;
            return Ok(Some(id));
        }
        let found: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM monitor_groups WHERE org_id = $1 AND name = $2 FOR NO KEY UPDATE",
        )
        .bind(org.0)
        .bind(name)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db("find group"))?;
        if found.is_some() {
            return Ok(found);
        }
    }
    Err(AppError::conflict(
        codes::GROUP_CHANGE_CONFLICT,
        "the group changed while saving; try again",
    ))
}

/// [`resolve_group`] for every name of a batch, taken in name order.
pub(crate) async fn resolve_groups(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    names: &[Option<String>],
    source: WriteSource,
    actor: Option<UserId>,
) -> Result<Vec<Option<Uuid>>> {
    let wanted: BTreeSet<&str> = names
        .iter()
        .filter_map(|n| n.as_deref().and_then(normalize_group_name))
        .collect();
    let mut ids: HashMap<&str, Uuid> = HashMap::with_capacity(wanted.len());
    for name in wanted {
        if let Some(id) = resolve_group(tx, org, Some(name), source, actor).await? {
            ids.insert(name, id);
        }
    }
    Ok(names
        .iter()
        .map(|n| {
            n.as_deref()
                .and_then(normalize_group_name)
                .and_then(|n| ids.get(n).copied())
        })
        .collect())
}

#[async_trait]
impl MonitorGroupStore for PgMonitorGroupStore {
    async fn list(&self, org: OrgId) -> Result<Vec<MonitorGroup>> {
        let rows: Vec<GroupRow> = sqlx::query_as(&format!(
            "SELECT {GROUP_COLUMNS} FROM monitor_groups g WHERE g.org_id = $1 ORDER BY g.name"
        ))
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db("list groups"))?;
        rows.into_iter().map(GroupRow::decode).collect()
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<MonitorGroup>> {
        let row: Option<GroupRow> = sqlx::query_as(&format!(
            "SELECT {GROUP_COLUMNS} FROM monitor_groups g WHERE g.id = $1 AND g.org_id = $2"
        ))
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(db("get group"))?;
        row.map(GroupRow::decode).transpose()
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewMonitorGroup,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MonitorGroup> {
        let name = required_name(&new.name)?;
        let channels: Vec<Uuid> = new.alerts.iter().map(|b| b.channel_id).collect();
        let mut tx = self.pool.begin().await.map_err(db("create group: begin"))?;
        lock_bound_channels(&mut tx, org, &channels, ALERT_CHANNEL_FIELD).await?;
        let row: Option<GroupRow> = sqlx::query_as(&format!(
            "INSERT INTO monitor_groups AS g \
                 (org_id, name, alert_as_one, renotify_interval_secs, alert_again, tags, \
                  write_source, created_by, updated_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8) \
             ON CONFLICT (org_id, name) DO NOTHING \
             RETURNING {GROUP_COLUMNS}"
        ))
        .bind(org.0)
        .bind(&name)
        .bind(new.alert_as_one)
        .bind(new.renotify_interval_secs as i32)
        .bind(new.alert_again)
        .bind(&new.tags)
        .bind(source.as_str())
        .bind(actor.map(|u| u.0))
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("create group"))?;
        let row = row.ok_or_else(name_taken_error)?;
        let owners = vec![row.id; channels.len()];
        let positions: Vec<i32> = (0..).take(channels.len()).collect();
        insert_bindings(
            &mut tx,
            org,
            BindingOwner::Group,
            &owners,
            &channels,
            &positions,
        )
        .await?;
        record_audit_tx(
            &mut tx,
            org,
            actor,
            "group.created",
            serde_json::json!({
                "group_id": row.id,
                "name": name,
                "alert_as_one": new.alert_as_one,
            }),
        )
        .await?;
        tx.commit().await.map_err(db("create group: commit"))?;
        // Returned before the bindings above were written.
        let mut group = row.decode()?;
        group.alerts = new.alerts;
        Ok(group)
    }

    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: MonitorGroupUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<MonitorGroup>> {
        let name = update.name.as_deref().map(required_name).transpose()?;
        let channels: Option<Vec<Uuid>> = update
            .alerts
            .as_ref()
            .map(|a| a.iter().map(|b| b.channel_id).collect());
        let mut tx = self.pool.begin().await.map_err(db("update group: begin"))?;
        if let Some(ids) = &channels {
            lock_bound_channels(&mut tx, org, ids, ALERT_CHANNEL_FIELD).await?;
        }
        let before: Option<GroupRow> = sqlx::query_as(&format!(
            "SELECT {GROUP_COLUMNS} FROM monitor_groups g \
             WHERE g.id = $1 AND g.org_id = $2 FOR UPDATE OF g"
        ))
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("lock group"))?;
        let Some(before) = before.map(GroupRow::decode).transpose()? else {
            return Ok(None);
        };
        if let Some(ids) = &channels {
            set_bindings(&mut tx, org, BindingOwner::Group, id, ids).await?;
        }
        let row: GroupRow = sqlx::query_as(&format!(
            "UPDATE monitor_groups AS g SET \
                 name = COALESCE($3, name), \
                 alert_as_one = COALESCE($4, alert_as_one), \
                 renotify_interval_secs = COALESCE($5, renotify_interval_secs), \
                 alert_again = COALESCE($6, alert_again), \
                 tags = COALESCE($7, tags), \
                 write_source = $8, updated_by = $9, updated_at = now() \
             WHERE g.id = $1 AND g.org_id = $2 \
             RETURNING {GROUP_COLUMNS}"
        ))
        .bind(id)
        .bind(org.0)
        .bind(name)
        .bind(update.alert_as_one)
        .bind(update.renotify_interval_secs.map(|n| n as i32))
        .bind(update.alert_again)
        .bind(update.tags)
        .bind(source.as_str())
        .bind(actor.map(|u| u.0))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match name_taken(&e) {
            true => name_taken_error(),
            false => db("update group")(e),
        })?;
        let after = row.decode()?;
        record_audit_tx(
            &mut tx,
            org,
            actor,
            "group.updated",
            updated_metadata(&before, &after),
        )
        .await?;
        tx.commit().await.map_err(db("update group: commit"))?;
        Ok(Some(after))
    }

    async fn delete(
        &self,
        org: OrgId,
        id: Uuid,
        actor: Option<UserId>,
    ) -> Result<Option<DeletedGroup>> {
        let mut tx = self.pool.begin().await.map_err(db("delete group: begin"))?;
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(db("delete group: account lock"))?;
        let name: Option<String> = sqlx::query_scalar(
            "SELECT name FROM monitor_groups WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db("lock group"))?;
        let Some(name) = name else {
            return Ok(None);
        };
        // Taken now, before any incident: the delete's own set-null on them
        // would otherwise lock monitor rows after incidents.
        let members: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM targets WHERE org_id = $1 AND group_id = $2 ORDER BY id FOR UPDATE",
        )
        .bind(org.0)
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(db("lock group members"))?;
        let (closed_incident, detached) = close_group_incident(&mut tx, org, id, actor).await?;
        sqlx::query("DELETE FROM monitor_groups WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(org.0)
            .execute(&mut *tx)
            .await
            .map_err(db("delete group"))?;
        record_audit_tx(
            &mut tx,
            org,
            actor,
            "group.deleted",
            serde_json::json!({
                "group_id": id,
                "name": name,
                "members": members.len(),
                "incident_closed": closed_incident,
                "detached": detached.len(),
            }),
        )
        .await?;
        tx.commit().await.map_err(db("delete group: commit"))?;
        Ok(Some(DeletedGroup {
            name,
            members: members.len(),
            closed_incident,
            detached,
        }))
    }
}

/// Closes the group's open incident for a delete holding the group row, and
/// detaches its members still down: each becomes a new episode of its own
/// incident, owed a first page to its own channels.
async fn close_group_incident(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    group: Uuid,
    actor: Option<UserId>,
) -> Result<(Option<Uuid>, Vec<Uuid>)> {
    let open: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM incidents \
         WHERE org_id = $1 AND group_id = $2 AND origin = 'group' AND ended_at IS NULL",
    )
    .bind(org.0)
    .bind(group)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("open group incident"))?;
    let Some(incident) = open else {
        return Ok((None, Vec::new()));
    };
    advisory_xact_lock(&mut **tx, &incident_lock_key(incident))
        .await
        .map_err(db("group incident lock"))?;
    let still_open: Option<bool> = sqlx::query_scalar(
        "SELECT ended_at IS NULL FROM incidents WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(incident)
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("lock group incident"))?;
    if still_open != Some(true) {
        return Ok((None, Vec::new()));
    }

    let attached: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM incidents \
         WHERE org_id = $1 AND group_incident_id = $2 AND ended_at IS NULL ORDER BY id",
    )
    .bind(org.0)
    .bind(incident)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("group members down"))?;
    for member in &attached {
        advisory_xact_lock(&mut **tx, &incident_lock_key(*member))
            .await
            .map_err(db("member incident lock"))?;
    }
    let down: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT id, target_name FROM incidents \
         WHERE id = ANY($1) AND org_id = $2 AND group_incident_id = $3 AND ended_at IS NULL \
         ORDER BY id FOR UPDATE",
    )
    .bind(&attached)
    .bind(org.0)
    .bind(incident)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("lock group members down"))?;
    let detached: Vec<Uuid> = down.iter().map(|(id, _)| *id).collect();
    let names: Vec<String> = down.into_iter().filter_map(|(_, n)| n).collect();
    let note = group_deleted_message(&names);

    sqlx::query(&format!(
        "UPDATE incidents SET group_incident_id = NULL, routed_group_id = NULL, \
             group_member_seq = NULL, detached_at = now(), \
             page_owed_at = CASE WHEN {} THEN now() END, updated_at = now() \
         WHERE id = ANY($1) AND org_id = $2",
        may_page_predicate!("incidents"),
    ))
    .bind(&detached)
    .bind(org.0)
    .execute(&mut **tx)
    .await
    .map_err(db("detach group members"))?;
    sqlx::query(
        "INSERT INTO incident_events (org_id, incident_id, kind, actor_type, actor_id, message) \
         SELECT $1, m, $3, $4, $5, $6 FROM unnest($2::uuid[]) AS m",
    )
    .bind(org.0)
    .bind(&detached)
    .bind(IncidentEventKind::Detached.as_db_str())
    .bind(actor_type(actor).as_db_str())
    .bind(actor.map(|u| u.0))
    .bind(&note)
    .execute(&mut **tx)
    .await
    .map_err(db("log detach"))?;

    sqlx::query(
        "UPDATE incidents SET state = 'resolved', ended_at = now(), \
             duration_secs = GREATEST(0, EXTRACT(EPOCH FROM (now() - started_at))::int), \
             resolved_by = $3, resolved_at = now(), next_escalation_at = NULL, \
             recovering_since = NULL, closing_notice_at = now(), \
             closed_by_group_delete = true, updated_at = now() \
         WHERE id = $1 AND org_id = $2",
    )
    .bind(incident)
    .bind(org.0)
    .bind(actor.map(|u| u.0))
    .execute(&mut **tx)
    .await
    .map_err(db("close group incident"))?;
    sqlx::query(
        "INSERT INTO incident_events (org_id, incident_id, kind, actor_type, actor_id, message) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(org.0)
    .bind(incident)
    .bind(IncidentEventKind::GroupDeleted.as_db_str())
    .bind(actor_type(actor).as_db_str())
    .bind(actor.map(|u| u.0))
    .bind(&note)
    .execute(&mut **tx)
    .await
    .map_err(db("log group delete"))?;
    Ok((Some(incident), detached))
}

fn updated_metadata(before: &MonitorGroup, after: &MonitorGroup) -> serde_json::Value {
    let changed: Vec<&str> = [
        ("name", before.name != after.name),
        ("alert_as_one", before.alert_as_one != after.alert_as_one),
        ("alerts", before.alerts != after.alerts),
        (
            "renotify_interval_secs",
            before.renotify_interval_secs != after.renotify_interval_secs,
        ),
        ("alert_again", before.alert_again != after.alert_again),
        ("tags", before.tags != after.tags),
    ]
    .into_iter()
    .filter_map(|(field, differs)| differs.then_some(field))
    .collect();
    let mut meta = serde_json::json!({
        "group_id": after.id,
        "name": after.name,
        "changed": changed,
    });
    if before.alert_as_one != after.alert_as_one {
        meta["alert_as_one"] = serde_json::json!(after.alert_as_one);
    }
    meta
}

/// Single-tenant test double: groups by id, no members or incidents behind
/// them.
#[derive(Default)]
pub struct InMemoryMonitorGroupStore {
    groups: parking_lot::Mutex<Vec<MonitorGroup>>,
}

impl InMemoryMonitorGroupStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, group: MonitorGroup) {
        self.groups.lock().push(group);
    }
}

#[async_trait]
impl MonitorGroupStore for InMemoryMonitorGroupStore {
    async fn list(&self, _org: OrgId) -> Result<Vec<MonitorGroup>> {
        let mut groups = self.groups.lock().clone();
        groups.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(groups)
    }

    async fn get(&self, _org: OrgId, id: Uuid) -> Result<Option<MonitorGroup>> {
        Ok(self.groups.lock().iter().find(|g| g.id == id).cloned())
    }

    async fn create(
        &self,
        _org: OrgId,
        new: NewMonitorGroup,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MonitorGroup> {
        let name = required_name(&new.name)?;
        let mut groups = self.groups.lock();
        if groups.iter().any(|g| g.name == name) {
            return Err(name_taken_error());
        }
        let now = Utc::now();
        let group = MonitorGroup {
            id: Uuid::now_v7(),
            name,
            alert_as_one: new.alert_as_one,
            alerts: new.alerts,
            renotify_interval_secs: new.renotify_interval_secs,
            alert_again: new.alert_again,
            tags: new.tags,
            escalation_policy_id: None,
            write_source: source,
            created_at: now,
            created_by: actor,
            updated_at: now,
            updated_by: actor,
        };
        groups.push(group.clone());
        Ok(group)
    }

    async fn update(
        &self,
        _org: OrgId,
        id: Uuid,
        update: MonitorGroupUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<MonitorGroup>> {
        let name = update.name.as_deref().map(required_name).transpose()?;
        let mut groups = self.groups.lock();
        if let Some(name) = &name
            && groups.iter().any(|g| g.id != id && &g.name == name)
        {
            return Err(name_taken_error());
        }
        let Some(g) = groups.iter_mut().find(|g| g.id == id) else {
            return Ok(None);
        };
        if let Some(name) = name {
            g.name = name;
        }
        if let Some(v) = update.alert_as_one {
            g.alert_as_one = v;
        }
        if let Some(v) = update.alerts {
            g.alerts = v;
        }
        if let Some(v) = update.renotify_interval_secs {
            g.renotify_interval_secs = v;
        }
        if let Some(v) = update.alert_again {
            g.alert_again = v;
        }
        if let Some(v) = update.tags {
            g.tags = v;
        }
        g.write_source = source;
        g.updated_by = actor;
        g.updated_at = Utc::now();
        Ok(Some(g.clone()))
    }

    async fn delete(
        &self,
        _org: OrgId,
        id: Uuid,
        _actor: Option<UserId>,
    ) -> Result<Option<DeletedGroup>> {
        let mut groups = self.groups.lock();
        let Some(at) = groups.iter().position(|g| g.id == id) else {
            return Ok(None);
        };
        let gone = groups.remove(at);
        Ok(Some(DeletedGroup {
            name: gone.name,
            members: 0,
            closed_incident: None,
            detached: Vec::new(),
        }))
    }
}
