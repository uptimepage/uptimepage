use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use crate::api::types::{TagCount, TargetsSummary};
use crate::config::PostgresConfig;
use crate::domain::target::MAX_TAGS_PER_TARGET;
use crate::domain::{
    CheckSpec, NewTarget, NewTargetWithRegions, OrgId, RegionIncidentPolicy, Target, TargetAlerts,
    TargetUpdate, UserId, WriteSource,
};
use crate::error::{AppError, Result};
use crate::quotas::service::count_sql;
use crate::security::Cipher;
use crate::storage::accounts;
use crate::storage::locks::{account_lock_key, advisory_xact_lock};
use crate::storage::postgres_secrets::{decrypt_in_place, encrypt_in_place};
use crate::storage::traits::{RegionOption, TagAddOutcome, TargetFilter, TargetSort, TargetStore};

/// Org-scoped Postgres-backed target store. Every query binds the `org`
/// passed by the caller (resolved from the request's `CurrentOrg`) so reads,
/// updates, and deletes are isolated to that organisation. The store holds no
/// ambient org of its own — a missing scope is a compile error, not a silent
/// cross-tenant read. Cross-tenant operations (e.g. scheduler-wide
/// enumeration) go through `crate::storage::admin::AdminRepo`, never through
/// this type.
pub struct PostgresTargetStore {
    pool: PgPool,
    cipher: Option<Arc<Cipher>>,
    /// Region assigned to targets created through this store, so a new target
    /// is scheduled immediately instead of waiting for boot reconciliation.
    default_region: Arc<str>,
}

/// The owner check runs on its own connection, so a membership removed between
/// it and the write lands here as a constraint violation. Same answer either
/// way rather than a 500.
fn owner_left_the_org(e: &sqlx::Error) -> bool {
    e.as_database_error().is_some_and(|d| {
        d.code().as_deref() == Some("23503") && d.constraint() == Some("targets_owner_is_member_fk")
    })
}

fn owner_not_member() -> AppError {
    AppError::bad_request_field(
        crate::api::error::codes::OWNER_NOT_MEMBER,
        "owner_user_id is not a member of this org",
        "owner_user_id",
    )
}

impl PostgresTargetStore {
    /// Open the pool and run Postgres migrations. Returns just the pool so
    /// startup can provision the default org before constructing the store.
    pub async fn connect_pool(cfg: &PostgresConfig) -> Result<PgPool> {
        tracing::info!(
            max_connections = cfg.max_connections,
            min_connections = cfg.min_connections,
            "connecting to postgres"
        );
        let pool = PgPoolOptions::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            .acquire_timeout(Duration::from_secs(cfg.acquire_timeout_secs))
            // Per-connection so self-host deploys without the tuned
            // postgresql.conf still cap leaked-transaction connection pins.
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    conn.execute("SET idle_in_transaction_session_timeout = '30s'")
                        .await?;
                    Ok(())
                })
            })
            .connect(&cfg.url)
            .await
            .context("failed to connect to postgres")?;
        tracing::info!("running postgres migrations");
        sqlx::migrate!("./migrations/postgres")
            .run(&pool)
            .await
            .context("postgres migrations")?;
        // Non-fatal: the DEFAULT partition accepts writes even if provisioning
        // fails, and the daily retention tick retries. Don't block boot on it.
        if let Err(err) = super::partitions::ensure_partitions(&pool).await {
            tracing::warn!(?err, "partition provisioning at boot failed; continuing");
        }
        assert_default_plan_present(&pool).await?;
        tracing::info!("postgres ready");
        Ok(pool)
    }

    pub fn from_pool(pool: PgPool, cipher: Option<Arc<Cipher>>) -> Self {
        Self {
            pool,
            cipher,
            default_region: Arc::from("default"),
        }
    }

    /// Set the region new targets are assigned to. Wired from
    /// `scheduler.effective_default_region()` at boot.
    pub fn with_default_region(mut self, region: impl Into<Arc<str>>) -> Self {
        self.default_region = region.into();
        self
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    fn encode_check(&self, check: &CheckSpec) -> Result<serde_json::Value> {
        let mut v = serde_json::to_value(check).context("encoding check_spec JSON")?;
        if let Some(cipher) = &self.cipher {
            encrypt_in_place(&mut v, cipher)?;
        }
        Ok(v)
    }

    fn decode_row(&self, row: TargetRow) -> Result<Target> {
        decode_target_row(row, self.cipher.as_deref())
    }

    fn rows_to_targets(&self, rows: Vec<TargetRow>) -> Result<Vec<Target>> {
        rows.into_iter().map(|r| self.decode_row(r)).collect()
    }

    /// Assign freshly-created targets to the default region within the create
    /// transaction. Ensures the region row exists first (idempotent) so a store
    /// configured with a not-yet-reconciled region can't FK-violate.
    async fn assign_default_region(
        &self,
        conn: &mut sqlx::PgConnection,
        target_ids: &[Uuid],
    ) -> Result<()> {
        let region: &str = &self.default_region;
        sqlx::query(
            "INSERT INTO regions (id, name, city) VALUES ($1, $1, '') \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(region)
        .execute(&mut *conn)
        .await
        .context("assign_default_region: ensure region")?;
        sqlx::query(
            "INSERT INTO target_regions (target_id, region) \
             SELECT unnest($1::uuid[]), $2 ON CONFLICT DO NOTHING",
        )
        .bind(target_ids)
        .bind(region)
        .execute(&mut *conn)
        .await
        .context("assign_default_region: insert assignments")?;
        Ok(())
    }
}

/// Refuse to boot if the `'free'` org default or the `'founding'` signup-cutoff
/// target is missing — either would FK-violate on the next signup. Plan-id
/// renames are blocked at the schema layer (BEFORE-UPDATE trigger on `plans`),
/// so an absent row can only come from an operator-side `DELETE`.
async fn assert_default_plan_present(pool: &PgPool) -> Result<()> {
    let (ok,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM plans WHERE id = 'free') \
         AND EXISTS (SELECT 1 FROM plans WHERE id = 'founding')",
    )
    .fetch_one(pool)
    .await
    .context("assert_default_plan_present: query")?;
    if !ok {
        return Err(AppError::Other(anyhow::anyhow!(
            "plans 'free' and/or 'founding' are missing — org signup would FK-violate. Restore them from the plans seed."
        )));
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
pub(crate) struct TargetRow {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) check_spec: serde_json::Value,
    pub(crate) interval_secs: i32,
    pub(crate) enabled: bool,
    pub(crate) tags: Vec<String>,
    pub(crate) alerts: serde_json::Value,
    pub(crate) region_policy: serde_json::Value,
    pub(crate) alert_confirmations: i32,
    pub(crate) notify_recovery: bool,
    pub(crate) renotify_interval_secs: i32,
    pub(crate) group_name: Option<String>,
    pub(crate) owner_user_id: Option<Uuid>,
    pub(crate) write_source: String,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) plan_hold_at: Option<DateTime<Utc>>,
}

