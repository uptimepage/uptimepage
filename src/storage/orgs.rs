//! Org lifecycle and access-control helpers. The `organizations` and
//! `memberships` tables sit outside every tenant-scoped repository — every
//! `SELECT` against them lives here so the access layer for those tables has
//! exactly one owner.

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::quota::Plan;
use crate::domain::{
    Membership, OrgId, Organization, PageRef, PublicOrgBranding, PublicStyle, Role, StatusPageId,
    UserId,
};
use crate::error::{AppError, Result};
use crate::quotas::service::count_sql;
use crate::storage::accounts;
use crate::storage::locks::{account_lock_key, advisory_xact_lock, signup_lock_key, user_lock_key};

/// Returns the id of the single live (non-soft-deleted) organisation, or
/// `None` if zero or more than one exist. Used by the path-based public
/// surface to find "the lone tenant" on a self-host deploy — `Some(id)` only
/// fires when the box has exactly one tenant, so multi-tenant SaaS deploys
/// always see `None` and route via subdomain instead.
pub async fn find_lone_active_org(pool: &PgPool) -> Result<Option<OrgId>> {
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM organizations WHERE deleted_at IS NULL LIMIT 2")
            .fetch_all(pool)
            .await
            .context("find_lone_active_org")?;
    match rows.as_slice() {
        [(id,)] => Ok(Some(OrgId(*id))),
        _ => Ok(None),
    }
}

/// Returns true iff `user` is a current member of `org` *and* `org` is not
/// soft-deleted. Both filters matter:
///
///  * the membership row is the access-control check
///  * `deleted_at IS NULL` closes the "bookmark survives delete" bug — a stale
///    tab pointing at a deleted org's resources must 403/404, not 200.
pub async fn is_active_member(pool: &PgPool, user: UserId, org: OrgId) -> Result<bool> {
    let (exists,): (bool,) = sqlx::query_as(
        r#"SELECT EXISTS (
            SELECT 1 FROM memberships m
            JOIN organizations o ON o.id = m.org_id
            WHERE m.user_id = $1
              AND m.org_id = $2
              AND o.deleted_at IS NULL
        )"#,
    )
    .bind(user.0)
    .bind(org.0)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("is_active_member: {e}")))?;
    Ok(exists)
}

/// Returns the user's oldest active membership, regardless of role. Used
/// as the fallback when a session has no `active_org_id` (e.g. just-issued
/// login, cleared cookie), and as the onboarding-page anchor right after
/// signup.
///
/// Why "oldest membership, any role":
///  * For a brand-new user the signup transaction created exactly one
///    owned org, so that's what wins.
///  * For an invited-only user (no own org), their oldest invitation is
///    a meaningful landing spot — better than 403 with nowhere to go.
///  * Robust against slug rename — does not rely on inferring identity
///    from the slug's shape.
pub async fn oldest_membership_for_user<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    user: UserId,
) -> Result<Option<OrgId>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        r#"SELECT m.org_id FROM memberships m
           JOIN organizations o ON o.id = m.org_id
           WHERE m.user_id = $1 AND o.deleted_at IS NULL
           ORDER BY m.created_at ASC, m.org_id ASC
           LIMIT 1"#,
    )
    .bind(user.0)
    .fetch_optional(exec)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("oldest_membership_for_user: {e}")))?;
    Ok(row.map(|(id,)| OrgId(id)))
}

/// True if no row in `organizations` holds the slug, tombstones included —
/// the same test the create and rename paths apply, so a "check-slug" answer
/// and the write that follows it agree.
pub async fn slug_is_available(pool: &PgPool, slug: &str) -> Result<bool> {
    let (exists,): (bool,) =
        sqlx::query_as(r#"SELECT EXISTS (SELECT 1 FROM organizations WHERE slug = $1)"#)
            .bind(slug)
            .fetch_one(pool)
            .await
            .context("slug_is_available")?;
    Ok(!exists)
}

/// Atomic create: organisation row + `owner` membership for the caller, in one
/// transaction, on the caller's account. The new org shares that account's
/// plan and its single pool of caps — it buys a workspace, not capacity.
///
/// `plans.max_orgs` bounds how many the account may hold, counted under a
/// per-account advisory lock so two concurrent creates cannot both pass.
/// Soft-deleted orgs inside their grace period do not count (their monitoring
/// is paused), which is why [`restore_org`] re-checks the same cap.
///
/// Returns `Ok(None)` if any row holds the slug, a soft-deleted one included —
/// otherwise restoring it later would collide.
pub async fn create_org_with_owner(
    pool: &PgPool,
    user: UserId,
    slug: &str,
    name: &str,
) -> Result<Option<Organization>> {
    let mut tx = pool.begin().await.context("create_org_with_owner: begin")?;

    // A user who only ever joined someone else's org has no account yet; their
    // first own org opens one on the default plan.
    let account = accounts::ensure_account_for_user(&mut tx, user, DEFAULT_PLAN).await?;
    // Serialises concurrent creates on the same account. Without it the count
    // below races under READ COMMITTED — two transactions both observe
    // count < limit and both succeed, blowing past the cap. Held until commit.
    advisory_xact_lock(&mut *tx, &account_lock_key(account))
        .await
        .context("create_org_with_owner: advisory lock")?;

    let (owned, max_orgs): (i64, i32) = sqlx::query_as(ORG_ALLOWANCE_SQL)
        .bind(account.0)
        .fetch_one(&mut *tx)
        .await
        .context("create_org_with_owner: org count")?;
    let limit = i64::from(max_orgs);
    if owned >= limit {
        tx.rollback().await.ok();
        return Err(AppError::unprocessable(
            crate::api::error::codes::OWNER_ORG_LIMIT,
            format!("this account already holds the limit of {limit} organisations"),
        ));
    }

    let row: Option<OrgRow> = sqlx::query_as(
        r#"INSERT INTO organizations (slug, name, account_id)
           SELECT $1, $2, $3
           WHERE NOT EXISTS (SELECT 1 FROM organizations WHERE slug = $1)
           ON CONFLICT (slug) WHERE deleted_at IS NULL DO NOTHING
           RETURNING id, slug::text AS slug, name, created_at, updated_at, deleted_at"#,
    )
    .bind(slug)
    .bind(name)
    .bind(account.0)
    .fetch_optional(&mut *tx)
    .await
    .context("create_org_with_owner: insert organization")?;

    let Some(org_row) = row else {
        tx.rollback().await.ok();
        return Ok(None);
    };

    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user.0)
        .bind(org_row.id)
        .execute(&mut *tx)
        .await
        .context("create_org_with_owner: insert membership")?;

    record_audit_tx(
        &mut tx,
        OrgId(org_row.id),
        Some(user),
        "org.created",
        Value::Null,
    )
    .await
    .context("create_org_with_owner: audit")?;

    tx.commit().await.context("create_org_with_owner: commit")?;
    Ok(Some(org_row.into_org()))
}

/// First N accounts get the founding plan at signup; later signups fall back to
/// free. A transaction-scoped advisory lock around the count keeps the cutoff
/// exact under concurrent signups.
const FOUNDING_CUTOFF: i64 = 1000;

/// The plan an account opens on when nothing grants it a better one. Matches
/// the `accounts.plan_id` column default.
const DEFAULT_PLAN: &str = "free";

