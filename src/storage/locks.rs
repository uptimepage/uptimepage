//! Postgres transaction-scoped advisory locks.
//!
//! Several flows guard a "count then write under a cap" sequence that is NOT
//! race-safe under READ COMMITTED on its own: concurrent actors each see a
//! snapshot count, all pass `+1 <= limit`, and the cap is overshot. The fix
//! everywhere is the same — take a `pg_advisory_xact_lock` keyed on the
//! subject, then count, then write, all in one transaction.
//!
//! The lock key is load-bearing: flows that must serialise against each other
//! have to hash the *same* string, and flows that must not collide have to
//! differ. That correspondence used to live only in copied SQL string
//! literals and prose comments — one transposed key and the guarantee breaks
//! silently. This module is the single owner of both the lock statement and
//! the key derivation, so the relationships live in one place and are
//! reviewed together rather than re-derived at each call site:
//!
//! - [`org_lock_key`] — one org. Left for guards whose subject really is a
//!   single org, such as one org's outstanding channel-link codes.
//! - [`account_lock_key`] — one account. Every pooled resource cap (targets,
//!   status pages, channels, members, invitations, …) counts across the
//!   account's orgs, so its guard must serialise on the account, not on one of
//!   its orgs. Account deletion's membership freeze takes the same key: it
//!   holds against member adds only if it contends with them.
//! - [`membership_lock_key`] — one org's memberships. Removing a member and
//!   changing a role each lock the target and then every owner row; holding
//!   this first keeps two of them in one org from deadlocking.
//! - [`user_lock_key`] — one user. The per-user API-token cap guard.
//! - [`user_delete_lock_key`] — one user, a deliberately distinct namespace
//!   from [`user_lock_key`] so account deletion does not serialise against
//!   unrelated per-user cap writes.
//! - [`magic_link_send_lock_key`] — one email address's send window, so
//!   concurrent anonymous requests elect a single sender. Own namespace.
//! - [`signup_lock_key`] — one global key for every signup, so the founding
//!   first-N plan count cannot be raced past its cutoff by concurrent signups.
//! - [`job_lock_key`] — one named background job. Used by [`try_job`] so two
//!   ticks of the same periodic worker cannot run concurrently when the
//!   previous tick is slow (network blip, CH mutation queue, etc).

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use sqlx::{PgExecutor, PgPool};

use crate::domain::{AccountId, OrgId, UserId};

/// Lock key for a per-org critical section (membership / target caps, the
/// deletion-time membership freeze).
pub fn org_lock_key(org: OrgId) -> String {
    org.0.to_string()
}

/// Lock key for a per-account critical section. Every pooled resource cap
/// serialises here rather than on the org, because the count spans all of the
/// account's orgs: two creates in two orgs of one account must contend.
pub fn account_lock_key(account: AccountId) -> String {
    format!("account:{}", account.0)
}

/// Lock key for changes to one org's memberships (removal, role change).
pub fn membership_lock_key(org: OrgId) -> String {
    format!("membership:{}", org.0)
}

/// Lock key for a per-user cap critical section (the API-token count, and the
/// org-delete double-submit guard).
pub fn user_lock_key(user: UserId) -> String {
    user.0.to_string()
}

/// Lock key for account deletion. Distinct namespace from [`user_lock_key`]
/// on purpose: deletion must not contend with unrelated per-user cap writes.
pub fn user_delete_lock_key(user: UserId) -> String {
    format!("user_delete:{}", user.0)
}

/// Lock key for one address's magic-link send window. Lower-cased because the
/// column is `citext`: two casings are one address to the throttle query, so
/// they have to be one address to the lock as well.
pub fn magic_link_send_lock_key(email: &str) -> String {
    format!("magic_link_send:{}", email.to_lowercase())
}

/// Global lock key for the founding-plan signup cutoff. Every signup hashes
/// the same string, so they serialise while counting toward the first-N
/// founding cutoff and cannot overshoot it. Own namespace.
pub fn signup_lock_key() -> &'static str {
    "signup:founding"
}

