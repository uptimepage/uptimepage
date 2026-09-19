//! Storage for escalation policies — the per-org paging ladders and the
//! routing that binds them to a monitor (or the org default).
//!
//! Every method is org-scoped (`org: OrgId`), mirroring [`super::TargetStore`]:
//! one tenant can never read or mutate another's policies. Policies are
//! soft-deleted (`deleted_at`) so an in-flight incident's `escalation_policy_id`
//! still resolves its audit trail, while [`EscalationPolicyStore::get`] and
//! [`EscalationPolicyStore::resolve_for_target`] hide deleted rows so a removed
//! policy stops paging.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    EscalationPolicy, EscalationPolicySummary, EscalationStep, EscalationTarget,
    EscalationTargetType, NewEscalationPolicy, OrgId,
};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::storage::accounts;
use crate::storage::locks::{account_lock_key, advisory_xact_lock};

#[async_trait]
pub trait EscalationPolicyStore: Send + Sync {
    /// Lightweight index (no steps loaded), newest first.
    async fn list(&self, org: OrgId) -> Result<Vec<EscalationPolicySummary>>;
    /// Full policy with ordered steps + targets. `None` for missing/deleted.
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<EscalationPolicy>>;
    /// Create one policy with its steps. Atomically capped at `max_policies`
    /// (`ESCALATION_POLICY_QUOTA_EXCEEDED` on breach); a duplicate name yields
    /// `ESCALATION_POLICY_NAME_TAKEN`.
    async fn create(
        &self,
        org: OrgId,
        new: NewEscalationPolicy,
        max_policies: i64,
    ) -> Result<EscalationPolicy>;
    /// Replace a policy's metadata + entire step list in one transaction.
    /// `None` when the policy is missing/deleted.
    async fn replace(
        &self,
        org: OrgId,
        id: Uuid,
        new: NewEscalationPolicy,
    ) -> Result<Option<EscalationPolicy>>;
    /// Soft-delete. Returns `false` when nothing live matched.
    async fn delete(&self, org: OrgId, id: Uuid) -> Result<bool>;
    /// The live policy that governs a monitor: its own binding, else the org
    /// default, else `None`. A soft-deleted policy resolves to `None`. This is
    /// the effective policy the engine pages through.
    async fn resolve_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Option<Uuid>>;
    /// A monitor's *own* explicit binding (ignoring the org default), or `None`
    /// when it inherits. Drives the "inherit vs override" form, distinct from
    /// [`Self::resolve_for_target`].
    async fn target_policy(&self, org: OrgId, target_id: Uuid) -> Result<Option<Uuid>>;
    /// Bind (or clear) a monitor's policy. Caller validates the policy exists
    /// in the org. Returns `false` when the target does not exist in the org.
    async fn set_target_policy(
        &self,
        org: OrgId,
        target_id: Uuid,
        policy_id: Option<Uuid>,
    ) -> Result<bool>;
    /// The org's default policy id, if set and live.
    async fn org_default(&self, org: OrgId) -> Result<Option<Uuid>>;
    /// Set (or clear) the org default. Caller validates the policy.
    async fn set_org_default(&self, org: OrgId, policy_id: Option<Uuid>) -> Result<()>;
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.is_unique_violation())
}

fn name_taken() -> AppError {
    AppError::unprocessable(
        codes::ESCALATION_POLICY_NAME_TAKEN,
        "an escalation policy with this name already exists",
    )
}

// ── Postgres impl ────────────────────────────────────────────────────────

pub struct PgEscalationPolicyStore {
    pool: PgPool,
}