/// `(live orgs, cap)` for one account (`$1`). The cap folds an unexpired
/// `plan_overrides` row the same way `QuotaService` does, so a raised
/// allowance is honoured by the writer and not only shown in the UI.
const ORG_ALLOWANCE_SQL: &str = "SELECT \
     (SELECT count(*) FROM organizations \
       WHERE account_id = $1 AND deleted_at IS NULL), \
     COALESCE((SELECT (po.override_json->>'max_orgs')::int FROM plan_overrides po \
                WHERE po.account_id = a.id \
                  AND (po.expires_at IS NULL OR po.expires_at > now()) \
                  AND jsonb_typeof(po.override_json->'max_orgs') = 'number'), p.max_orgs) \
     FROM accounts a JOIN plans p ON p.id = a.plan_id WHERE a.id = $1";

/// First-org-at-signup creator. Opens the account (granting the founding plan
/// while the first-N promise lasts) and its first org together. Bypasses
/// [`create_org_with_owner`]'s `max_orgs` check because a brand-new account
/// needs at least one org to be usable; the cap only kicks in for additional,
/// user-named orgs created afterwards.
///
/// The slug is generated by [`generate_signup_slug`]; on the rare collision
/// the caller retries with a fresh slug.
///
/// Runs inside an existing `Transaction` so the signup tx in the OAuth
/// callback can roll back atomically on failure.
/// `generate_signup_slug` collides at p≈1e-9 per pair; 5 retries covers that
/// without spinning.
const SIGNUP_SLUG_RETRIES: u32 = 5;

/// Signup-org creation inside a caller's new-user tx. Name and slug are
/// neutral: a slug is a public host on our domain, so nothing user-supplied
/// may land there unreviewed.
pub async fn create_signup_org_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: UserId,
) -> Result<OrgId> {
    for _ in 0..SIGNUP_SLUG_RETRIES {
        let slug = crate::domain::generate_signup_slug();
        if let Some(org_id) =
            create_signup_org_with_owner_in_tx(tx, user, &slug, "My status").await?
        {
            return Ok(org_id);
        }
    }
    Err(AppError::Other(anyhow::anyhow!(
        "signup slug retries exhausted ({SIGNUP_SLUG_RETRIES})"
    )))
}

pub async fn create_signup_org_with_owner_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: UserId,
    slug: &str,
    name: &str,
) -> Result<Option<OrgId>> {
    // Serialise signups while granting the founding plan so a concurrent burst
    // cannot read the same sub-cutoff count and overshoot the first-N promise.
    advisory_xact_lock(&mut **tx, signup_lock_key())
        .await
        .context("create_signup_org_with_owner_in_tx: advisory lock")?;
    // A founding account that has since dropped all of its orgs holds no slot:
    // it is on its way out with the purge, matching every other org count.
    let (account_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO accounts (owner_user_id, plan_id) \
         SELECT $1, CASE WHEN \
           (SELECT count(*) FROM accounts a WHERE a.plan_id = 'founding' \
             AND EXISTS (SELECT 1 FROM organizations o \
                          WHERE o.account_id = a.id AND o.deleted_at IS NULL)) < $2 \
           THEN 'founding' ELSE 'free' END \
         ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL \
         DO UPDATE SET owner_user_id = EXCLUDED.owner_user_id \
         RETURNING id",
    )
    .bind(user.0)
    .bind(FOUNDING_CUTOFF)
    .fetch_one(&mut **tx)
    .await
    .context("create_signup_org_with_owner_in_tx: open account")?;

    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, $2, $3 \
         WHERE NOT EXISTS (SELECT 1 FROM organizations WHERE slug = $1) \
         ON CONFLICT (slug) WHERE deleted_at IS NULL DO NOTHING RETURNING id",
    )
    .bind(slug)
    .bind(name)
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await
    .context("create_signup_org_with_owner_in_tx: insert org")?;
    let Some((org_id,)) = row else {
        return Ok(None);
    };
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(user.0)
        .bind(org_id)
        .execute(&mut **tx)
        .await
        .context("create_signup_org_with_owner_in_tx: insert membership")?;
    record_audit_tx(
        &mut *tx,
        OrgId(org_id),
        Some(user),
        "org.created",
        Value::Null,
    )
    .await
    .context("create_signup_org_with_owner_in_tx: audit")?;
    Ok(Some(OrgId(org_id)))
}

/// Number of members in one org, for the team page's own header. The seat
/// *cap* is pooled across the account (`count_sql::members`), so this number
/// is display-only and can be smaller than what `/usage` reports.
pub async fn active_member_count(pool: &PgPool, org: OrgId) -> Result<u32> {
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM memberships WHERE org_id = $1")
        .bind(org.0)
        .fetch_one(pool)
        .await
        .context("active_member_count")?;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Find one org by id. Returns soft-deleted rows too — callers that need to
/// hide them (most user-facing GETs) check `deleted_at` themselves.
pub async fn get_org(pool: &PgPool, org: OrgId) -> Result<Option<Organization>> {
    let row: Option<OrgRow> = sqlx::query_as(
        r#"SELECT id, slug::text AS slug, name, created_at, updated_at, deleted_at
           FROM organizations WHERE id = $1"#,
    )
    .bind(org.0)
    .fetch_optional(pool)
    .await
    .context("get_org")?;
    Ok(row.map(OrgRow::into_org))
}

/// Active (non-soft-deleted) orgs the user belongs to, oldest membership first
/// so the picker has a stable order.
pub async fn list_orgs_for_user(pool: &PgPool, user: UserId) -> Result<Vec<OrgWithRole>> {
    let rows: Vec<OrgWithRoleRow> = sqlx::query_as(
        r#"SELECT o.id, o.slug::text AS slug, o.name, o.created_at, o.updated_at,
                  o.deleted_at, m.role
           FROM organizations o
           JOIN memberships m ON m.org_id = o.id
           WHERE m.user_id = $1 AND o.deleted_at IS NULL
           ORDER BY m.created_at ASC"#,
    )
    .bind(user.0)
    .fetch_all(pool)
    .await
    .context("list_orgs_for_user")?;
    Ok(rows.into_iter().map(OrgWithRoleRow::into_dto).collect())
}

/// Soft-deleted orgs the caller is recorded as having deleted, via the latest
/// `org.deleted` audit-log entry per org. Drives the "restore deleted
/// organization" UI in account settings.
pub async fn list_deleted_orgs_deleted_by(
    pool: &PgPool,
    user: UserId,
) -> Result<Vec<Organization>> {
    let rows: Vec<OrgRow> = sqlx::query_as(
        r#"SELECT o.id, o.slug::text AS slug, o.name, o.created_at, o.updated_at, o.deleted_at
           FROM organizations o
           WHERE o.deleted_at IS NOT NULL
             AND $1 = (
                 SELECT al.actor_id FROM org_audit_log al
                 WHERE al.org_id = o.id AND al.action = 'org.deleted'
                 ORDER BY al.occurred_at DESC LIMIT 1
             )
           ORDER BY o.deleted_at DESC"#,
    )
    .bind(user.0)
    .fetch_all(pool)
    .await
    .context("list_deleted_orgs_deleted_by")?;
    Ok(rows.into_iter().map(OrgRow::into_org).collect())
}

/// True iff `user` has the `owner` role on `org` (regardless of soft-delete
/// state). Used by routes that operate on soft-deleted orgs (e.g. restore),
/// where [`is_active_member`]'s `deleted_at IS NULL` filter would mask the row.
pub async fn is_owner(pool: &PgPool, user: UserId, org: OrgId) -> Result<bool> {
    let (exists,): (bool,) = sqlx::query_as(
        r#"SELECT EXISTS (
            SELECT 1 FROM memberships
            WHERE user_id = $1 AND org_id = $2 AND role = 'owner'
        )"#,
    )
    .bind(user.0)
    .bind(org.0)
    .fetch_one(pool)
    .await
    .context("is_owner")?;
    Ok(exists)
}

