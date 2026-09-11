//! The account: the subject every resource quota is counted against.
//!
//! An org is a workspace; the account owns the plan and one shared pool of
//! caps across all of its orgs. Two consequences the rest of the code leans
//! on: creating an extra org buys no extra capacity, and a membership in
//! someone else's org contributes none of the member's own capacity.
//!
//! Soft-deleted orgs are outside the pool — their monitoring is paused, so
//! they cost nothing — which is why restoring one re-checks the pool the same
//! way creating one does.

use anyhow::Context;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::domain::{AccountId, OrgId, UserId};
use crate::error::{AppError, Result};

/// SQL fragment expanding to an account's live orgs, with the account bound at
/// placeholder `param` (`"$1"`, `"$7"`, …). Every pooled count is
/// `... WHERE org_id IN (live_orgs(..))`, so the number a customer is blocked
/// at and the number the usage page shows come from one definition.
pub fn live_orgs(param: &str) -> String {
    format!(
        "SELECT /* SAFE: the account IS the tenant key of a pooled quota — this fragment exists to widen a count from one org to the account's own orgs, and the bound account comes from the caller's org */ \
         id FROM organizations WHERE account_id = {param} AND deleted_at IS NULL"
    )
}

/// The account an org belongs to. Every org has one (the column is NOT NULL),
/// so a missing row means the org itself is gone.
pub async fn account_for_org<'e, E: PgExecutor<'e>>(exec: E, org: OrgId) -> Result<AccountId> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT /* SAFE: resolves the caller's own org to its account; bound to that one org id */ \
         account_id FROM organizations WHERE id = $1",
    )
    .bind(org.0)
    .fetch_optional(exec)
    .await
    .context("account_for_org")?;
    row.map(|(id,)| AccountId(id)).ok_or_else(|| {
        AppError::not_found(crate::api::error::codes::ORG_NOT_FOUND, "org not found")
    })
}

/// Whether `user` owns the account that `org` bills to.
pub async fn pays_for_org<'e, E: PgExecutor<'e>>(
    exec: E,
    user: UserId,
    org: OrgId,
) -> Result<bool> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT /* SAFE: bound to the one org id the caller already holds; answers who pays for it */ \
         o.id FROM organizations o \
         JOIN accounts a ON a.id = o.account_id \
         WHERE o.id = $1 AND a.owner_user_id = $2",
    )
    .bind(org.0)
    .bind(user.0)
    .fetch_optional(exec)
    .await
    .context("pays_for_org")?;
    Ok(row.is_some())
}

/// The account a user owns, if they have one. A user who only ever joined
/// other people's orgs has none until they create an org of their own.
pub async fn account_for_user<'e, E: PgExecutor<'e>>(
    exec: E,
    user: UserId,
) -> Result<Option<AccountId>> {
    let row: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM accounts WHERE owner_user_id = $1")
        .bind(user.0)
        .fetch_optional(exec)
        .await
        .context("account_for_user")?;
    Ok(row.map(|(id,)| AccountId(id)))
}

/// The user's account, created on first need. `plan` seeds a fresh row only;
/// an existing account keeps the plan it already has, so this can never
/// re-grant a tier (or downgrade one) behind billing's back.
pub async fn ensure_account_for_user(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: UserId,
    plan: &str,
) -> Result<AccountId> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO accounts (owner_user_id, plan_id) VALUES ($1, $2) \
         ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL \
         DO UPDATE SET owner_user_id = EXCLUDED.owner_user_id \
         RETURNING id",
    )
    .bind(user.0)
    .bind(plan)
    .fetch_one(&mut **tx)
    .await
    .context("ensure_account_for_user")?;
    Ok(AccountId(id))
}

/// Live orgs held by the account. Matches [`live_orgs`], so the `max_orgs`
/// cap and every other pooled count agree on which orgs exist.
pub async fn live_org_count<'e, E: PgExecutor<'e>>(exec: E, account: AccountId) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT /* SAFE: counts one account's own orgs — the account is the tenant key here */ \
         count(*) FROM organizations WHERE account_id = $1 AND deleted_at IS NULL",
    )
    .bind(account.0)
    .fetch_one(exec)
    .await
    .context("live_org_count")?;
    Ok(n)
}

