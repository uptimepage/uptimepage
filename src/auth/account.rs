//! Self-service account deletion + recovery (GDPR right to erasure).
//!
//! Deletion is a soft-delete with a grace window: the user vanishes from the
//! app immediately (every read filters `deleted_at IS NULL`) and the daily
//! purge job (`jobs::purge_deleted`) hard-erases the row once the grace period
//! elapses *and* no live recovery token remains. The recovery token's
//! `expires_at` is the single source of truth for "is the window still open";
//! the purge job and this module both derive the boundary from the one
//! `deleted_at` stamp rather than independently re-counting 30 days.
//!
//! The blocking decision and every write are ONE transaction under a per-user
//! advisory lock plus the per-org lock `orgs::add_member` takes, so a
//! concurrent invite-accept cannot slip a member into an org between the
//! "is this org safe to tombstone?" check and the tombstone write.

use anyhow::Context;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::api::error::codes;
use crate::domain::{AccountId, OrgId, UserId};
use crate::error::{AppError, Result};
use crate::storage::locks::{account_lock_key, advisory_xact_lock, user_delete_lock_key};
use crate::storage::orgs::record_audit_tx;

/// One blocking org in an `OWNS_SHARED_ORGS` rejection.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockingOrg {
    pub id: Uuid,
    pub slug: String,
}

/// Result of a successful deletion request.
#[derive(Debug, Clone)]
pub struct DeletionOutcome {
    pub email: String,
    /// The grace boundary: [`restore_account`] works until this instant, after
    /// which the hard purge runs.
    pub grace_deadline: DateTime<Utc>,
}