impl PgEscalationPolicyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct PolicyRow {
    id: Uuid,
    name: String,
    description: Option<String>,
    repeat_count: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct StepRow {
    id: Uuid,
    level: i32,
    delay_secs: i32,
}

#[derive(sqlx::FromRow)]
struct TargetRow {
    id: Uuid,
    step_id: Uuid,
    target_type: String,
    user_id: Option<Uuid>,
    schedule_id: Option<Uuid>,
    channel_id: Option<Uuid>,
}

/// Run an `EXISTS`-style `id = $1 AND org_id = $2` ownership query; map a miss
/// to an `ESCALATION_POLICY_INVALID` 422 with `msg`.
async fn assert_owned_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    sql: &str,
    id: Uuid,
    msg: &'static str,
) -> Result<()> {
    let found: Option<Uuid> = sqlx::query_scalar(sql)
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("validate escalation target: {e}")))?;
    if found.is_none() {
        return Err(AppError::unprocessable(
            codes::ESCALATION_POLICY_INVALID,
            msg,
        ));
    }
    Ok(())
}

/// Insert a policy's steps + targets inside an open transaction. Validates that
/// every target's referenced id belongs to `org` (the parent-chain org-match
/// trigger guards step↔policy, not the leaf references, so this closes the IDOR).
async fn insert_steps_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    policy_id: Uuid,
    steps: &[crate::domain::NewEscalationStep],
) -> Result<()> {
    for step in steps {
        let step_id: Uuid = sqlx::query_scalar(
            r#"INSERT INTO escalation_steps (org_id, policy_id, level, delay_secs)
               VALUES ($1, $2, $3, $4) RETURNING id"#,
        )
        .bind(org.0)
        .bind(policy_id)
        .bind(step.level)
        .bind(step.delay_secs)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::unprocessable(
                    codes::ESCALATION_POLICY_INVALID,
                    "duplicate escalation level in policy",
                )
            } else {
                AppError::Other(anyhow::anyhow!("insert escalation_step: {e}"))
            }
        })?;
        for target in &step.targets {
            // Each id type is validated against `org` so a policy cannot page a
            // foreign channel, schedule, or non-member (closes the IDOR the
            // parent-chain org-match trigger does not cover).
            if let Some(cid) = target.channel_id {
                assert_owned_tx(
                    tx,
                    org,
                    "SELECT id FROM notification_channels WHERE id = $1 AND org_id = $2",
                    cid,
                    "escalation target references an unknown notification channel",
                )
                .await?;
            }
            if let Some(uid) = target.user_id {
                assert_owned_tx(
                    tx,
                    org,
                    "SELECT user_id FROM memberships WHERE user_id = $1 AND org_id = $2",
                    uid,
                    "escalation target references a user who is not a member of this organization",
                )
                .await?;
            }
            if let Some(sid) = target.schedule_id {
                assert_owned_tx(
                    tx,
                    org,
                    "SELECT id FROM on_call_schedules \
                     WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
                    sid,
                    "escalation target references an unknown on-call schedule",
                )
                .await?;
            }
            sqlx::query(
                r#"INSERT INTO escalation_targets
                       (org_id, step_id, target_type, user_id, schedule_id, channel_id)
                   VALUES ($1, $2, $3, $4, $5, $6)"#,
            )
            .bind(org.0)
            .bind(step_id)
            .bind(target.target_type.as_db_str())
            .bind(target.user_id)
            .bind(target.schedule_id)
            .bind(target.channel_id)
            .execute(&mut **tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("insert escalation_target: {e}")))?;
        }
    }
    Ok(())
}

fn assemble(policy: PolicyRow, steps: Vec<StepRow>, targets: Vec<TargetRow>) -> EscalationPolicy {
    let mut out: Vec<EscalationStep> = steps
        .into_iter()
        .map(|s| EscalationStep {
            id: s.id,
            level: s.level,
            delay_secs: s.delay_secs,
            targets: vec![],
        })
        .collect();
    for t in targets {
        if let Some(step) = out.iter_mut().find(|s| s.id == t.step_id) {
            step.targets.push(EscalationTarget {
                id: t.id,
                target_type: EscalationTargetType::from_db_str(&t.target_type),
                user_id: t.user_id,
                schedule_id: t.schedule_id,
                channel_id: t.channel_id,
            });
        }
    }
    EscalationPolicy {
        id: policy.id,
        name: policy.name,
        description: policy.description,
        repeat_count: policy.repeat_count,
        steps: out,
        created_at: policy.created_at,
        updated_at: policy.updated_at,
    }
}