/// Owner user-ids of `org` (regardless of soft-delete state), ordered by id so
/// callers taking a per-owner lock acquire in a deadlock-free order. Generic
/// over the executor so it can run inside a caller's transaction.
pub async fn owner_user_ids<'c, E>(executor: E, org: OrgId) -> Result<Vec<UserId>>
where
    E: sqlx::PgExecutor<'c>,
{
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        r#"SELECT user_id FROM memberships
           WHERE org_id = $1 AND role = 'owner'
           ORDER BY user_id"#,
    )
    .bind(org.0)
    .fetch_all(executor)
    .await
    .context("owner_user_ids")?;
    Ok(rows.into_iter().map(|(id,)| UserId(id)).collect())
}

/// Combined access-control resolution for routes gated on "active owner of
/// this org". One round-trip returns enough to map to 404 (no membership /
/// soft-deleted org) or 403 (member but not owner) without two separate
/// queries. The split helpers ([`is_active_member`] / [`is_owner`]) stay
/// available for paths that need only one signal.
pub async fn membership_status(
    pool: &PgPool,
    user: UserId,
    org: OrgId,
) -> Result<MembershipStatus> {
    let row: Option<(String, bool)> = sqlx::query_as(
        r#"SELECT m.role, o.deleted_at IS NULL AS active
           FROM memberships m
           JOIN organizations o ON o.id = m.org_id
           WHERE m.user_id = $1 AND m.org_id = $2"#,
    )
    .bind(user.0)
    .bind(org.0)
    .fetch_optional(pool)
    .await
    .context("membership_status")?;
    Ok(match row {
        None => MembershipStatus::None,
        Some((_, false)) => MembershipStatus::None,
        Some((role, true)) => match role.as_str() {
            "owner" => MembershipStatus::Owner,
            _ => MembershipStatus::Member,
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipStatus {
    Owner,
    Member,
    /// No membership row, or the org is soft-deleted. Cloak as 404.
    None,
}

/// Atomic, partial update of mutable org fields. Either or both of `new_name`
/// / `new_slug` may be `None`; the caller must supply at least one (an
/// all-`None` call is a logic error and rejected with a debug assertion).
///
/// Returns:
///  * [`UpdateOrgOutcome::Updated`] with the post-update row — including the
///    no-op case where the new value(s) equal the current ones. No audit row
///    is written for a no-op so audit history stays meaningful.
///  * [`UpdateOrgOutcome::NotFound`] if the org is missing or soft-deleted.
///  * [`UpdateOrgOutcome::SlugTaken`] when `new_slug` collides with another
///    row (including soft-deleted: slugs stay reserved through the grace
///    window — the unique index doesn't exclude tombstones).
///
/// One transaction covers the row lock, the write, and one audit row per
/// *actually-changed* field, so a combined `{name, slug}` PATCH can't leave
/// the org half-updated. Sub-second-level row lock via `FOR UPDATE` in the
/// CTE serialises concurrent renames of the same org.
pub async fn update_org_fields(
    pool: &PgPool,
    org: OrgId,
    actor: UserId,
    new_name: Option<&str>,
    new_slug: Option<&str>,
) -> Result<UpdateOrgOutcome> {
    debug_assert!(
        new_name.is_some() || new_slug.is_some(),
        "update_org_fields called with no mutable field; handler should reject empty PATCH"
    );

    let mut tx = pool.begin().await.context("update_org_fields: begin")?;

    // A tombstoned holder sits outside the partial index, so the rename would
    // go through and then break that org's restore. Both facts in one probe,
    // or a missing org would report SlugTaken instead of NotFound.
    if let Some(slug) = new_slug {
        let (live, held): (bool, bool) = sqlx::query_as(
            r#"SELECT
                 EXISTS (SELECT 1 FROM organizations WHERE id = $2 AND deleted_at IS NULL),
                 EXISTS (SELECT 1 FROM organizations WHERE slug = $1 AND id <> $2)"#,
        )
        .bind(slug)
        .bind(org.0)
        .fetch_one(&mut *tx)
        .await
        .context("update_org_fields: slug holder probe")?;
        if !live {
            tx.rollback().await.ok();
            return Ok(UpdateOrgOutcome::NotFound);
        }
        if held {
            tx.rollback().await.ok();
            return Ok(UpdateOrgOutcome::SlugTaken);
        }
    }

    // One CTE+UPDATE: lock the row, read the prior (slug, name), write only
    // the supplied fields (COALESCE keeps untouched columns), and return both
    // pre- and post- values. `RETURNING` only fires when a row was updated,
    // so missing/soft-deleted orgs yield `None` here → NotFound without a
    // second round-trip.
    let res: std::result::Result<Option<UpdateRow>, sqlx::Error> = sqlx::query_as(
        r#"
        WITH prev AS (
            SELECT id, slug::text AS slug, name FROM organizations
            WHERE id = $1 AND deleted_at IS NULL
            FOR UPDATE
        )
        UPDATE organizations o
           SET name       = COALESCE($2, o.name),
               slug       = COALESCE($3::citext, o.slug),
               updated_at = now()
          FROM prev
         WHERE o.id = prev.id
        RETURNING o.id,
                  o.slug::text AS slug,
                  o.name,
                  o.created_at,
                  o.updated_at,
                  o.deleted_at,
                  prev.slug AS prev_slug,
                  prev.name AS prev_name
        "#,
    )
    .bind(org.0)
    .bind(new_name)
    .bind(new_slug)
    .fetch_optional(&mut *tx)
    .await;
    let row = match res {
        Ok(Some(r)) => r,
        Ok(None) => {
            tx.rollback().await.ok();
            return Ok(UpdateOrgOutcome::NotFound);
        }
        Err(e) if is_unique_violation(&e) => {
            tx.rollback().await.ok();
            return Ok(UpdateOrgOutcome::SlugTaken);
        }
        Err(e) => {
            return Err(AppError::Other(
                anyhow::Error::from(e).context("update_org_fields: update"),
            ));
        }
    };

    let slug_changed = row.slug != row.prev_slug;
    let name_changed = row.name != row.prev_name;
    if slug_changed {
        record_audit_tx(
            &mut tx,
            org,
            Some(actor),
            "org.slug_changed",
            serde_json::json!({ "from": row.prev_slug, "to": row.slug }),
        )
        .await
        .context("update_org_fields: audit slug")?;
    }
    if name_changed {
        record_audit_tx(
            &mut tx,
            org,
            Some(actor),
            "org.renamed",
            serde_json::json!({ "name": row.name }),
        )
        .await
        .context("update_org_fields: audit name")?;
    }
    tx.commit().await.context("update_org_fields: commit")?;
    Ok(UpdateOrgOutcome::Updated(row.into_org()))
}

#[derive(Debug, Clone)]
pub enum UpdateOrgOutcome {
    Updated(Organization),
    NotFound,
    SlugTaken,
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e.as_database_error(), Some(db) if db.code().as_deref() == Some("23505"))
}

#[derive(sqlx::FromRow)]
struct UpdateRow {
    id: Uuid,
    slug: String,
    name: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    prev_slug: String,
    prev_name: String,
}

impl UpdateRow {
    fn into_org(self) -> Organization {
        Organization {
            id: OrgId(self.id),
            slug: self.slug,
            name: self.name,
            created_at: self.created_at,
            updated_at: self.updated_at,
            deleted_at: self.deleted_at,
        }
    }
}

/// Soft-delete an org. Audit row shares the transaction so the "who deleted
/// this" lookup in [`list_deleted_orgs_deleted_by`] is authoritative.
///
/// Guard-free, and with no production caller left — it exists so a test can
/// tombstone an account's final org. Real deletes take
/// [`soft_delete_org_for_user`].
pub async fn soft_delete_org(pool: &PgPool, org: OrgId, actor: UserId) -> Result<bool> {
    let mut tx = pool.begin().await.context("soft_delete_org: begin")?;
    let deleted = soft_delete_org_tx(&mut tx, org, actor).await?;
    if !deleted {
        tx.rollback().await.ok();
        return Ok(false);
    }
    tx.commit().await.context("soft_delete_org: commit")?;
    Ok(true)
}

/// Owner-initiated delete: refuses the actor's last active org, so an account
/// is never left with nothing to sign in to. Holds the per-user lock, or two
/// concurrent deletes each see a sibling and both commit.
pub async fn soft_delete_org_for_user(
    pool: &PgPool,
    org: OrgId,
    actor: UserId,
) -> Result<DeleteOutcome> {
    let mut tx = pool
        .begin()
        .await
        .context("soft_delete_org_for_user: begin")?;

    advisory_xact_lock(&mut *tx, &user_lock_key(actor))
        .await
        .context("soft_delete_org_for_user: advisory lock")?;

    let (has_sibling,): (bool,) = sqlx::query_as(
        r#"SELECT EXISTS (
            SELECT 1 FROM memberships m
            JOIN organizations o ON o.id = m.org_id
            WHERE m.user_id = $1 AND m.org_id <> $2 AND o.deleted_at IS NULL
        )"#,
    )
    .bind(actor.0)
    .bind(org.0)
    .fetch_one(&mut *tx)
    .await
    .context("soft_delete_org_for_user: sibling probe")?;
    if !has_sibling {
        tx.rollback().await.ok();
        return Ok(DeleteOutcome::LastOrg);
    }

    let deleted = soft_delete_org_tx(&mut tx, org, actor).await?;
    if !deleted {
        tx.rollback().await.ok();
        return Ok(DeleteOutcome::NotFound);
    }
    tx.commit()
        .await
        .context("soft_delete_org_for_user: commit")?;
    Ok(DeleteOutcome::Deleted)
}

/// `false` when the row was already deleted or never existed — the caller
/// decides whether that is a no-op or a 404.
async fn soft_delete_org_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    actor: UserId,
) -> Result<bool> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        r#"UPDATE organizations
           SET deleted_at = now(), updated_at = now()
           WHERE id = $1 AND deleted_at IS NULL
           RETURNING id"#,
    )
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .context("soft_delete_org: update")?;
    if row.is_none() {
        return Ok(false);
    }

    // A session left pinned to the tombstone fails `is_active_member`, so
    // `CurrentOrg` rejects every request with no way back through the UI.
    // Tiebreak matches `oldest_membership_for_user` so a session and a fresh
    // sign-in never disagree; NULL when nothing is left, which `restore_org`
    // adopts back.
    sqlx::query(
        r#"UPDATE sessions s
           SET active_org_id = (
               SELECT m.org_id FROM memberships m
               JOIN organizations o ON o.id = m.org_id
               WHERE m.user_id = s.user_id AND o.deleted_at IS NULL
               ORDER BY m.created_at ASC, m.org_id ASC
               LIMIT 1
           )
           WHERE s.active_org_id = $1 AND s.expires_at > now()"#,
    )
    .bind(org.0)
    .execute(&mut **tx)
    .await
    .context("soft_delete_org: repoint sessions")?;

    record_audit_tx(tx, org, Some(actor), "org.deleted", Value::Null)
        .await
        .context("soft_delete_org: audit")?;
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    /// Already tombstoned, or never existed.
    NotFound,
    /// The actor's only surviving org — deleting it would strand the account.
    LastOrg,
}