/// Cap on named monitors in one audit row's metadata: past this the count
/// answers "what did they stop watching" as well as a full list would.
const AUDIT_SAMPLE: usize = 100;

fn pause_action(enabled: bool) -> &'static str {
    if enabled {
        "target.resumed"
    } else {
        "target.paused"
    }
}

/// Shared shape for a pause/resume audit row, so a bulk flip and a single one
/// read the same way to whoever is asking who stopped watching a monitor.
fn pause_metadata<'a>(flipped: impl Iterator<Item = (&'a Uuid, &'a String)>) -> serde_json::Value {
    let named: Vec<(&Uuid, &String)> = flipped.collect();
    let sample: Vec<serde_json::Value> = named
        .iter()
        .take(AUDIT_SAMPLE)
        .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
        .collect();
    serde_json::json!({
        "count": named.len(),
        "targets": sample,
        "truncated": named.len() > AUDIT_SAMPLE,
    })
}

/// Build a `Target` from a row, decrypting `check_spec` if a cipher is
/// configured. Shared between [`PostgresTargetStore`] (org-scoped reads) and
/// [`crate::storage::AdminRepo`] (cross-tenant scheduler enumeration).
pub(crate) fn decode_target_row(row: TargetRow, cipher: Option<&Cipher>) -> Result<Target> {
    let mut check_json = row.check_spec;
    if let Some(cipher) = cipher {
        decrypt_in_place(&mut check_json, cipher, row.id)?;
    }
    let check: CheckSpec =
        serde_json::from_value(check_json).context("decoding check_spec JSON")?;
    let alerts: TargetAlerts =
        serde_json::from_value(row.alerts).context("decoding alerts JSON")?;
    // Degrade a malformed/unknown policy to the default rather than failing the
    // whole row — this read feeds the cross-tenant scheduler + incident writer,
    // so one bad value must not stop scheduling for the target (or its batch).
    let region_policy = serde_json::from_value(row.region_policy).unwrap_or_else(|e| {
        tracing::warn!(target_id = %row.id, error = %e, "invalid region_policy; defaulting to majority");
        Default::default()
    });
    Ok(Target {
        id: row.id,
        name: row.name,
        check,
        interval: Duration::from_secs(row.interval_secs.max(0) as u64),
        enabled: row.enabled,
        tags: row.tags,
        alerts,
        alert_confirmations: row.alert_confirmations.max(1) as u32,
        notify_recovery: row.notify_recovery,
        renotify_interval_secs: row.renotify_interval_secs.max(0) as u32,
        region_policy,
        group_name: row.group_name,
        owner_user_id: row.owner_user_id,
        write_source: WriteSource::from_db(&row.write_source),
        created_at: row.created_at,
        updated_at: row.updated_at,
        plan_hold_at: row.plan_hold_at,
    })
}

#[async_trait]
impl TargetStore for PostgresTargetStore {
    async fn list(&self, org: OrgId, filter: TargetFilter) -> Result<Vec<Target>> {
        let limit = filter.limit.unwrap_or(100).min(10_000) as i64;
        let offset = filter.offset as i64;
        let order_clause = match filter.sort {
            TargetSort::RecentActivity => "ORDER BY updated_at DESC",
            TargetSort::Name => "ORDER BY name ASC",
            TargetSort::Created => "ORDER BY created_at ASC",
        };
        let q_like = filter
            .q
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let group = filter
            .group
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let sql = format!(
            r#"SELECT id, name, check_spec, interval_secs, enabled, tags, alerts, region_policy,
                      alert_confirmations, notify_recovery, renotify_interval_secs,
                      group_name, owner_user_id,
                      write_source,
                      created_at, updated_at, plan_hold_at
               FROM targets
               WHERE org_id = $1
                 AND ($2::bool IS NULL OR enabled = $2)
                 AND ($3::text IS NULL OR $3 = ANY(tags))
                 AND ($4::text IS NULL OR name ILIKE $4)
                 AND ($5::text IS NULL OR group_name = $5)
                 AND ($6::uuid IS NULL OR owner_user_id = $6)
                 AND ($9::text IS NULL OR kind = $9)
                 AND ($10::text IS NULL OR EXISTS (
                     SELECT 1 FROM target_regions tr
                     WHERE tr.target_id = targets.id AND tr.region = $10))
               {order_clause}
               LIMIT $7 OFFSET $8"#,
        );
        let rows: Vec<TargetRow> = sqlx::query_as::<_, TargetRow>(&sql)
            .bind(org.0)
            .bind(filter.enabled)
            .bind(filter.tag)
            .bind(q_like)
            .bind(group)
            .bind(filter.owner)
            .bind(limit)
            .bind(offset)
            .bind(filter.kind)
            .bind(filter.region)
            .fetch_all(&self.pool)
            .await
            .context("query targets")?;
        self.rows_to_targets(rows)
    }