impl PgEscalationPolicyStore {
    /// Load steps + targets for a policy and assemble the full aggregate.
    /// Assumes the policy row was already fetched (and proven live in `org`).
    async fn hydrate(&self, org: OrgId, policy: PolicyRow) -> Result<EscalationPolicy> {
        let steps: Vec<StepRow> = sqlx::query_as(
            "SELECT id, level, delay_secs FROM escalation_steps \
             WHERE org_id = $1 AND policy_id = $2 ORDER BY level",
        )
        .bind(org.0)
        .bind(policy.id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("load escalation_steps: {e}")))?;
        let targets: Vec<TargetRow> = sqlx::query_as(
            "SELECT t.id, t.step_id, t.target_type, t.user_id, t.schedule_id, t.channel_id \
             FROM escalation_targets t JOIN escalation_steps s ON s.id = t.step_id \
             WHERE t.org_id = $1 AND s.policy_id = $2 ORDER BY t.created_at",
        )
        .bind(org.0)
        .bind(policy.id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("load escalation_targets: {e}")))?;
        Ok(assemble(policy, steps, targets))
    }
}

#[async_trait]
impl EscalationPolicyStore for PgEscalationPolicyStore {
    async fn list(&self, org: OrgId) -> Result<Vec<EscalationPolicySummary>> {
        let rows: Vec<(
            Uuid,
            String,
            Option<String>,
            i32,
            i64,
            DateTime<Utc>,
            DateTime<Utc>,
        )> = sqlx::query_as(
            "SELECT p.id, p.name, p.description, p.repeat_count, \
                    (SELECT count(*) FROM escalation_steps s WHERE s.policy_id = p.id), \
                    p.created_at, p.updated_at \
                 FROM escalation_policies p \
                 WHERE p.org_id = $1 AND p.deleted_at IS NULL \
                 ORDER BY p.created_at DESC",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("list escalation_policies: {e}")))?;
        Ok(rows
            .into_iter()
            .map(
                |(id, name, description, repeat_count, step_count, created_at, updated_at)| {
                    EscalationPolicySummary {
                        id,
                        name,
                        description,
                        repeat_count,
                        step_count,
                        created_at,
                        updated_at,
                    }
                },
            )
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<EscalationPolicy>> {
        let row: Option<PolicyRow> = sqlx::query_as(
            "SELECT id, name, description, repeat_count, created_at, updated_at \
             FROM escalation_policies WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("get escalation_policy: {e}")))?;
        match row {
            Some(p) => Ok(Some(self.hydrate(org, p).await?)),
            None => Ok(None),
        }
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewEscalationPolicy,
        max_policies: i64,
    ) -> Result<EscalationPolicy> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("advisory lock: {e}")))?;
        let pool_orgs = accounts::live_orgs("$6");
        let row: Option<PolicyRow> = sqlx::query_as(&format!(
            r#"INSERT INTO escalation_policies (org_id, name, description, repeat_count)
               SELECT $1, $2, $3, $4
               WHERE (SELECT count(*) FROM escalation_policies
                      WHERE org_id IN ({pool_orgs}) AND deleted_at IS NULL) < $5
               RETURNING id, name, description, repeat_count, created_at, updated_at"#
        ))
        .bind(org.0)
        .bind(&new.name)
        .bind(&new.description)
        .bind(new.repeat_count)
        .bind(max_policies)
        .bind(account.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                name_taken()
            } else {
                AppError::Other(anyhow::anyhow!("insert escalation_policy: {e}"))
            }
        })?;
        let Some(policy) = row else {
            tx.rollback().await.ok();
            return Err(AppError::unprocessable(
                codes::ESCALATION_POLICY_QUOTA_EXCEEDED,
                "escalation policy limit reached for this plan",
            ));
        };
        let id = policy.id;
        insert_steps_tx(&mut tx, org, id, &new.steps).await?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        Ok(self
            .get(org, id)
            .await?
            .expect("just-created policy is live"))
    }

    async fn replace(
        &self,
        org: OrgId,
        id: Uuid,
        new: NewEscalationPolicy,
    ) -> Result<Option<EscalationPolicy>> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        let exists: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM escalation_policies \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("lock escalation_policy: {e}")))?;
        if exists.is_none() {
            return Ok(None);
        }
        sqlx::query(
            "UPDATE escalation_policies \
             SET name = $3, description = $4, repeat_count = $5, updated_at = now() \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .bind(&new.name)
        .bind(&new.description)
        .bind(new.repeat_count)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                name_taken()
            } else {
                AppError::Other(anyhow::anyhow!("update escalation_policy: {e}"))
            }
        })?;
        // Replace the whole ladder: targets cascade with their steps.
        sqlx::query("DELETE FROM escalation_steps WHERE policy_id = $1 AND org_id = $2")
            .bind(id)
            .bind(org.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("clear escalation_steps: {e}")))?;
        insert_steps_tx(&mut tx, org, id, &new.steps).await?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        self.get(org, id).await
    }

    async fn delete(&self, org: OrgId, id: Uuid) -> Result<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("begin: {e}")))?;
        let result = sqlx::query(
            "UPDATE escalation_policies SET deleted_at = now() \
             WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("delete escalation_policy: {e}")))?;
        if result.rows_affected() == 0 {
            return Ok(false);
        }
        // Clear dangling bindings so affected monitors fall back to the org
        // default (a soft-delete leaves the FK unenforced, unlike a hard one).
        sqlx::query(
            "UPDATE targets SET escalation_policy_id = NULL \
             WHERE org_id = $2 AND escalation_policy_id = $1",
        )
        .bind(id)
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("clear target bindings: {e}")))?;
        sqlx::query(
            "UPDATE organizations SET default_escalation_policy_id = NULL \
             /* SAFE: organizations is keyed by id, which is the tenant boundary */ \
             WHERE id = $2 AND default_escalation_policy_id = $1",
        )
        .bind(id)
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("clear org default: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("commit: {e}")))?;
        Ok(true)
    }

    async fn resolve_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Option<Uuid>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT p.id FROM targets t \
             JOIN organizations o ON o.id = t.org_id \
             LEFT JOIN escalation_policies p \
                 ON p.id = COALESCE(t.escalation_policy_id, o.default_escalation_policy_id) \
                 AND p.deleted_at IS NULL \
             WHERE t.id = $1 AND t.org_id = $2",
        )
        .bind(target_id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("resolve_for_target: {e}")))?
        .flatten();
        Ok(id)
    }

    async fn target_policy(&self, org: OrgId, target_id: Uuid) -> Result<Option<Uuid>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT p.id FROM targets t \
             LEFT JOIN escalation_policies p \
                 ON p.id = t.escalation_policy_id AND p.deleted_at IS NULL \
             WHERE t.id = $1 AND t.org_id = $2",
        )
        .bind(target_id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("target_policy: {e}")))?
        .flatten();
        Ok(id)
    }

    async fn set_target_policy(
        &self,
        org: OrgId,
        target_id: Uuid,
        policy_id: Option<Uuid>,
    ) -> Result<bool> {
        // Self-guard the bound policy: clearing (NULL) is always allowed, but a
        // non-null id must be a live policy in this org. Belt-and-suspenders
        // behind the handler's validation so no caller can bind a foreign or
        // soft-deleted policy.
        let result = sqlx::query(
            "UPDATE targets SET escalation_policy_id = $3, updated_at = now() \
             WHERE id = $1 AND org_id = $2 \
                 AND ($3::uuid IS NULL OR EXISTS ( \
                     SELECT 1 FROM escalation_policies p \
                     WHERE p.id = $3 AND p.org_id = $2 AND p.deleted_at IS NULL \
                 ))",
        )
        .bind(target_id)
        .bind(org.0)
        .bind(policy_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("set_target_policy: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn org_default(&self, org: OrgId) -> Result<Option<Uuid>> {
        let id: Option<Uuid> = sqlx::query_scalar(
            "SELECT p.id FROM organizations o \
             LEFT JOIN escalation_policies p \
                 ON p.id = o.default_escalation_policy_id AND p.deleted_at IS NULL \
             /* SAFE: organizations is keyed by id, which is the tenant boundary */ \
             WHERE o.id = $1",
        )
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("org_default: {e}")))?
        .flatten();
        Ok(id)
    }

    async fn set_org_default(&self, org: OrgId, policy_id: Option<Uuid>) -> Result<()> {
        sqlx::query("UPDATE organizations SET default_escalation_policy_id = $2 /* SAFE: organizations is keyed by id, which is the tenant boundary */ WHERE id = $1")
            .bind(org.0)
            .bind(policy_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("set_org_default: {e}")))?;
        Ok(())
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────