/// Clear `deleted_at` on a soft-deleted org, but only if:
///  * the caller is the user who deleted it (latest `org.deleted` audit entry
///    actor matches `actor`), and
///  * it's still inside the `grace_days` window.
///
/// One UPDATE does the eligibility check + the write atomically; a follow-up
/// SELECT only fires when nothing was updated, to distinguish the three "no"
/// cases for the caller. Returns the restored row from the same transaction
/// so handlers don't re-fetch and race with concurrent mutations.
/// `plan` is the account's effective plan (overrides and add-ons already
/// folded in) — the caller resolves it through `QuotaService` and hands the
/// same numbers the create paths enforce at.
pub async fn restore_org(
    pool: &PgPool,
    org: OrgId,
    actor: UserId,
    grace_days: u32,
    plan: &Plan,
) -> Result<RestoreOutcome> {
    let mut tx = pool.begin().await.context("restore_org: begin")?;

    let account = accounts::account_for_org(&mut *tx, org).await?;
    advisory_xact_lock(&mut *tx, &account_lock_key(account))
        .await
        .context("restore_org: advisory lock")?;

    // A tombstone frees an org slot, so a restore has to re-earn it — else
    // delete, create a replacement, restore lands the account above the cap.
    let (owned, max_orgs): (i64, i32) = sqlx::query_as(ORG_ALLOWANCE_SQL)
        .bind(account.0)
        .fetch_one(&mut *tx)
        .await
        .context("restore_org: org count")?;
    if owned >= i64::from(max_orgs) {
        tx.rollback().await.ok();
        return Ok(RestoreOutcome::OwnerLimit);
    }

    let res: Result<Option<OrgRow>, sqlx::Error> = sqlx::query_as(
        r#"UPDATE organizations
           SET deleted_at = NULL, updated_at = now()
           WHERE id = $1
             AND deleted_at IS NOT NULL
             AND deleted_at > now() - ($2::int * INTERVAL '1 day')
             AND $3 = (
                 SELECT al.actor_id FROM org_audit_log al
                 WHERE al.org_id = $1 AND al.action = 'org.deleted'
                 ORDER BY al.occurred_at DESC LIMIT 1
             )
           RETURNING id, slug::text AS slug, name, created_at, updated_at, deleted_at"#,
    )
    .bind(org.0)
    .bind(i64::from(grace_days))
    .bind(actor.0)
    .fetch_optional(&mut *tx)
    .await;
    // Reachable only for a row that predates the create/rename slug guards —
    // those hold the slug for the whole window, so nothing can take it out
    // from under a restorable org. The partial index would not have stopped it.
    let updated = match res {
        Ok(row) => row,
        Err(e) if is_unique_violation(&e) => {
            tx.rollback().await.ok();
            return Ok(RestoreOutcome::SlugTaken);
        }
        Err(e) => {
            return Err(AppError::Other(
                anyhow::Error::from(e).context("restore_org: update"),
            ));
        }
    };

    if let Some(row) = updated {
        // The tombstone is lifted, so the org's monitors, pages, channels and
        // seats are back in the account's pool — count them here, inside the
        // same transaction and under the same account lock the create paths
        // take, and undo the restore if any cap is now breached. Checking
        // `max_orgs` alone would let a customer park 50 monitors in a deleted
        // org, refill the pool elsewhere, and restore for double the plan.
        let counts: PooledCounts = sqlx::query_as(&pooled_counts_sql())
            .bind(account.0)
            .fetch_one(&mut *tx)
            .await
            .context("restore_org: pooled counts")?;
        if let Some((quota, current, limit)) = counts.first_breach(plan) {
            tx.rollback().await.ok();
            return Ok(RestoreOutcome::QuotaExceeded {
                quota,
                current,
                limit,
            });
        }
        record_audit_tx(&mut tx, org, Some(actor), "org.restored", Value::Null)
            .await
            .context("restore_org: audit")?;
        // Re-arm heartbeats: their anchor froze at deletion, so the first
        // post-restore evaluation would otherwise open a false incident.
        sqlx::query("UPDATE heartbeat_monitors SET armed_at = now() WHERE org_id = $1")
            .bind(org.0)
            .execute(&mut *tx)
            .await
            .context("restore_org: re-arm heartbeats")?;
        // Members with no other org were NULLed by the delete; without this
        // they keep failing `CurrentOrg` until they sign in again.
        sqlx::query(
            r#"UPDATE sessions s
               SET active_org_id = $1
               WHERE s.active_org_id IS NULL
                 AND s.expires_at > now()
                 AND EXISTS (
                     SELECT 1 FROM memberships m
                     WHERE m.org_id = $1 AND m.user_id = s.user_id
                 )"#,
        )
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .context("restore_org: adopt orphaned sessions")?;
        tx.commit().await.context("restore_org: commit")?;
        return Ok(RestoreOutcome::Restored(row.into_org()));
    }

    // Update was a no-op; figure out which precondition failed for a clean
    // status code. This runs at most once per failed restore — the happy
    // path is one round-trip.
    let row: Option<(Option<DateTime<Utc>>, Option<Uuid>)> = sqlx::query_as(
        r#"SELECT o.deleted_at,
                  (SELECT al.actor_id FROM org_audit_log al
                   WHERE al.org_id = o.id AND al.action = 'org.deleted'
                   ORDER BY al.occurred_at DESC LIMIT 1) AS last_deleter
           FROM organizations o WHERE o.id = $1"#,
    )
    .bind(org.0)
    .fetch_optional(&mut *tx)
    .await
    .context("restore_org: diagnose")?;
    tx.rollback().await.ok();
    Ok(match row {
        None => RestoreOutcome::NotFound,
        Some((None, _)) => RestoreOutcome::NotDeleted,
        Some((Some(deleted_at), _))
            if Utc::now().signed_duration_since(deleted_at).num_days() >= i64::from(grace_days) =>
        {
            RestoreOutcome::WindowExpired
        }
        // Soft-deleted, in window, but caller isn't the deleter → cloak as
        // NotFound so non-deleters never confirm existence.
        Some((Some(_), _)) => RestoreOutcome::NotFound,
    })
}