/// Soft-delete the caller's account and schedule the hard purge.
///
/// Flow: BEGIN → locks → blocking check → mutations
/// (re-asserting the invariant at write time) → hashed recovery row → COMMIT.
/// The caller sends the confirmation email *after* this returns (post-commit,
/// so a mail failure can't roll back the deletion the user asked for).
pub async fn request_deletion(
    pool: &PgPool,
    user_id: UserId,
    grace_days: u32,
) -> Result<DeletionOutcome> {
    let mut tx = pool.begin().await.context("delete_account: begin")?;

    // Per-user lock: serialises a double-submit of the delete button and
    // pairs with the per-account locks below so membership is frozen across
    // the blocking decision. `user_delete_lock_key` is a deliberately distinct
    // namespace from `user_lock_key` (the API-token cap), so account deletion
    // never contends with that — see `storage::locks`.
    advisory_xact_lock(&mut *tx, &user_delete_lock_key(user_id))
        .await
        .context("delete_account: user advisory lock")?;

    // Candidate orgs that go down with the account: the caller is the only
    // owner, or the org bills to the caller's account. Either way the org has
    // nobody else who can carry it, so it is tombstoned with the user when it
    // holds nobody else and blocks the deletion when it does. Ordered by id so
    // the per-org locks are always taken in a stable order (deadlock-free
    // against any other multi-org locker). Only the ids are needed here —
    // slug + the other-member verdict are read back *under the locks* so the
    // blocking decision can't race a concurrent invite-accept.
    let candidate_ids: Vec<Uuid> = sqlx::query_scalar(
        r#"SELECT o.id
             FROM organizations o
             JOIN memberships m
               ON m.org_id = o.id AND m.user_id = $1 AND m.role = 'owner'
             JOIN accounts a ON a.id = o.account_id
            WHERE o.deleted_at IS NULL
              AND (a.owner_user_id = $1
                   OR (SELECT count(*) FROM memberships mo
                        WHERE mo.org_id = o.id AND mo.role = 'owner') = 1)
            ORDER BY o.id"#,
    )
    .bind(user_id.0)
    .fetch_all(&mut *tx)
    .await
    .context("delete_account: select orgs that go with the account")?;

    // Same `account_lock_key` as `orgs::add_member` / `invitations::create`,
    // so holding it means no member can be added to these orgs until our
    // transaction ends. Those writers moved to the account key when the caps
    // became pooled; locking the org here instead would leave the freeze
    // contending with nothing, and a member could join a candidate org
    // between the re-read below and the tombstone. Sorted and deduplicated so
    // two concurrent deletions cannot take the same pair in opposite orders.
    let accounts: Vec<Uuid> = sqlx::query_scalar(
        "SELECT /* SAFE: bound to the orgs going down with the caller's account, gathered above under their user-delete lock */ \
         DISTINCT account_id FROM organizations WHERE id = ANY($1) ORDER BY 1",
    )
    .bind(&candidate_ids)
    .fetch_all(&mut *tx)
    .await
    .context("delete_account: resolve accounts")?;
    for account_id in &accounts {
        advisory_xact_lock(&mut *tx, &account_lock_key(AccountId(*account_id)))
            .await
            .context("delete_account: account advisory lock")?;
    }

    // Re-read the candidates *under the locks* in one query, computing the
    // other-member verdict server-side. Any candidate that still has other
    // members must be deleted first.
    let verdicts: Vec<(Uuid, String, bool)> = sqlx::query_as(
        r#"SELECT o.id, o.slug::text AS slug,
                  EXISTS (SELECT 1 FROM memberships
                           WHERE org_id = o.id AND user_id <> $2) AS has_others
             FROM organizations o
            WHERE o.id = ANY($1)
            ORDER BY o.id"#,
    )
    .bind(&candidate_ids)
    .bind(user_id.0)
    .fetch_all(&mut *tx)
    .await
    .context("delete_account: re-check members under locks")?;

    let blocking: Vec<BlockingOrg> = verdicts
        .iter()
        .filter(|(_, _, has_others)| *has_others)
        .map(|(id, slug, _)| BlockingOrg {
            id: *id,
            slug: slug.clone(),
        })
        .collect();
    if !blocking.is_empty() {
        tx.rollback().await.ok();
        return Err(AppError::unprocessable_details(
            codes::OWNS_SHARED_ORGS,
            "Delete these organisations before deleting your account.",
            serde_json::json!({ "orgs": blocking }),
        ));
    }

    // All candidates are now safe to tombstone (none has other members).
    let bound_ids: Vec<Uuid> = verdicts.into_iter().map(|(id, _, _)| id).collect();

    // Soft-delete the user. The `deleted_at IS NULL` guard makes a second
    // deletion of an already-soft-deleted account a clean 4xx rather than a
    // silent re-stamp (the one-active recovery-token unique index is the
    // backstop if this guard is ever bypassed).
    let row: Option<(DateTime<Utc>, String)> = sqlx::query_as(
        r#"UPDATE users
              SET deleted_at = now(), updated_at = now()
            WHERE id = $1 AND deleted_at IS NULL
        RETURNING deleted_at, email::text"#,
    )
    .bind(user_id.0)
    .fetch_optional(&mut *tx)
    .await
    .context("delete_account: soft-delete user")?;
    let Some((deleted_at, email)) = row else {
        tx.rollback().await.ok();
        return Err(AppError::conflict(
            codes::ACCOUNT_ALREADY_DELETED,
            "This account is already scheduled for deletion.",
        ));
    };

    // Tombstone each bound org, re-asserting the no-other-members
    // invariant at write time. Zero rows ⇒ someone joined between the check
    // and here ⇒ roll the whole transaction back and report it as blocked.
    for org_id in &bound_ids {
        let updated: Option<(Uuid,)> = sqlx::query_as(
            r#"UPDATE organizations
                  SET deleted_at = now(), updated_at = now()
                WHERE id = $1
                  AND deleted_at IS NULL
                  AND (SELECT count(*) FROM memberships
                        WHERE org_id = $1 AND user_id <> $2) = 0
            RETURNING id"#,
        )
        .bind(org_id)
        .bind(user_id.0)
        .fetch_optional(&mut *tx)
        .await
        .context("delete_account: tombstone bound org")?;
        if updated.is_none() {
            tx.rollback().await.ok();
            return Err(AppError::unprocessable_details(
                codes::OWNS_SHARED_ORGS,
                "Organisation membership changed during deletion; nothing was deleted. \
                 Retry after deleting shared organisations.",
                serde_json::json!({ "orgs": [{ "id": org_id }] }),
            ));
        }
        // Audit row per tombstoned org — the same surface `org.deleted` /
        // `org.restored` use, so recovery's `user.deletion_recovered` pairs
        // with it and `list_deleted_orgs_deleted_by` keeps working.
        record_audit_tx(
            &mut tx,
            OrgId(*org_id),
            Some(user_id),
            "user.deletion_requested",
            serde_json::json!({ "user_id": user_id.0 }),
        )
        .await
        .context("delete_account: audit")?;
    }

    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id.0)
        .execute(&mut *tx)
        .await
        .context("delete_account: drop sessions")?;
    sqlx::query("DELETE FROM api_tokens WHERE user_id = $1")
        .bind(user_id.0)
        .execute(&mut *tx)
        .await
        .context("delete_account: drop api tokens")?;
    sqlx::query(
        "/* SAFE: user-delete drops pending invites across every org they invited from */ \
         DELETE FROM invitations WHERE inviter_id = $1 AND accepted_at IS NULL",
    )
    .bind(user_id.0)
    .execute(&mut *tx)
    .await
    .context("delete_account: drop pending invitations")?;
    // Drop memberships in *shared* orgs only. The owner membership on each
    // tombstoned bound org is intentionally kept: it is how recovery re-grants
    // access to the restored orgs and how the bound set is re-derived without
    // a schema column. The eventual user hard-purge cascades these away, and
    // the org soft-delete reaches its own grace boundary in lockstep (same
    // `deleted_at`), so nothing leaks past the window.
    sqlx::query("DELETE FROM memberships WHERE user_id = $1 AND org_id <> ALL($2)")
        .bind(user_id.0)
        .bind(&bound_ids)
        .execute(&mut *tx)
        .await
        .context("delete_account: drop shared memberships")?;

    // Grace boundary, anchored to the single `deleted_at` stamp so this and the
    // purge job agree by construction rather than re-deriving "30 days". The
    // soft-deleted user can restore (see `restore_account`) within this window;
    // the purge job hard-deletes after it.
    let grace_deadline = deleted_at + chrono::Duration::days(i64::from(grace_days));

    tx.commit().await.context("delete_account: commit")?;

    Ok(DeletionOutcome {
        email,
        grace_deadline,
    })
}