#[derive(Default)]
pub struct InMemoryEscalationPolicyStore {
    inner: Mutex<MemState>,
}

#[derive(Default)]
struct MemState {
    policies: Vec<(OrgId, EscalationPolicy, bool)>,
    target_policy: Vec<(OrgId, Uuid, Uuid)>,
    org_default: Vec<(OrgId, Uuid)>,
}

impl InMemoryEscalationPolicyStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn materialise(
    org: OrgId,
    id: Uuid,
    new: &NewEscalationPolicy,
    now: DateTime<Utc>,
) -> EscalationPolicy {
    let _ = org;
    EscalationPolicy {
        id,
        name: new.name.clone(),
        description: new.description.clone(),
        repeat_count: new.repeat_count,
        steps: new
            .steps
            .iter()
            .map(|s| EscalationStep {
                id: Uuid::now_v7(),
                level: s.level,
                delay_secs: s.delay_secs,
                targets: s
                    .targets
                    .iter()
                    .map(|t| EscalationTarget {
                        id: Uuid::now_v7(),
                        target_type: t.target_type,
                        user_id: t.user_id,
                        schedule_id: t.schedule_id,
                        channel_id: t.channel_id,
                    })
                    .collect(),
            })
            .collect(),
        created_at: now,
        updated_at: now,
    }
}