#[derive(Debug, Clone)]
pub enum RestoreOutcome {
    Restored(Organization),
    NotFound,
    NotDeleted,
    WindowExpired,
    /// Another org took the slug while this one was tombstoned.
    SlugTaken,
    /// Restoring would put the caller over the owned-org cap.
    OwnerLimit,
    /// The org's own rows would put the account over a pooled cap. A tombstone
    /// frees its monitors, pages and seats from the pool, so a restore has to
    /// re-earn all of them, not just the org slot.
    QuotaExceeded {
        quota: &'static str,
        current: i64,
        limit: i64,
    },
}

/// The pooled counts an org brings back with it, in the order they are
/// checked. Built by lifting the tombstone inside the transaction and counting
/// through the same `count_sql` the create paths use, so "what restore
/// enforces" and "what create enforces" cannot drift.
#[derive(sqlx::FromRow)]
struct PooledCounts {
    targets: i64,
    flow: i64,
    status_pages: i64,
    public_components: i64,
    notification_channels: i64,
    escalation_policies: i64,
    on_call_schedules: i64,
    maintenance_windows: i64,
    members: i64,
}

impl PooledCounts {
    /// The first cap this breaches, if any.
    fn first_breach(&self, plan: &Plan) -> Option<(&'static str, i64, i64)> {
        use crate::quotas::service::usage_keys as k;
        [
            (k::TARGETS, self.targets, plan.max_targets),
            (k::FLOW_CHECKS, self.flow, plan.max_flow_checks),
            (k::STATUS_PAGES, self.status_pages, plan.max_status_pages),
            (
                k::PUBLIC_COMPONENTS,
                self.public_components,
                plan.max_public_components,
            ),
            (
                k::NOTIFICATION_CHANNELS,
                self.notification_channels,
                plan.max_notification_channels,
            ),
            (
                k::ESCALATION_POLICIES,
                self.escalation_policies,
                plan.max_escalation_policies,
            ),
            (
                k::ON_CALL_SCHEDULES,
                self.on_call_schedules,
                plan.max_on_call_schedules,
            ),
            (
                k::MAINTENANCE_WINDOWS,
                self.maintenance_windows,
                plan.max_maintenance_windows,
            ),
            (k::MEMBERS, self.members, plan.max_members),
        ]
        .into_iter()
        .map(|(name, current, limit)| (name, current, i64::from(limit)))
        .find(|(_, current, limit)| current > limit)
    }
}

/// Pooled counts for one account (`$1`), assembled from the same per-quota
/// queries every other enforcement site runs.
fn pooled_counts_sql() -> String {
    use crate::quotas::service::count_sql as c;
    format!(
        "SELECT ({}) AS targets, ({}) AS flow, ({}) AS status_pages, \
         ({}) AS public_components, ({}) AS notification_channels, \
         ({}) AS escalation_policies, ({}) AS on_call_schedules, \
         ({}) AS maintenance_windows, ({}) AS members",
        c::targets(),
        c::flow(),
        c::status_pages(),
        c::public_components(),
        c::notification_channels(),
        c::escalation_policies(),
        c::on_call_schedules(),
        c::maintenance_windows(),
        c::members(),
    )
}