/// Every org the account has ever held, tombstones included: a deleted org
/// can still be restored inside its grace window, and anything cached under
/// it must not survive an account change.
pub async fn org_ids<'e, E: PgExecutor<'e>>(exec: E, account: AccountId) -> Result<Vec<OrgId>> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT /* SAFE: lists one account's own orgs — the account is the tenant key here */ \
         id FROM organizations WHERE account_id = $1",
    )
    .bind(account.0)
    .fetch_all(exec)
    .await
    .context("org_ids")?;
    Ok(rows.into_iter().map(|(id,)| OrgId(id)).collect())
}

/// One live org of the account, for resolving its plan through the per-org
/// cached path; any of them resolves the same plan. `None` when the account
/// holds no live org.
pub async fn first_live_org<'e, E: PgExecutor<'e>>(
    exec: E,
    account: AccountId,
) -> Result<Option<OrgId>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT /* SAFE: picks one of the account's own orgs — the account is the tenant key here */ \
         id FROM organizations WHERE account_id = $1 AND deleted_at IS NULL \
         ORDER BY created_at ASC, id ASC LIMIT 1",
    )
    .bind(account.0)
    .fetch_optional(exec)
    .await
    .context("first_live_org")?;
    Ok(row.map(|(id,)| OrgId(id)))
}

/// The account's plan id. Cheap enough to read inside a write transaction
/// that needs the cap without going through the cached quota service.
pub async fn plan_id_for_account<'e, E: PgExecutor<'e>>(
    exec: E,
    account: AccountId,
) -> Result<String> {
    let (plan,): (String,) = sqlx::query_as("SELECT plan_id FROM accounts WHERE id = $1")
        .bind(account.0)
        .fetch_one(exec)
        .await
        .context("plan_id_for_account")?;
    Ok(plan)
}

/// Orgs the user's account currently holds, and the `plans.max_orgs` cap they
/// are measured against — the same pair `create_org_with_owner` enforces on, so
/// the console's "2 of 3" and the create-time rejection agree. A user with no
/// account yet is measured against the default plan they would open one on.
pub async fn org_allowance_for_user(pool: &PgPool, user: UserId) -> Result<(i64, i64)> {
    let row: Option<(i64, i32)> = sqlx::query_as(
        "SELECT /* SAFE: scoped to the account this user owns, bound as $1 below */ \
         (SELECT count(*) FROM organizations o \
           WHERE o.account_id = a.id AND o.deleted_at IS NULL), \
         COALESCE((SELECT (po.override_json->>'max_orgs')::int FROM plan_overrides po \
                    WHERE po.account_id = a.id \
                      AND (po.expires_at IS NULL OR po.expires_at > now()) \
                      AND jsonb_typeof(po.override_json->'max_orgs') = 'number'), p.max_orgs) \
         FROM accounts a JOIN plans p ON p.id = a.plan_id \
         WHERE a.owner_user_id = $1",
    )
    .bind(user.0)
    .fetch_optional(pool)
    .await
    .context("org_allowance_for_user")?;
    if let Some((used, cap)) = row {
        return Ok((used, i64::from(cap)));
    }
    let cap: Option<i32> = sqlx::query_scalar("SELECT max_orgs FROM plans WHERE id = 'free'")
        .fetch_optional(pool)
        .await
        .context("org_allowance_for_user: default plan")?;
    Ok((0, i64::from(cap.unwrap_or(1))))
}

/// Drop accounts that own nothing and belong to nobody — the residue of a
/// purged owner whose orgs were purged too. Called by the retention job after
/// both cascades, never on a request path.
pub async fn reap_orphaned(pool: &PgPool) -> Result<u64> {
    let deleted = sqlx::query(
        "DELETE /* SAFE: a retention sweep, deliberately cross-tenant — it reaps only accounts that own nothing and belong to nobody, and never touches a row any tenant can still reach */ \
         FROM accounts a \
         WHERE a.owner_user_id IS NULL \
           AND NOT EXISTS (SELECT 1 FROM organizations o WHERE o.account_id = a.id)",
    )
    .execute(pool)
    .await
    .context("reap_orphaned accounts")?
    .rows_affected();
    Ok(deleted)
}