#[async_trait]
impl EscalationPolicyStore for InMemoryEscalationPolicyStore {
    async fn list(&self, org: OrgId) -> Result<Vec<EscalationPolicySummary>> {
        Ok(self
            .inner
            .lock()
            .policies
            .iter()
            .filter(|(o, _, deleted)| *o == org && !*deleted)
            .map(|(_, p, _)| EscalationPolicySummary {
                id: p.id,
                name: p.name.clone(),
                description: p.description.clone(),
                repeat_count: p.repeat_count,
                step_count: p.steps.len() as i64,
                created_at: p.created_at,
                updated_at: p.updated_at,
            })
            .collect())
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<EscalationPolicy>> {
        Ok(self
            .inner
            .lock()
            .policies
            .iter()
            .find(|(o, p, deleted)| *o == org && p.id == id && !*deleted)
            .map(|(_, p, _)| p.clone()))
    }

    async fn create(
        &self,
        org: OrgId,
        new: NewEscalationPolicy,
        max_policies: i64,
    ) -> Result<EscalationPolicy> {
        let mut g = self.inner.lock();
        if g.policies
            .iter()
            .any(|(o, p, deleted)| *o == org && !*deleted && p.name == new.name)
        {
            return Err(name_taken());
        }
        if g.policies
            .iter()
            .filter(|(o, _, deleted)| *o == org && !*deleted)
            .count() as i64
            >= max_policies
        {
            return Err(AppError::unprocessable(
                codes::ESCALATION_POLICY_QUOTA_EXCEEDED,
                "escalation policy limit reached for this plan",
            ));
        }
        let policy = materialise(org, Uuid::now_v7(), &new, Utc::now());
        g.policies.push((org, policy.clone(), false));
        Ok(policy)
    }

    async fn replace(
        &self,
        org: OrgId,
        id: Uuid,
        new: NewEscalationPolicy,
    ) -> Result<Option<EscalationPolicy>> {
        let mut g = self.inner.lock();
        if g.policies
            .iter()
            .any(|(o, p, deleted)| *o == org && !*deleted && p.id != id && p.name == new.name)
        {
            return Err(name_taken());
        }
        let created_at = match g
            .policies
            .iter()
            .find(|(o, p, deleted)| *o == org && p.id == id && !*deleted)
        {
            Some((_, p, _)) => p.created_at,
            None => return Ok(None),
        };
        let mut policy = materialise(org, id, &new, Utc::now());
        policy.created_at = created_at;
        if let Some(slot) = g
            .policies
            .iter_mut()
            .find(|(o, p, deleted)| *o == org && p.id == id && !*deleted)
        {
            slot.1 = policy.clone();
        }
        Ok(Some(policy))
    }

    async fn delete(&self, org: OrgId, id: Uuid) -> Result<bool> {
        let mut g = self.inner.lock();
        let found = g
            .policies
            .iter_mut()
            .find(|(o, p, deleted)| *o == org && p.id == id && !*deleted);
        let Some(slot) = found else { return Ok(false) };
        slot.2 = true;
        // Clear dangling bindings so affected monitors fall back to the default.
        g.target_policy.retain(|(o, _, p)| !(*o == org && *p == id));
        g.org_default.retain(|(o, p)| !(*o == org && *p == id));
        Ok(true)
    }

    async fn resolve_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Option<Uuid>> {
        let g = self.inner.lock();
        let bound = g
            .target_policy
            .iter()
            .find(|(o, t, _)| *o == org && *t == target_id)
            .map(|(_, _, p)| *p)
            .or_else(|| {
                g.org_default
                    .iter()
                    .find(|(o, _)| *o == org)
                    .map(|(_, p)| *p)
            });
        // Hide soft-deleted/unknown policies, mirroring the SQL join.
        Ok(bound.filter(|pid| {
            g.policies
                .iter()
                .any(|(o, p, deleted)| *o == org && p.id == *pid && !*deleted)
        }))
    }

    async fn target_policy(&self, org: OrgId, target_id: Uuid) -> Result<Option<Uuid>> {
        let g = self.inner.lock();
        Ok(g.target_policy
            .iter()
            .find(|(o, t, _)| *o == org && *t == target_id)
            .map(|(_, _, p)| *p)
            .filter(|pid| {
                g.policies
                    .iter()
                    .any(|(o, p, deleted)| *o == org && p.id == *pid && !*deleted)
            }))
    }

    async fn set_target_policy(
        &self,
        org: OrgId,
        target_id: Uuid,
        policy_id: Option<Uuid>,
    ) -> Result<bool> {
        let mut g = self.inner.lock();
        g.target_policy
            .retain(|(o, t, _)| !(*o == org && *t == target_id));
        if let Some(pid) = policy_id {
            g.target_policy.push((org, target_id, pid));
        }
        Ok(true)
    }

    async fn org_default(&self, org: OrgId) -> Result<Option<Uuid>> {
        let g = self.inner.lock();
        Ok(g.org_default
            .iter()
            .find(|(o, _)| *o == org)
            .map(|(_, p)| *p)
            .filter(|pid| {
                g.policies
                    .iter()
                    .any(|(o, p, deleted)| *o == org && p.id == *pid && !*deleted)
            }))
    }

    async fn set_org_default(&self, org: OrgId, policy_id: Option<Uuid>) -> Result<()> {
        let mut g = self.inner.lock();
        g.org_default.retain(|(o, _)| *o != org);
        if let Some(pid) = policy_id {
            g.org_default.push((org, pid));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NewEscalationStep, NewEscalationTarget};

    fn org() -> OrgId {
        OrgId(Uuid::from_u128(0xA1))
    }
    fn other_org() -> OrgId {
        OrgId(Uuid::from_u128(0xB2))
    }

    fn channel_step(level: i32, delay: i32, channel: Uuid) -> NewEscalationStep {
        NewEscalationStep {
            level,
            delay_secs: delay,
            targets: vec![NewEscalationTarget {
                target_type: EscalationTargetType::Channel,
                user_id: None,
                schedule_id: None,
                channel_id: Some(channel),
            }],
        }
    }

    fn policy(name: &str, steps: Vec<NewEscalationStep>) -> NewEscalationPolicy {
        NewEscalationPolicy {
            name: name.into(),
            description: None,
            repeat_count: 0,
            steps,
        }
    }

    #[tokio::test]
    async fn create_get_list_delete_roundtrip() {
        let store = InMemoryEscalationPolicyStore::new();
        let cid = Uuid::now_v7();
        let p = store
            .create(
                org(),
                policy("primary", vec![channel_step(1, 300, cid)]),
                10,
            )
            .await
            .unwrap();
        assert_eq!(p.steps.len(), 1);
        assert_eq!(store.list(org()).await.unwrap().len(), 1);
        assert!(store.get(org(), p.id).await.unwrap().is_some());
        assert!(store.delete(org(), p.id).await.unwrap());
        assert!(store.get(org(), p.id).await.unwrap().is_none());
        assert!(store.list(org()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_enforces_cap_and_unique_name() {
        let store = InMemoryEscalationPolicyStore::new();
        store.create(org(), policy("a", vec![]), 1).await.unwrap();
        let over = store
            .create(org(), policy("b", vec![]), 1)
            .await
            .unwrap_err();
        assert!(matches!(over, AppError::Unprocessable { .. }));
        let dup = store
            .create(org(), policy("a", vec![]), 10)
            .await
            .unwrap_err();
        assert!(matches!(dup, AppError::Unprocessable { .. }));
    }

    #[tokio::test]
    async fn resolve_prefers_target_binding_then_org_default() {
        let store = InMemoryEscalationPolicyStore::new();
        let def = store
            .create(org(), policy("default", vec![]), 10)
            .await
            .unwrap();
        let bound = store
            .create(org(), policy("bound", vec![]), 10)
            .await
            .unwrap();
        let target = Uuid::now_v7();

        // No binding, no default → none.
        assert_eq!(store.resolve_for_target(org(), target).await.unwrap(), None);
        // Org default applies to every monitor without its own policy, but the
        // monitor's *own* binding stays None (the form shows "inherit").
        store.set_org_default(org(), Some(def.id)).await.unwrap();
        assert_eq!(
            store.resolve_for_target(org(), target).await.unwrap(),
            Some(def.id)
        );
        assert_eq!(store.target_policy(org(), target).await.unwrap(), None);
        // A target binding wins over the org default.
        store
            .set_target_policy(org(), target, Some(bound.id))
            .await
            .unwrap();
        assert_eq!(
            store.resolve_for_target(org(), target).await.unwrap(),
            Some(bound.id)
        );
        assert_eq!(
            store.target_policy(org(), target).await.unwrap(),
            Some(bound.id)
        );
        // Deleting the bound policy falls back to the org default.
        store.delete(org(), bound.id).await.unwrap();
        assert_eq!(
            store.resolve_for_target(org(), target).await.unwrap(),
            Some(def.id)
        );
    }

    #[tokio::test]
    async fn policies_are_isolated_per_org() {
        let store = InMemoryEscalationPolicyStore::new();
        let a = store
            .create(org(), policy("ops", vec![]), 10)
            .await
            .unwrap();
        store
            .create(other_org(), policy("ops", vec![]), 10)
            .await
            .unwrap();
        assert!(store.get(other_org(), a.id).await.unwrap().is_none());
        assert!(!store.delete(other_org(), a.id).await.unwrap());
        assert!(store.get(org(), a.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn replace_swaps_the_whole_ladder() {
        let store = InMemoryEscalationPolicyStore::new();
        let cid = Uuid::now_v7();
        let p = store
            .create(org(), policy("p", vec![channel_step(1, 300, cid)]), 10)
            .await
            .unwrap();
        let updated = store
            .replace(
                org(),
                p.id,
                policy(
                    "p",
                    vec![channel_step(1, 60, cid), channel_step(2, 120, cid)],
                ),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.steps.len(), 2);
        assert_eq!(updated.created_at, p.created_at);
    }
}