/// List of (membership, optional display email) for every member of an org,
/// ordered by membership creation time. The email lookup goes through
/// `users`, which is fine because membership and user tables are co-located.
pub async fn list_members(pool: &PgPool, org: OrgId) -> Result<Vec<MemberView>> {
    let rows: Vec<MemberRow> = sqlx::query_as(
        r#"SELECT m.user_id, m.role, m.created_at, u.email::text AS email
           FROM memberships m
           JOIN users u ON u.id = m.user_id
           WHERE m.org_id = $1
           ORDER BY m.created_at ASC"#,
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .context("list_members")?;
    Ok(rows
        .into_iter()
        .map(|r| MemberView {
            membership: Membership {
                user_id: UserId(r.user_id),
                org_id: org,
                role: Role::from_db_str(&r.role).unwrap_or(Role::Member),
                created_at: r.created_at,
            },
            email: r.email,
        })
        .collect())
}

/// Remove a member from an org. Refuses to remove the last owner (would leave
/// the org headless) and the owner of the account the org bills to (the org
/// would keep counting against a pool its payer can no longer open). Writes a
/// `member.removed` audit row with the removed user's id in metadata. Returns
/// the outcome so the handler can map to the right HTTP status.
pub async fn remove_member(
    pool: &PgPool,
    org: OrgId,
    actor: UserId,
    user: UserId,
) -> Result<RemoveOutcome> {
    let mut tx = pool.begin().await.context("remove_member: begin")?;
    let row: Option<(String,)> = sqlx::query_as(
        r#"SELECT role FROM memberships
           WHERE org_id = $1 AND user_id = $2 FOR UPDATE"#,
    )
    .bind(org.0)
    .bind(user.0)
    .fetch_optional(&mut *tx)
    .await
    .context("remove_member: select")?;
    let Some((role,)) = row else {
        tx.rollback().await.ok();
        return Ok(RemoveOutcome::NotFound);
    };
    if role == "owner" {
        // Lock ALL owner rows, not just the target: two concurrent
        // demote/remove calls against different owners would otherwise each
        // count 2 and leave the org ownerless. (Aggregate + FOR UPDATE is
        // invalid SQL — lock the rows, count what came back.)
        let owners: Vec<(Uuid,)> = sqlx::query_as(
            r#"SELECT user_id FROM memberships
               WHERE org_id = $1 AND role = 'owner' FOR UPDATE"#,
        )
        .bind(org.0)
        .fetch_all(&mut *tx)
        .await
        .context("remove_member: lock owners")?;
        if owners.len() <= 1 {
            tx.rollback().await.ok();
            return Ok(RemoveOutcome::LastOwner);
        }
    }
    let pays_for_org: Option<(Uuid,)> = sqlx::query_as(
        r#"SELECT o.id FROM organizations o
           JOIN accounts a ON a.id = o.account_id
           WHERE o.id = $1 AND a.owner_user_id = $2"#,
    )
    .bind(org.0)
    .bind(user.0)
    .fetch_optional(&mut *tx)
    .await
    .context("remove_member: account owner check")?;
    if pays_for_org.is_some() {
        tx.rollback().await.ok();
        return Ok(RemoveOutcome::AccountOwner);
    }
    // The FK clears these anyway; doing it here moves updated_at with them and
    // lets the audit row say how many monitors lost their owner.
    let disowned = sqlx::query(
        r#"UPDATE targets SET owner_user_id = NULL, updated_at = now()
            WHERE org_id = $1 AND owner_user_id = $2"#,
    )
    .bind(org.0)
    .bind(user.0)
    .execute(&mut *tx)
    .await
    .context("remove_member: disown targets")?
    .rows_affected();
    sqlx::query(r#"DELETE FROM memberships WHERE org_id = $1 AND user_id = $2"#)
        .bind(org.0)
        .bind(user.0)
        .execute(&mut *tx)
        .await
        .context("remove_member: delete")?;
    record_audit_tx(
        &mut tx,
        org,
        Some(actor),
        "member.removed",
        serde_json::json!({ "user_id": user.0, "monitors_disowned": disowned }),
    )
    .await
    .context("remove_member: audit")?;
    tx.commit().await.context("remove_member: commit")?;
    Ok(RemoveOutcome::Removed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    NotFound,
    LastOwner,
    AccountOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetRoleOutcome {
    Updated,
    Unchanged,
    NotFound,
    LastOwner,
}

/// Change a member's role. Mirrors [`remove_member`]'s tx + FOR-UPDATE shape:
/// the in-tx owner count keeps a demotion from leaving the org ownerless.
/// A no-op (same role) commits nothing and writes no audit row.
pub async fn set_member_role(
    pool: &PgPool,
    org: OrgId,
    actor: UserId,
    target: UserId,
    new_role: Role,
) -> Result<SetRoleOutcome> {
    let mut tx = pool.begin().await.context("set_member_role: begin")?;
    let row: Option<(String,)> = sqlx::query_as(
        r#"SELECT role FROM memberships
           WHERE org_id = $1 AND user_id = $2 FOR UPDATE"#,
    )
    .bind(org.0)
    .bind(target.0)
    .fetch_optional(&mut *tx)
    .await
    .context("set_member_role: select")?;
    let Some((current_role,)) = row else {
        tx.rollback().await.ok();
        return Ok(SetRoleOutcome::NotFound);
    };
    if current_role == new_role.as_db_str() {
        tx.rollback().await.ok();
        return Ok(SetRoleOutcome::Unchanged);
    }
    if current_role == "owner" && new_role != Role::Owner {
        // Same all-owner-rows lock as remove_member — see the comment there.
        let owners: Vec<(Uuid,)> = sqlx::query_as(
            r#"SELECT user_id FROM memberships
               WHERE org_id = $1 AND role = 'owner' FOR UPDATE"#,
        )
        .bind(org.0)
        .fetch_all(&mut *tx)
        .await
        .context("set_member_role: lock owners")?;
        if owners.len() <= 1 {
            tx.rollback().await.ok();
            return Ok(SetRoleOutcome::LastOwner);
        }
    }
    sqlx::query(r#"UPDATE memberships SET role = $3 WHERE org_id = $1 AND user_id = $2"#)
        .bind(org.0)
        .bind(target.0)
        .bind(new_role.as_db_str())
        .execute(&mut *tx)
        .await
        .context("set_member_role: update")?;
    record_audit_tx(
        &mut tx,
        org,
        Some(actor),
        "member.role_changed",
        serde_json::json!({
            "user_id": target.0,
            "from": current_role,
            "to": new_role.as_db_str(),
        }),
    )
    .await
    .context("set_member_role: audit")?;
    tx.commit().await.context("set_member_role: commit")?;
    Ok(SetRoleOutcome::Updated)
}

/// Outcome of [`add_member`]. The invitation accept flow distinguishes
/// "already a member" (no-op, succeed idempotently) from a fresh insert,
/// and from a rejection because the org is at its member cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddMemberOutcome {
    Added,
    AlreadyMember,
    /// The org already holds `limit` members; the row was not inserted.
    /// `current` is the member count before this attempt.
    LimitReached {
        current: i64,
        limit: i64,
    },
}

/// Add a user to an org with the given role. Single audit row per successful
/// insert; idempotent — re-adding an existing member is a no-op.
///
/// This is the canonical site for invitation acceptance and any future flow
/// that grants org access. Kept here (not in `auth::invitations`) so the
/// `memberships` table has exactly one writer outside of org create/delete.
///
/// `actor` is the user the audit row should be attributed to. For
/// self-redeem of an invitation pass `user`; for an owner-initiated add
/// (future flow) pass the owner. Mirrors the `remove_member(actor, user)`
/// signature so the audit log records *who* effected the change, not just
/// *who* was changed.
///
/// `max_members` is the plan cap, and it counts *people across the account's
/// orgs*: the same person in two of them holds one seat, so adding them to a
/// second org costs nothing. Enforced atomically here under a per-account
/// advisory lock (the same key every other pooled cap takes): the row is
/// inserted, then the post-insert count is taken inside the same transaction
/// and the insert is rolled back if it crossed the cap. Re-adding an existing
/// member stays a no-op and is never rejected by the cap.
pub async fn add_member(
    pool: &PgPool,
    org: OrgId,
    actor: UserId,
    user: UserId,
    role: Role,
    max_members: u32,
) -> Result<AddMemberOutcome> {
    let mut tx = pool.begin().await.context("add_member: begin")?;
    let outcome = add_member_in_tx(&mut tx, org, actor, user, role, max_members).await?;
    if matches!(outcome, AddMemberOutcome::Added) {
        tx.commit().await.context("add_member: commit")?;
    } else {
        tx.rollback().await.ok();
    }
    Ok(outcome)
}

/// Membership insert inside the caller's transaction, so a caller can bind it
/// to a state change that must not outlive a refused seat. Any outcome other
/// than `Added` leaves nothing of its own behind in the transaction.
pub async fn add_member_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    actor: UserId,
    user: UserId,
    role: Role,
    max_members: u32,
) -> Result<AddMemberOutcome> {
    let account = accounts::account_for_org(&mut **tx, org).await?;
    advisory_xact_lock(&mut **tx, &account_lock_key(account))
        .await
        .context("add_member: advisory lock")?;
    let inserted: Option<(Uuid,)> = sqlx::query_as(
        r#"INSERT INTO memberships (user_id, org_id, role)
           VALUES ($1, $2, $3)
           ON CONFLICT (user_id, org_id) DO NOTHING
           RETURNING org_id"#,
    )
    .bind(user.0)
    .bind(org.0)
    .bind(role.as_db_str())
    .fetch_optional(&mut **tx)
    .await
    .context("add_member: insert")?;
    if inserted.is_none() {
        return Ok(AddMemberOutcome::AlreadyMember);
    }
    // Count *after* the insert, over the account's pool (the same query
    // `/usage` reports from, so the enforced number equals the shown one). If
    // this fresh row crossed the cap, undo it.
    let (members,): (i64,) = sqlx::query_as(&count_sql::members())
        .bind(account.0)
        .fetch_one(&mut **tx)
        .await
        .context("add_member: count members")?;
    let limit = i64::from(max_members);
    if members > limit {
        sqlx::query("DELETE FROM memberships WHERE user_id = $1 AND org_id = $2")
            .bind(user.0)
            .bind(org.0)
            .execute(&mut **tx)
            .await
            .context("add_member: undo over-cap insert")?;
        return Ok(AddMemberOutcome::LimitReached {
            current: members - 1,
            limit,
        });
    }
    record_audit_tx(
        &mut *tx,
        org,
        Some(actor),
        "member.added",
        serde_json::json!({ "user_id": user.0, "role": role.as_db_str() }),
    )
    .await
    .context("add_member: audit")?;
    Ok(AddMemberOutcome::Added)
}

/// AUTHENTICATED-PATH ONLY. Resolves an org slug to its id for callers that
/// have already proved identity (API token, session). Filters
/// `deleted_at IS NULL` and nothing else — callers must layer additional
/// access control (e.g. [`is_active_member`]) before they trust the result.
///
/// Used by the API-token request path: tokens carry no active-org state, so
/// `X-Uptimepage-Org: <slug>` is the only way the handler learns which
/// org to scope to.
///
/// Do NOT call this from any unauthenticated path. For
/// `*.{base_domain}` lookups use [`find_public_status_org_by_slug`] instead
/// — that helper additionally filters `public_status_enabled = true` and
/// returns a [`PublicStatusOrg`] newtype the operator path can't accept.
/// Substituting the two would expose every org's public surface regardless
/// of opt-in.
pub async fn find_id_by_slug(pool: &PgPool, slug: &str) -> Result<Option<OrgId>> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM organizations WHERE slug = $1 AND deleted_at IS NULL")
            .bind(slug)
            .fetch_optional(pool)
            .await
            .context("find_id_by_slug")?;
    Ok(row.map(|(o,)| OrgId(o)))
}

/// Newtype returned by [`find_public_status_page_by_slug`]. Carries the
/// resolved [`PageRef`] from the unauthenticated public-status path, kept
/// distinct from operator/API-token resolution (opposite security envelopes)
/// so the two can't share lookup helpers.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedPublicPage(pub PageRef);

/// PUBLIC-STATUS PATH ONLY. Resolves a page slug to a [`ResolvedPublicPage`]
/// for the `*.{base_domain}` surface. Filters `status_pages.enabled = true`, the
/// page not held by its plan, and the owning org not soft-deleted — if any fails
/// the row is invisible (the handler maps `None` to 404).
///
/// This and [`resolve_default_page_for_lone_org`] are the only two doors onto
/// the public surface, so gating both is what lets everything downstream —
/// branding, components, incidents, the logo, the badge — skip the check.
pub async fn find_public_status_page_by_slug(
    pool: &PgPool,
    slug: &str,
) -> Result<Option<ResolvedPublicPage>> {
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        r#"/* SAFE: public-page lookup is intentionally not org_id-scoped */
           SELECT sp.id, sp.org_id
           FROM status_pages sp
           JOIN organizations o ON o.id = sp.org_id
           WHERE sp.slug = $1
             AND sp.enabled = true
             AND sp.plan_hold_at IS NULL
             AND o.deleted_at IS NULL"#,
    )
    .bind(slug)
    .fetch_optional(pool)
    .await
    .context("find_public_status_page_by_slug")?;
    Ok(row.map(|(page, org)| {
        ResolvedPublicPage(PageRef {
            page: StatusPageId(page),
            org: OrgId(org),
        })
    }))
}

/// PUBLIC-STATUS PATH ONLY (path-based / self-host). The lone live org's PRIMARY
/// page — its oldest page (the one signup created), if enabled. Pinning to the
/// oldest page keeps `/status` stable as other pages are added or toggled:
/// disabling the primary page 404s rather than silently switching the public
/// surface to a younger (e.g. internal) page. A plan hold reads the same way and
/// for the same reason: the page is unavailable, not replaced. 404s (via `None`)
/// if there is no single live org, the org has no page, or the primary page is
/// disabled or held.
pub async fn resolve_default_page_for_lone_org(pool: &PgPool) -> Result<Option<PageRef>> {
    let Some(org) = find_lone_active_org(pool).await? else {
        return Ok(None);
    };
    let row: Option<(Uuid, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"SELECT id, enabled, plan_hold_at FROM status_pages
           WHERE org_id = $1
           ORDER BY created_at ASC, id ASC
           LIMIT 1"#,
    )
    .bind(org.0)
    .fetch_optional(pool)
    .await
    .context("resolve_default_page_for_lone_org")?;
    Ok(row
        .filter(|(_, enabled, held)| *enabled && held.is_none())
        .map(|(page, _, _)| PageRef {
            page: StatusPageId(page),
            org,
        }))
}

/// Org name + page slug + the page's display branding, as needed to render a
/// status page (header falls back to the org name when `public_display_name`
/// is unset) or to build its live URL (`slug`).
#[derive(Debug, Clone)]
pub struct OrgBranding {
    pub name: String,
    pub slug: String,
    pub branding: PublicOrgBranding,
}

impl OrgBranding {
    /// Header name: the page's display override if non-blank, else the org name.
    pub fn resolved_display_name(&self) -> &str {
        self.branding
            .public_display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.name.as_str())
    }
}