/// Take a transaction-scoped advisory lock on `key`. Held until the
/// transaction commits or rolls back. Pass the transaction handle
/// (`&mut *tx`); the lock is released automatically with that transaction.
pub async fn advisory_xact_lock<'c, E>(executor: E, key: &str) -> sqlx::Result<()>
where
    E: PgExecutor<'c>,
{
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
        .bind(key)
        .execute(executor)
        .await
        .map(|_| ())
}

/// Lock key for a named background job. Distinct namespace from any
/// per-org / per-user keys so a job lock cannot accidentally collide with
/// user-flow critical sections.
pub fn job_lock_key(job: &'static str) -> String {
    format!("job:{job}")
}

/// Lock key for one app account's link offers, so two acknowledgements landing
/// together cannot both slip under the offer cooldown.
pub fn app_account_lock_key(app: &str, account_hex: &str) -> String {
    format!("app_account:{app}:{account_hex}")
}

/// Lock key for replacing one person's Telegram link code, so two requests
/// landing together cannot both leave a live code behind.
pub fn app_link_user_lock_key(user: UserId) -> String {
    format!("app_link_user:{}", user.0)
}

/// Lock key for an incident's lifecycle critical section. Concurrent
/// acknowledge / assign / resolve / escalate must serialise on the same
/// incident so the state machine cannot be raced (e.g. an auto-resolve
/// landing between a human ack's read and write). Own namespace so it never
/// collides with org / user / job keys in the shared hash space.
pub fn incident_lock_key(incident_id: uuid::Uuid) -> String {
    format!("incident:{incident_id}")
}

/// Run `body` only when the lock for `job` can be taken without waiting.
///
/// Holds a session-scoped `pg_try_advisory_lock` on a dedicated pool
/// connection for the duration of `body`, then releases it before the
/// connection returns to the pool. Errors at every step are treated as
/// "skip this tick" — a background job that cannot take its own lock is
/// never a fatal condition and the next tick retries from scratch.
///
/// Panic safety: `body` runs under `catch_unwind`. An unwinding panic
/// still releases the lock and closes the connection (so the panicking
/// session is not handed back to the pool with the lock still held),
/// then the panic is resumed.
pub async fn try_job<F, Fut>(pool: &PgPool, job: &'static str, body: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut conn = match pool.acquire().await {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(job, error = %err, "job_lock: pool acquire failed; skipping tick");
            return;
        }
    };
    let key = job_lock_key(job);
    let acquired: Result<bool, _> =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1::text, 0))")
            .bind(&key)
            .fetch_one(&mut *conn)
            .await;
    match acquired {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(job, "job_lock: held by another tick; skipping");
            return;
        }
        Err(err) => {
            tracing::warn!(job, error = %err, "job_lock: try_lock query failed; skipping tick");
            return;
        }
    }

    let outcome = AssertUnwindSafe(body()).catch_unwind().await;

    // Keyed unlock matches the acquire; do NOT use `pg_advisory_unlock_all()`
    // here — it would silently strip any other session-scoped lock a future
    // nested call (or library upgrade) might take on the same connection.
    let unlock = sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1::text, 0))")
        .bind(&key)
        .execute(&mut *conn)
        .await;
    if let Err(err) = unlock {
        tracing::warn!(job, error = %err, "job_lock: unlock failed");
    }

    if let Err(panic) = outcome {
        // Evict the connection rather than recycle it: the panic may have
        // left other session state (cursors, prepared statements, search_path)
        // in an unknown shape.
        conn.close_on_drop();
        std::panic::resume_unwind(panic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_lock_key_is_namespaced_to_avoid_org_or_user_collisions() {
        let key = job_lock_key("retention");
        // The "job:" prefix is the namespace contract; org / user keys are
        // bare UUIDs and the user-delete key has its own "user_delete:"
        // prefix, so this layout guarantees no collision in the shared
        // 64-bit `hashtextextended` space.
        assert!(key.starts_with("job:"));
        assert_eq!(key, "job:retention");
    }
}