#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub email: String,
    pub orgs: Vec<Uuid>,
}

/// Restore a soft-deleted account. Signing in does NOT do this: undoing an
/// erasure the user deliberately asked for must be its own deliberate act.
///
/// `None` when the account was already active, so a double-submit doesn't mail
/// the user twice.
pub async fn restore_account(pool: &PgPool, user_id: UserId) -> Result<Option<RestoreOutcome>> {
    let mut tx = pool.begin().await.context("restore: begin")?;
    let restored = undelete_in_tx(&mut tx, user_id).await?;
    let Some(outcome) = restored else {
        tx.rollback().await.ok();
        return Ok(None);
    };
    tx.commit().await.context("restore: commit")?;
    Ok(Some(outcome))
}

/// Clear `users.deleted_at`, lift the tombstone on the orgs bound to them, and
/// audit it. `None` when the row was already active — a benign race, not an
/// error.
async fn undelete_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    user_id: UserId,
) -> Result<Option<RestoreOutcome>> {
    // Serialise against the daily purge (same per-user lock): without it a
    // restore and a same-user purge can interleave on the last grace day, the
    // org cascade wiping tenant data microseconds before this clears
    // `deleted_at` → user restored into an emptied account.
    advisory_xact_lock(&mut **tx, &user_delete_lock_key(user_id))
        .await
        .context("undelete: user advisory lock")?;

    // Read when the account was closed *before* clearing it. That timestamp is
    // what separates the orgs this deletion tombstoned from ones the user had
    // already deleted on their own, and only the former are this recovery's to
    // undo. Read as its own statement rather than a subquery inside the
    // UPDATE's RETURNING: the lock above already makes it race-free, and the
    // intent survives a reader who does not know when a RETURNING subquery is
    // evaluated.
    let deleted_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM users WHERE id = $1")
            .bind(user_id.0)
            .fetch_optional(&mut **tx)
            .await
            .context("undelete: read deletion time")?
            .flatten();
    let Some(deleted_at) = deleted_at else {
        return Ok(None);
    };

    let row: Option<(String,)> = sqlx::query_as(
        "UPDATE users SET deleted_at = NULL, updated_at = now() \
         WHERE id = $1 AND deleted_at IS NOT NULL \
         RETURNING email::text",
    )
    .bind(user_id.0)
    .fetch_optional(&mut **tx)
    .await
    .context("undelete: un-delete user")?;
    let Some((email,)) = row else {
        return Ok(None);
    };

    // Lift the tombstone on the bound orgs this deletion took down — the ones
    // whose owner membership it deliberately kept. `delete_account` stamps the
    // user and those orgs from a single transaction-stable `now()`, so their
    // `deleted_at` is never earlier than the user's.
    //
    // An org the user deleted *before* asking to close the account is not part
    // of this recovery: sweeping it back in would restore capacity the account
    // had already given up, past `max_orgs` and past every pooled cap. It stays
    // tombstoned and recoverable on its own, through `restore_org`, which
    // counts what comes back.
    let restored: Vec<(Uuid,)> = sqlx::query_as(
        r#"UPDATE organizations o
              SET deleted_at = NULL, updated_at = now()
             FROM memberships m
            WHERE m.org_id = o.id
              AND m.user_id = $1
              AND m.role = 'owner'
              AND o.deleted_at IS NOT NULL
              AND o.deleted_at >= $2
        RETURNING o.id"#,
    )
    .bind(user_id.0)
    .bind(deleted_at)
    .fetch_all(&mut **tx)
    .await
    .context("undelete: un-delete bound orgs")?;

    for (org_id,) in &restored {
        record_audit_tx(
            tx,
            OrgId(*org_id),
            Some(user_id),
            "user.deletion_recovered",
            serde_json::json!({ "user_id": user_id.0 }),
        )
        .await
        .context("undelete: audit")?;
    }

    Ok(Some(RestoreOutcome {
        email,
        orgs: restored.into_iter().map(|(id,)| id).collect(),
    }))
}

/// `last_seen_at` is the session-touched activity timestamp (cookie auth,
/// API tokens, magic-link), not just OAuth handshakes — what users read
/// "Last sign-in" as.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AccountFacts {
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

pub async fn account_facts(pool: &PgPool, user_id: UserId) -> Result<Option<AccountFacts>> {
    Ok(sqlx::query_as::<_, AccountFacts>(
        "SELECT u.created_at, u.last_seen_at FROM users u \
          WHERE u.id = $1 AND u.deleted_at IS NULL",
    )
    .bind(user_id.0)
    .fetch_optional(pool)
    .await
    .context("account_facts: query")?)
}