/// PUBLIC-STATUS PATH ONLY. Loads a page's display branding (+ the owning org's
/// name for the header fallback) by page id. Keyed by id because the surface
/// that resolved the page already gated visibility; the org-soft-delete filter
/// stays so a tombstoned org renders nothing.
pub async fn load_page_branding(pool: &PgPool, page: StatusPageId) -> Result<Option<OrgBranding>> {
    let row: Option<BrandingRow> = sqlx::query_as(
        r#"SELECT o.name,
                  sp.slug::text AS slug,
                  sp.public_display_name,
                  sp.public_about,
                  sp.public_brand_color,
                  (SELECT pa.content_hash FROM page_assets pa
                   WHERE pa.status_page_id = sp.id AND pa.slot = 'logo') AS logo_hash,
                  sp.public_show_powered_by,
                  sp.public_style,
                  sp.public_hide_from_search,
                  sp.public_website_url
             FROM status_pages sp
             JOIN organizations o ON o.id = sp.org_id
            WHERE sp.id = $1
              AND o.deleted_at IS NULL"#,
    )
    .bind(page.0)
    .fetch_optional(pool)
    .await
    .context("load_page_branding")?;
    Ok(row.map(|r| OrgBranding {
        name: r.name,
        slug: r.slug,
        branding: PublicOrgBranding {
            public_display_name: r.public_display_name,
            public_about: r.public_about,
            public_brand_color: r.public_brand_color,
            logo_hash: r.logo_hash,
            public_show_powered_by: r.public_show_powered_by,
            public_style: PublicStyle::from_db(&r.public_style),
            public_hide_from_search: r.public_hide_from_search,
            public_website_url: r.public_website_url,
        },
    }))
}