    async fn distinct_groups(&self, org: OrgId) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT group_name FROM targets \
             WHERE org_id = $1 AND group_name IS NOT NULL AND group_name <> '' \
             ORDER BY group_name",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .context("distinct target groups")?;
        Ok(rows.into_iter().map(|(g,)| g).collect())
    }

    async fn count_by_kind(
        &self,
        org: OrgId,
        filter: TargetFilter,
    ) -> Result<std::collections::HashMap<String, i64>> {
        let q_like = filter
            .q
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{s}%"));
        let group = filter
            .group
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
            r#"SELECT kind, count(*)
               FROM targets
               WHERE org_id = $1
                 AND ($2::bool IS NULL OR enabled = $2)
                 AND ($3::text IS NULL OR $3 = ANY(tags))
                 AND ($4::text IS NULL OR name ILIKE $4)
                 AND ($5::text IS NULL OR group_name = $5)
                 AND ($6::uuid IS NULL OR owner_user_id = $6)
               GROUP BY kind"#,
        )
        .bind(org.0)
        .bind(filter.enabled)
        .bind(filter.tag)
        .bind(q_like)
        .bind(group)
        .bind(filter.owner)
        .fetch_all(&self.pool)
        .await
        .context("count targets by kind")?;
        Ok(rows
            .into_iter()
            .filter_map(|(kind, n)| kind.map(|k| (k, n)))
            .collect())
    }

    async fn names(&self, org: OrgId) -> Result<std::collections::HashMap<Uuid, String>> {
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT id, name FROM targets WHERE org_id = $1")
                .bind(org.0)
                .fetch_all(&self.pool)
                .await
                .context("query target names")?;
        Ok(rows.into_iter().collect())
    }

    async fn names_and_kinds(
        &self,
        org: OrgId,
    ) -> Result<std::collections::HashMap<Uuid, (String, String)>> {
        let rows: Vec<(Uuid, String, Option<String>)> =
            sqlx::query_as("SELECT id, name, check_spec->>'type' FROM targets WHERE org_id = $1")
                .bind(org.0)
                .fetch_all(&self.pool)
                .await
                .context("query target names + kinds")?;
        Ok(rows
            .into_iter()
            .map(|(id, name, kind)| (id, (name, kind.unwrap_or_default())))
            .collect())
    }

    async fn hosts_by_kind(&self, org: OrgId, kinds: &[&str]) -> Result<Vec<(String, String)>> {
        // tls_cert keys its subject as `host`, dns and domain_expiry as `domain`.
        let rows: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
            r#"SELECT kind, coalesce(check_spec->>'host', check_spec->>'domain')
               FROM targets
               WHERE org_id = $1 AND kind = ANY($2)"#,
        )
        .bind(org.0)
        .bind(kinds)
        .fetch_all(&self.pool)
        .await
        .context("query target hosts by kind")?;
        Ok(rows
            .into_iter()
            .filter_map(|(kind, host)| Some((kind?, host?)))
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<Target>> {
        let row: Option<TargetRow> = sqlx::query_as::<_, TargetRow>(
            r#"SELECT id, name, check_spec, interval_secs, enabled, tags, alerts, region_policy,
                      alert_confirmations, notify_recovery, renotify_interval_secs,
                      group_name, owner_user_id,
                      write_source,
                      created_at, updated_at, plan_hold_at
               FROM targets WHERE id = $1 AND org_id = $2"#,
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .context("get target by id")?;
        match row {
            Some(r) => Ok(Some(self.decode_row(r)?)),
            None => Ok(None),
        }
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewTarget,
        source: WriteSource,
        max_targets: i64,
        max_flow_checks: i64,
    ) -> Result<Target> {
        let check_json = self.encode_check(&new.check)?;
        let alerts_json = serde_json::to_value(&new.alerts).context("encoding alerts JSON")?;
        let region_policy_json = serde_json::to_value(new.region_policy.unwrap_or_default())
            .context("encoding region_policy JSON")?;
        // A per-account advisory lock held across count+INSERT in one tx. The
        // count-in-INSERT predicate alone is NOT race-safe under READ
        // COMMITTED — concurrent creators each see a snapshot count and all
        // pass `+1 <= limit`, overshooting. The subject is the account, not
        // the org: the cap is pooled, so two creates in two orgs of one
        // account have to contend.
        let mut tx = self.pool.begin().await.context("create target: begin")?;
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .context("create target: advisory lock")?;
        let (current,): (i64,) = sqlx::query_as(&count_sql::targets())
            .bind(account.0)
            .fetch_one(&mut *tx)
            .await
            .context("create target: count")?;
        if current + 1 > max_targets {
            tx.rollback().await.ok();
            return Err(AppError::quota_exceeded(
                "max_targets",
                current,
                max_targets,
                "free",
            ));
        }
        // Flow carries a tighter sub-cap, enforced under the same lock so
        // concurrent creators can't each pass a stale snapshot and overshoot it.
        if matches!(new.check, CheckSpec::Flow(_)) {
            let (flow_current,): (i64,) = sqlx::query_as(&count_sql::flow())
                .bind(account.0)
                .fetch_one(&mut *tx)
                .await
                .context("create target: flow count")?;
            if flow_current + 1 > max_flow_checks {
                tx.rollback().await.ok();
                return Err(AppError::quota_exceeded(
                    "max_flow_checks",
                    flow_current,
                    max_flow_checks,
                    "free",
                ));
            }
        }
        let row: TargetRow = sqlx::query_as::<_, TargetRow>(
            r#"INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled, tags, alerts,
                                    group_name, owner_user_id, write_source, region_policy,
                                    alert_confirmations, notify_recovery, renotify_interval_secs)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
               RETURNING id, name, check_spec, interval_secs, enabled, tags, alerts, region_policy,
                      alert_confirmations, notify_recovery, renotify_interval_secs,
                      group_name, owner_user_id,
                      write_source,
                      created_at, updated_at, plan_hold_at"#,
        )
        .bind(org.0)
        .bind(&new.name)
        .bind(check_json)
        .bind(new.interval.as_secs() as i32)
        .bind(new.enabled)
        .bind(&new.tags)
        .bind(alerts_json)
        .bind(&new.group_name)
        .bind(new.owner_user_id)
        .bind(source.as_str())
        .bind(region_policy_json)
        .bind(new.alert_confirmations.max(1) as i32)
        .bind(new.notify_recovery)
        .bind(new.renotify_interval_secs as i32)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match owner_left_the_org(&e) {
            true => owner_not_member(),
            false => anyhow::Error::new(e).context("insert target").into(),
        })?;
        self.assign_default_region(&mut tx, &[row.id]).await?;
        tx.commit().await.context("create target: commit")?;
        self.decode_row(row)
    }

    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: TargetUpdate,
        source: Option<WriteSource>,
        actor: Option<UserId>,
    ) -> Result<Option<Target>> {
        let check_json = update
            .check
            .as_ref()
            .map(|c| self.encode_check(c))
            .transpose()?;
        let interval_secs = update.interval.map(|d| d.as_secs() as i32);
        let alerts_json = update
            .alerts
            .as_ref()
            .map(|a| serde_json::to_value(a).context("encoding alerts JSON"))
            .transpose()?;
        let region_policy_json = update
            .region_policy
            .map(|p| serde_json::to_value(p).context("encoding region_policy JSON"))
            .transpose()?;

        // Same contract as `set_enabled`: an update that enables the target
        // re-arms its heartbeat in the same statement.
        let rearm_cte = if update.enabled == Some(true) {
            r#"WITH rearmed AS (
                   UPDATE heartbeat_monitors hm SET armed_at = now()
                   FROM targets t
                   WHERE hm.org_id = $12 AND hm.target_id = $1
                     AND t.id = hm.target_id AND t.enabled = false
               )
            "#
        } else {
            ""
        };
        // Only a flip of `enabled` has an audit row to bundle, so only that path
        // pays for a transaction. Every other edit stays the single statement it
        // was, which matters for the per-target writes a bulk create fans out.
        let mut tx = match update.enabled {
            Some(_) => Some(self.pool.begin().await.context("update target: begin")?),
            None => None,
        };
        // Read under a row lock, so a concurrent flip can't land between the
        // state this compares against and the write that follows.
        //
        // LOCK ORDER: this takes the `targets` row before the statement below
        // touches `heartbeat_monitors`, matching `set_enabled`. Hoisting the
        // rearm ahead of this read would invert the pair and deadlock a resume
        // racing a bulk resume of the same monitor.
        let was_enabled: Option<bool> = match tx.as_mut() {
            Some(tx) => sqlx::query_scalar(
                "SELECT enabled FROM targets WHERE id = $1 AND org_id = $2 FOR UPDATE",
            )
            .bind(id)
            .bind(org.0)
            .fetch_optional(&mut **tx)
            .await
            .context("update target: read enabled")?,
            None => None,
        };
        let sql = format!(
            r#"{rearm_cte}UPDATE targets SET
                 name = COALESCE($2, name),
                 check_spec = COALESCE($3, check_spec),
                 interval_secs = COALESCE($4, interval_secs),
                 enabled = COALESCE($5, enabled),
                 tags = COALESCE($6, tags),
                 alerts = COALESCE($7, alerts),
                 region_policy = COALESCE($14, region_policy),
                 alert_confirmations = COALESCE($15, alert_confirmations),
                 notify_recovery = COALESCE($16, notify_recovery),
                 renotify_interval_secs = COALESCE($17, renotify_interval_secs),
                 group_name = CASE WHEN $8::bool THEN $9 ELSE group_name END,
                 owner_user_id = CASE WHEN $10::bool THEN $11 ELSE owner_user_id END,
                 write_source = COALESCE($13, write_source),
                 updated_at = now()
               WHERE id = $1 AND org_id = $12
               RETURNING id, name, check_spec, interval_secs, enabled, tags, alerts, region_policy,
                      alert_confirmations, notify_recovery, renotify_interval_secs,
                      group_name, owner_user_id,
                      write_source,
                      created_at, updated_at, plan_hold_at"#,
        );
        let query = sqlx::query_as::<_, TargetRow>(&sql)
            .bind(id)
            .bind(update.name)
            .bind(check_json)
            .bind(interval_secs)
            .bind(update.enabled)
            .bind(update.tags)
            .bind(alerts_json)
            .bind(update.group_name.is_some())
            .bind(update.group_name.clone().flatten())
            .bind(update.owner_user_id.is_some())
            .bind(update.owner_user_id.flatten())
            .bind(org.0)
            .bind(source.map(WriteSource::as_str))
            .bind(region_policy_json)
            .bind(update.alert_confirmations.map(|n| n.max(1) as i32))
            .bind(update.notify_recovery)
            .bind(update.renotify_interval_secs.map(|n| n as i32));
        let row: Option<TargetRow> = match tx.as_mut() {
            Some(tx) => query.fetch_optional(&mut **tx).await,
            None => query.fetch_optional(&self.pool).await,
        }
        .map_err(|e| match owner_left_the_org(&e) {
            true => owner_not_member(),
            false => anyhow::Error::new(e).context("update target").into(),
        })?;
        if let Some(mut tx) = tx {
            // Only the enabled flip is audited here. The rest of an edit is
            // recoverable from the row itself; a paused monitor stops answering,
            // and nothing else records who stopped it.
            if let (Some(r), Some(was)) = (row.as_ref(), was_enabled)
                && r.enabled != was
            {
                crate::storage::orgs::record_audit_tx(
                    &mut tx,
                    org,
                    actor,
                    pause_action(r.enabled),
                    pause_metadata(std::iter::once((&r.id, &r.name))),
                )
                .await?;
            }
            tx.commit().await.context("update target: commit")?;
        }
        match row {
            Some(r) => Ok(Some(self.decode_row(r)?)),
            None => Ok(None),
        }
    }

    async fn delete(&self, org: OrgId, id: Uuid, actor: Option<UserId>) -> Result<bool> {
        let mut tx = self.pool.begin().await.context("delete target: begin")?;
        // RETURNING, not a prior read: after the commit there is nothing left
        // to read the name from.
        let removed: Option<(String, String)> = sqlx::query_as(
            "DELETE FROM targets WHERE id = $1 AND org_id = $2 \
             RETURNING name, check_spec->>'type'",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .context("delete target")?;
        if let Some((name, kind)) = &removed {
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                "target.deleted",
                serde_json::json!({ "target_id": id, "name": name, "kind": kind }),
            )
            .await?;
        }
        tx.commit().await.context("delete target: commit")?;
        Ok(removed.is_some())
    }

    async fn unbind_channel(&self, org: OrgId, channel_id: Uuid) -> Result<u64> {
        // No write_source stamp: this is system cleanup, not an operator edit.
        let res = sqlx::query(
            r#"UPDATE targets
               SET alerts = COALESCE(
                       (SELECT jsonb_agg(e)
                          FROM jsonb_array_elements(alerts) e
                         WHERE e->>'channel_id' IS DISTINCT FROM $2::text),
                       '[]'::jsonb),
                   updated_at = now()
               WHERE org_id = $1
                 AND alerts @> jsonb_build_array(
                         jsonb_build_object('channel_id', $2::text))"#,
        )
        .bind(org.0)
        .bind(channel_id)
        .execute(&self.pool)
        .await
        .context("unbind channel bindings")?;
        Ok(res.rows_affected())
    }

    async fn bulk_create(
        &self,
        org: OrgId,
        items: Vec<NewTargetWithRegions>,
        source: WriteSource,
        max_targets: i64,
        max_flow_checks: i64,
    ) -> Result<Vec<Target>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let flow_len = items
            .iter()
            .filter(|i| matches!(i.target.check, CheckSpec::Flow(_)))
            .count() as i64;
        let (items, region_sets): (Vec<NewTarget>, Vec<Vec<String>>) =
            items.into_iter().map(|i| (i.target, i.regions)).unzip();

        // Same lock-then-count-then-write pattern as the singular path:
        // the per-org advisory lock makes the count accurate against a
        // concurrent bulk (a count subquery alone is not race-safe under
        // READ COMMITTED). All-or-nothing on the cap.
        // Ids are minted here so each row's region set can be keyed to it
        // without leaning on RETURNING order, which Postgres does not promise.
        const SQL: &str = r#"INSERT INTO targets (id, org_id, name, check_spec, interval_secs, enabled, tags, alerts,
                                    group_name, owner_user_id, write_source,
                                    alert_confirmations, notify_recovery, renotify_interval_secs,
                                    region_policy)
               SELECT u.id, $9, u.name, u.check_spec, u.interval_secs, u.enabled,
                      ARRAY(SELECT jsonb_array_elements_text(u.tags)),
                      u.alerts,
                      u.group_name, u.owner_user_id,
                      $10,
                      u.alert_confirmations, u.notify_recovery, u.renotify_interval_secs,
                      u.region_policy
               FROM UNNEST($1::text[], $2::jsonb[], $3::int4[], $4::bool[], $5::jsonb[], $6::jsonb[],
                           $7::text[], $8::uuid[], $11::int4[], $12::bool[], $13::int4[],
                           $14::jsonb[], $15::uuid[])
                    AS u(name, check_spec, interval_secs, enabled, tags, alerts,
                         group_name, owner_user_id, alert_confirmations, notify_recovery,
                         renotify_interval_secs, region_policy, id)
               RETURNING id, name, check_spec, interval_secs, enabled, tags, alerts, region_policy,
                      alert_confirmations, notify_recovery, renotify_interval_secs,
                      group_name, owner_user_id,
                      write_source,
                      created_at, updated_at, plan_hold_at"#;

        let len = items.len();
        let mut names: Vec<String> = Vec::with_capacity(len);
        let mut intervals: Vec<i32> = Vec::with_capacity(len);
        let mut enabled: Vec<bool> = Vec::with_capacity(len);
        // Postgres rejects ragged 2-D arrays (text[][] must be rectangular), so
        // pass per-row tag lists as jsonb and unpack on the server side.
        let mut tags_json: Vec<Json<Vec<String>>> = Vec::with_capacity(len);
        let mut alerts_json: Vec<Json<TargetAlerts>> = Vec::with_capacity(len);
        let mut group_name: Vec<Option<String>> = Vec::with_capacity(len);
        let mut owner_user_id: Vec<Option<Uuid>> = Vec::with_capacity(len);
        let mut confirmations: Vec<i32> = Vec::with_capacity(len);
        let mut recoveries: Vec<bool> = Vec::with_capacity(len);
        let mut renotifies: Vec<i32> = Vec::with_capacity(len);
        let mut policies: Vec<Json<RegionIncidentPolicy>> = Vec::with_capacity(len);
        let ids: Vec<Uuid> = (0..len).map(|_| Uuid::new_v4()).collect();

        let mut tx = self.pool.begin().await.context("bulk create: begin")?;
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .context("bulk create: advisory lock")?;
        let (current,): (i64,) = sqlx::query_as(&count_sql::targets())
            .bind(account.0)
            .fetch_one(&mut *tx)
            .await
            .context("bulk create: count")?;
        if current + len as i64 > max_targets {
            tx.rollback().await.ok();
            return Err(AppError::quota_exceeded(
                "max_targets",
                current,
                max_targets,
                "free",
            ));
        }
        if flow_len > 0 {
            let (flow_current,): (i64,) = sqlx::query_as(&count_sql::flow())
                .bind(account.0)
                .fetch_one(&mut *tx)
                .await
                .context("bulk create: flow count")?;
            if flow_current + flow_len > max_flow_checks {
                tx.rollback().await.ok();
                return Err(AppError::quota_exceeded(
                    "max_flow_checks",
                    flow_current,
                    max_flow_checks,
                    "free",
                ));
            }
        }

        let rows: Vec<TargetRow> = if let Some(cipher) = &self.cipher {
            // Cipher path: walk each CheckSpec via serde_json::Value so credential
            // fields can be wrapped before binding.
            let mut check_specs: Vec<Json<serde_json::Value>> = Vec::with_capacity(len);
            for new in items {
                let mut v = serde_json::to_value(&new.check).context("encoding check_spec JSON")?;
                encrypt_in_place(&mut v, cipher)?;
                names.push(new.name);
                check_specs.push(Json(v));
                intervals.push(new.interval.as_secs() as i32);
                enabled.push(new.enabled);
                tags_json.push(Json(new.tags));
                alerts_json.push(Json(new.alerts));
                group_name.push(new.group_name);
                owner_user_id.push(new.owner_user_id);
                confirmations.push(new.alert_confirmations.max(1) as i32);
                recoveries.push(new.notify_recovery);
                renotifies.push(new.renotify_interval_secs as i32);
                policies.push(Json(new.region_policy.unwrap_or_default()));
            }
            sqlx::query_as::<_, TargetRow>(SQL)
                .bind(&names)
                .bind(&check_specs)
                .bind(&intervals)
                .bind(&enabled)
                .bind(&tags_json)
                .bind(&alerts_json)
                .bind(&group_name)
                .bind(&owner_user_id)
                .bind(org.0)
                .bind(source.as_str())
                .bind(&confirmations)
                .bind(&recoveries)
                .bind(&renotifies)
                .bind(&policies)
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await
                .context("bulk insert targets")?
        } else {
            // Plaintext path: bind CheckSpec directly so sqlx serializes straight to
            // wire bytes, skipping the intermediate Value tree.
            let mut check_specs: Vec<Json<CheckSpec>> = Vec::with_capacity(len);
            for new in items {
                names.push(new.name);
                check_specs.push(Json(new.check));
                intervals.push(new.interval.as_secs() as i32);
                enabled.push(new.enabled);
                tags_json.push(Json(new.tags));
                alerts_json.push(Json(new.alerts));
                group_name.push(new.group_name);
                owner_user_id.push(new.owner_user_id);
                confirmations.push(new.alert_confirmations.max(1) as i32);
                recoveries.push(new.notify_recovery);
                renotifies.push(new.renotify_interval_secs as i32);
                policies.push(Json(new.region_policy.unwrap_or_default()));
            }
            sqlx::query_as::<_, TargetRow>(SQL)
                .bind(&names)
                .bind(&check_specs)
                .bind(&intervals)
                .bind(&enabled)
                .bind(&tags_json)
                .bind(&alerts_json)
                .bind(&group_name)
                .bind(&owner_user_id)
                .bind(org.0)
                .bind(source.as_str())
                .bind(&confirmations)
                .bind(&recoveries)
                .bind(&renotifies)
                .bind(&policies)
                .bind(&ids)
                .fetch_all(&mut *tx)
                .await
                .context("bulk insert targets")?
        };

        let mut target_ids: Vec<Uuid> = Vec::new();
        let mut region_ids: Vec<&str> = Vec::new();
        let mut unplaced: Vec<Uuid> = Vec::new();
        for (id, regions) in ids.iter().zip(&region_sets) {
            if regions.is_empty() {
                unplaced.push(*id);
            }
            for region in regions {
                target_ids.push(*id);
                region_ids.push(region);
            }
        }
        if !target_ids.is_empty() {
            sqlx::query(
                "INSERT INTO target_regions (target_id, region) \
                 SELECT * FROM unnest($1::uuid[], $2::text[])",
            )
            .bind(&target_ids)
            .bind(&region_ids)
            .execute(&mut *tx)
            .await
            .context("bulk create: assign regions")?;
        }
        if !unplaced.is_empty() {
            self.assign_default_region(&mut tx, &unplaced).await?;
        }
        tx.commit().await.context("bulk create: commit")?;
        let position: HashMap<Uuid, usize> =
            ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut rows = rows;
        rows.sort_by_key(|r| position[&r.id]);
        self.rows_to_targets(rows)
    }

    async fn list_updated_since(&self, org: OrgId, since: DateTime<Utc>) -> Result<Vec<Target>> {
        let rows: Vec<TargetRow> = sqlx::query_as::<_, TargetRow>(
            r#"SELECT id, name, check_spec, interval_secs, enabled, tags, alerts, region_policy,
                      alert_confirmations, notify_recovery, renotify_interval_secs,
                      group_name, owner_user_id,
                      write_source,
                      created_at, updated_at, plan_hold_at
               FROM targets WHERE org_id = $1 AND updated_at > $2"#,
        )
        .bind(org.0)
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .context("query targets updated since")?;
        self.rows_to_targets(rows)
    }

    async fn list_tags(
        &self,
        org: OrgId,
        prefix: Option<String>,
        limit: usize,
    ) -> Result<Vec<TagCount>> {
        let prefix_pat = prefix.as_deref().map(|p| format!("{p}%"));
        let rows: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT tag, count(*) AS c
               FROM targets, unnest(tags) AS tag
               WHERE org_id = $1
                 AND ($2::text IS NULL OR tag LIKE $2)
               GROUP BY tag
               ORDER BY c DESC, tag ASC
               LIMIT $3"#,
        )
        .bind(org.0)
        .bind(prefix_pat)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .context("aggregate tag counts")?;
        Ok(rows
            .into_iter()
            .map(|(name, c)| TagCount {
                name,
                count: c.max(0) as u64,
            })
            .collect())
    }

    async fn summary(&self, org: OrgId) -> Result<TargetsSummary> {
        let row: (i64, i64) = sqlx::query_as(
            r#"SELECT count(*) FILTER (WHERE TRUE),
                      count(*) FILTER (WHERE enabled)
               FROM targets
               WHERE org_id = $1"#,
        )
        .bind(org.0)
        .fetch_one(&self.pool)
        .await
        .context("targets summary")?;
        let total = row.0.max(0) as u64;
        let enabled = row.1.max(0) as u64;
        Ok(TargetsSummary {
            total,
            enabled,
            disabled: total - enabled,
        })
    }

    async fn set_enabled(
        &self,
        org: OrgId,
        ids: &[Uuid],
        enabled: bool,
        actor: Option<UserId>,
    ) -> Result<Vec<Uuid>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // `prev` is the pre-update snapshot, and it does three jobs: enabling
        // re-arms heartbeat monitors off it, so only real disabled→enabled flips
        // touch; it tells the audit row which ids actually changed state; and its
        // `FOR UPDATE` serialises two concurrent flips of the same monitor, which
        // would otherwise both read the same `was` and both write an audit row
        // for one state change.
        //
        // LOCK ORDER: targets first, then `heartbeat_monitors` — `prev` is what
        // `rearmed` reads, so the dependency pins that order. `update()` takes
        // the same two locks in the same order. Reversing either invites a
        // deadlock between a single resume and a bulk one.
        let sql = if enabled {
            r#"WITH prev AS (
                   SELECT id, name, enabled FROM targets
                   WHERE id = ANY($1) AND org_id = $3
                   FOR UPDATE
               ), rearmed AS (
                   UPDATE heartbeat_monitors hm SET armed_at = now()
                   FROM prev
                   WHERE hm.org_id = $3 AND hm.target_id = prev.id
                     AND prev.enabled = false
               )
               UPDATE targets t SET enabled = $2, updated_at = now()
               FROM prev WHERE t.id = prev.id AND t.org_id = $3
               RETURNING t.id, t.name, prev.enabled AS was"#
        } else {
            r#"WITH prev AS (
                   SELECT id, name, enabled FROM targets
                   WHERE id = ANY($1) AND org_id = $3
                   FOR UPDATE
               )
               UPDATE targets t SET enabled = $2, updated_at = now()
               FROM prev WHERE t.id = prev.id AND t.org_id = $3
               RETURNING t.id, t.name, prev.enabled AS was"#
        };
        let mut tx = self.pool.begin().await.context("bulk set_enabled: begin")?;
        let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(sql)
            .bind(ids)
            .bind(enabled)
            .bind(org.0)
            .fetch_all(&mut *tx)
            .await
            .context("bulk set_enabled")?;
        // Re-pausing an already-paused monitor changed nothing, so it leaves no
        // row: an audit trail of non-events is one nobody can read.
        let flipped: Vec<&(Uuid, String, bool)> =
            rows.iter().filter(|(_, _, was)| *was != enabled).collect();
        if !flipped.is_empty() {
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                pause_action(enabled),
                pause_metadata(flipped.iter().map(|(id, name, _)| (id, name))),
            )
            .await?;
        }
        tx.commit().await.context("bulk set_enabled: commit")?;
        Ok(rows.into_iter().map(|(id, _, _)| id).collect())
    }

    async fn delete_bulk(
        &self,
        org: OrgId,
        ids: &[Uuid],
        actor: Option<UserId>,
    ) -> Result<Vec<Uuid>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await.context("bulk delete: begin")?;
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            r#"DELETE FROM targets WHERE id = ANY($1) AND org_id = $2 RETURNING id, name"#,
        )
        .bind(ids)
        .bind(org.0)
        .fetch_all(&mut *tx)
        .await
        .context("bulk delete")?;
        if !rows.is_empty() {
            // A 10 000-entry JSONB blob answers "what did they wipe" no better
            // than the count plus a readable sample.
            const SAMPLE: usize = 100;
            let sample: Vec<serde_json::Value> = rows
                .iter()
                .take(SAMPLE)
                .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
                .collect();
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                "target.bulk_deleted",
                serde_json::json!({
                    "count": rows.len(),
                    "targets": sample,
                    "truncated": rows.len() > SAMPLE,
                }),
            )
            .await?;
        }
        tx.commit().await.context("bulk delete: commit")?;
        Ok(rows.into_iter().map(|(id, _)| id).collect())
    }

    async fn add_tags(&self, org: OrgId, ids: &[Uuid], tags: &[String]) -> Result<TagAddOutcome> {
        if ids.is_empty() || tags.is_empty() {
            return Ok(TagAddOutcome::default());
        }
        // Both the merge and the cap read `t.tags` rather than a CTE snapshot:
        // under READ COMMITTED a concurrent add re-runs them against the row it
        // re-fetched, where a snapshot would write back a list missing whatever
        // the other statement just added.
        let rows: Vec<(Uuid, bool)> = sqlx::query_as(
            r#"WITH candidates AS (
                 SELECT id FROM targets WHERE id = ANY($1) AND org_id = $3
               ),
               applied AS (
                 UPDATE targets t
                 SET tags = (
                       SELECT array_agg(DISTINCT x) FROM unnest(t.tags || $2) AS x
                     ),
                     updated_at = now()
                 WHERE t.id IN (SELECT id FROM candidates)
                   AND t.org_id = $3
                   AND cardinality((
                         SELECT array_agg(DISTINCT x) FROM unnest(t.tags || $2) AS x
                       )) <= $4
                 RETURNING t.id
               )
               SELECT id, id IN (SELECT id FROM applied) AS applied FROM candidates"#,
        )
        .bind(ids)
        .bind(tags)
        .bind(org.0)
        .bind(MAX_TAGS_PER_TARGET as i32)
        .fetch_all(&self.pool)
        .await
        .context("bulk add_tags")?;
        let (updated, over_cap) = rows.into_iter().partition::<Vec<_>, _>(|(_, ok)| *ok);
        Ok(TagAddOutcome {
            updated: updated.into_iter().map(|(id, _)| id).collect(),
            over_cap: over_cap.into_iter().map(|(id, _)| id).collect(),
        })
    }

    async fn remove_tags(&self, org: OrgId, ids: &[Uuid], tags: &[String]) -> Result<Vec<Uuid>> {
        if ids.is_empty() || tags.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"UPDATE targets
               SET tags = COALESCE((
                 SELECT array_agg(t)
                 FROM unnest(tags) AS t
                 WHERE NOT (t = ANY($2))
               ), ARRAY[]::text[]),
               updated_at = now()
               WHERE id = ANY($1) AND org_id = $3
               RETURNING id"#,
        )
        .bind(ids)
        .bind(tags)
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .context("bulk remove_tags")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn set_group(&self, org: OrgId, ids: &[Uuid], group: Option<&str>) -> Result<Vec<Uuid>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"UPDATE targets SET group_name = $2, updated_at = now()
               WHERE id = ANY($1) AND org_id = $3 RETURNING id"#,
        )
        .bind(ids)
        .bind(group)
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .context("bulk set_group")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("postgres ping")?;
        Ok(())
    }

    async fn regions_for_org(&self, org: OrgId) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT tr.region \
             FROM target_regions tr JOIN targets t ON t.id = tr.target_id \
             WHERE t.org_id = $1 \
             ORDER BY tr.region",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .context("postgres regions_for_org")?;
        Ok(rows.into_iter().map(|(r,)| r).collect())
    }

    async fn available_regions(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM regions WHERE enabled ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .context("postgres available_regions")?;
        Ok(rows.into_iter().map(|(r,)| r).collect())
    }

    async fn flow_capable_regions(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT a.region FROM agents a \
             JOIN regions r ON r.id = a.region \
             WHERE a.enabled AND a.flow_capable AND r.enabled ORDER BY a.region",
        )
        .fetch_all(&self.pool)
        .await
        .context("postgres flow_capable_regions")?;
        Ok(rows.into_iter().map(|(r,)| r).collect())
    }

    async fn default_selected_regions(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM regions WHERE enabled AND default_selected ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .context("postgres default_selected_regions")?;
        Ok(rows.into_iter().map(|(r,)| r).collect())
    }

    async fn available_regions_detailed(&self) -> Result<Vec<RegionOption>> {
        let rows: Vec<(
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<f64>,
            Option<f64>,
        )> = sqlx::query_as(
            "SELECT id, name, city, country_code, continent, latitude, longitude FROM regions \
             WHERE enabled ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("postgres available_regions_detailed")?;
        Ok(rows
            .into_iter()
            .map(
                |(id, name, city, country_code, continent, latitude, longitude)| RegionOption {
                    id,
                    name,
                    city: city.unwrap_or_default(),
                    country_code,
                    continent,
                    latitude,
                    longitude,
                },
            )
            .collect())
    }

    async fn regions_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Option<Vec<String>>> {
        let owns: (bool,) =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM targets WHERE id = $1 AND org_id = $2)")
                .bind(target_id)
                .bind(org.0)
                .fetch_one(&self.pool)
                .await
                .context("postgres regions_for_target ownership")?;
        if !owns.0 {
            return Ok(None);
        }
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT region FROM target_regions WHERE target_id = $1 ORDER BY region",
        )
        .bind(target_id)
        .fetch_all(&self.pool)
        .await
        .context("postgres regions_for_target")?;
        Ok(Some(rows.into_iter().map(|(r,)| r).collect()))
    }

    async fn set_target_regions(
        &self,
        org: OrgId,
        target_id: Uuid,
        regions: &[String],
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await.context("set regions: begin")?;
        let owns: (bool,) =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM targets WHERE id = $1 AND org_id = $2)")
                .bind(target_id)
                .bind(org.0)
                .fetch_one(&mut *tx)
                .await
                .context("set regions: ownership")?;
        if !owns.0 {
            tx.rollback().await.ok();
            return Ok(false);
        }
        sqlx::query("DELETE FROM target_regions WHERE target_id = $1")
            .bind(target_id)
            .execute(&mut *tx)
            .await
            .context("set regions: clear")?;
        sqlx::query(
            "INSERT INTO target_regions (target_id, region) \
             SELECT $1, unnest($2::text[])",
        )
        .bind(target_id)
        .bind(regions)
        .execute(&mut *tx)
        .await
        .context("set regions: insert")?;
        tx.commit().await.context("set regions: commit")?;
        Ok(true)
    }
}