/// Look up a user's id by email (CITEXT). Drives the invitation flow's
/// "is the recipient already a member here?" check before INSERT, and the
/// "already-existing user redeems invitation" path on accept.
pub async fn find_user_by_email(pool: &PgPool, email: &str) -> Result<Option<UserId>> {
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM users WHERE email = $1::citext AND deleted_at IS NULL")
            .bind(email)
            .fetch_optional(pool)
            .await
            .context("find_user_by_email")?;
    Ok(row.map(|(u,)| UserId(u)))
}

/// A soft-deleted account keeps its rows, so a credential still resolves to it.
/// Every sign-in path has to ask this before it opens a session.
pub async fn user_deleted_at(pool: &PgPool, user: UserId) -> Result<Option<DateTime<Utc>>> {
    let row: Option<(Option<DateTime<Utc>>,)> =
        sqlx::query_as("SELECT deleted_at FROM users WHERE id = $1")
            .bind(user.0)
            .fetch_optional(pool)
            .await
            .context("user_deleted_at")?;
    Ok(row.and_then(|(d,)| d))
}

/// Tombstone-inclusive variant for re-auth restore; invitations keep the
/// active-only [`find_user_by_email`]. The email unique index is partial, so
/// active + tombstoned rows can coexist — prefer active, then newest.
pub async fn find_user_by_email_including_deleted(
    pool: &PgPool,
    email: &str,
) -> Result<Option<(UserId, Option<DateTime<Utc>>)>> {
    let row: Option<(Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, deleted_at FROM users WHERE email = $1::citext \
         ORDER BY (deleted_at IS NULL) DESC, created_at DESC LIMIT 1",
    )
    .bind(email)
    .fetch_optional(pool)
    .await
    .context("find_user_by_email_including_deleted")?;
    Ok(row.map(|(u, d)| (UserId(u), d)))
}

#[derive(Debug, Clone)]
pub struct OrgWithRole {
    pub org: Organization,
    pub role: Role,
}

#[derive(Debug, Clone)]
pub struct MemberView {
    pub membership: Membership,
    pub email: String,
}

/// Append an atomic row to the compliance-grade `org_audit_log`.
///
/// This is the **only** writer to `org_audit_log`. The `&mut Transaction`
/// signature is load-bearing: the audit row commits with its data change,
/// so a reader of the log reads exactly the state transitions that
/// actually happened. The downstream contract — GDPR DSR exports, SOC2
/// trails, "who renamed this org" queries — depends on every row being
/// present and atomic with its mutation.
///
/// Do not call from `tokio::spawn`, do not fire-and-forget, do not write
/// to `org_audit_log` from anywhere else. High-volume best-effort events
/// (rate-limit hits, quota blocks) go to `quota_events` via
/// [`crate::quotas::service::record_quota_event`] — a different table
/// with a different durability contract, by design.
///
/// The single-writer invariant is fenced in
/// `scripts/sg-rules/org_audit_single_writer.yml`.
pub(crate) async fn record_audit_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: OrgId,
    actor: Option<UserId>,
    action: &str,
    metadata: Value,
) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO org_audit_log (org_id, actor_id, action, metadata)
           VALUES ($1, $2, $3, $4)"#,
    )
    .bind(org.0)
    .bind(actor.map(|u| u.0))
    .bind(action)
    .bind(metadata)
    .execute(&mut **tx)
    .await
    .context("record_audit_tx")?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct BrandingRow {
    name: String,
    slug: String,
    public_display_name: Option<String>,
    public_about: Option<String>,
    public_brand_color: Option<String>,
    logo_hash: Option<String>,
    public_show_powered_by: Option<bool>,
    public_style: String,
    public_hide_from_search: bool,
    public_website_url: Option<String>,
}

#[derive(sqlx::FromRow)]
struct OrgRow {
    id: Uuid,
    slug: String,
    name: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
}

impl OrgRow {
    fn into_org(self) -> Organization {
        Organization {
            id: OrgId(self.id),
            slug: self.slug,
            name: self.name,
            created_at: self.created_at,
            updated_at: self.updated_at,
            deleted_at: self.deleted_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct OrgWithRoleRow {
    id: Uuid,
    slug: String,
    name: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    role: String,
}

impl OrgWithRoleRow {
    fn into_dto(self) -> OrgWithRole {
        let role = Role::from_db_str(&self.role).unwrap_or(Role::Member);
        OrgWithRole {
            org: Organization {
                id: OrgId(self.id),
                slug: self.slug,
                name: self.name,
                created_at: self.created_at,
                updated_at: self.updated_at,
                deleted_at: self.deleted_at,
            },
            role,
        }
    }
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    user_id: Uuid,
    role: String,
    created_at: DateTime<Utc>,
    email: String,
}

/// Org display-name lookup for the alert-delivery path, so the engine can
/// attribute mail without depending on the full org store.
#[async_trait]
pub trait OrgDirectory: Send + Sync {
    async fn display_name(&self, org: OrgId) -> Result<Option<String>>;
    /// Addresses of the org's owners, for mail about the account rather than
    /// about a monitor. Soft-deleted users and orgs are skipped.
    async fn owner_emails(&self, org: OrgId) -> Result<Vec<String>>;
}

pub struct PgOrgDirectory {
    pool: PgPool,
    /// Org names change ~never but are read once per alert send; a short TTL
    /// collapses a paging burst's repeated lookups to one query per window.
    names: moka::future::Cache<OrgId, Option<String>>,
}

impl PgOrgDirectory {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            names: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(60))
                .build(),
        }
    }
}

#[async_trait]
impl OrgDirectory for PgOrgDirectory {
    async fn display_name(&self, org: OrgId) -> Result<Option<String>> {
        if let Some(name) = self.names.get(&org).await {
            return Ok(name);
        }
        let name = get_org(&self.pool, org).await?.map(|o| o.name);
        self.names.insert(org, name.clone()).await;
        Ok(name)
    }

    async fn owner_emails(&self, org: OrgId) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"SELECT u.email
               FROM memberships m
               JOIN users u ON u.id = m.user_id
               JOIN organizations o ON o.id = m.org_id
               WHERE m.org_id = $1 AND m.role = 'owner'
                 AND u.deleted_at IS NULL AND o.deleted_at IS NULL
               ORDER BY u.email"#,
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .context("owner_emails")?;
        Ok(rows.into_iter().map(|(e,)| e).collect())
    }
}

#[derive(Default)]
pub struct InMemoryOrgDirectory {
    names: parking_lot::Mutex<std::collections::HashMap<OrgId, String>>,
    owners: parking_lot::Mutex<std::collections::HashMap<OrgId, Vec<String>>>,
}

impl InMemoryOrgDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, org: OrgId, name: impl Into<String>) {
        self.names.lock().insert(org, name.into());
    }

    pub fn insert_owner_email(&self, org: OrgId, email: impl Into<String>) {
        self.owners
            .lock()
            .entry(org)
            .or_default()
            .push(email.into());
    }
}

#[async_trait]
impl OrgDirectory for InMemoryOrgDirectory {
    async fn display_name(&self, org: OrgId) -> Result<Option<String>> {
        Ok(self.names.lock().get(&org).cloned())
    }

    async fn owner_emails(&self, org: OrgId) -> Result<Vec<String>> {
        Ok(self.owners.lock().get(&org).cloned().unwrap_or_default())
    }
}
