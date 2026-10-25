//! The page password: the throttle a try passes first, the session a right
//! one opens, and what the owner sees of it. The hash is read here and only
//! here, where a password or a feed link is checked against it.

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::domain::{
    AccessEventKind, AccessEventMethod, OrgId, PageAccess, PagePasswordView, PageRef,
    PageSessionMethod, StatusPageId, UserId, normalize_email,
};
use crate::error::Result;
use crate::storage::locks::advisory_xact_lock;
use crate::storage::orgs::record_audit_tx;

use super::sessions::{self, CreatedSession, Opener};

/// Wrong tries per page per network inside [`TRY_WINDOW_MINUTES`].
pub const TRY_LIMIT: i64 = 10;
pub const TRY_WINDOW_MINUTES: i32 = 15;

/// A wrong-try slot, held while the password is checked. It is recorded as
/// a `password_failed` event from the start, so a burst in flight counts in
/// full; a right password hands it back.
#[derive(Debug, Clone, Copy)]
pub struct TryClaim {
    id: Uuid,
    occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub enum TryOutcome {
    Claimed(TryClaim),
    /// Seconds until the oldest counted try leaves the window.
    Throttled {
        retry_after_secs: i64,
    },
}

/// Serialised on the page and network, so the eleventh of a parallel burst
/// sees the ten before it, and a throttled try costs no argon2 run.
pub async fn claim_try(
    pool: &PgPool,
    page: PageRef,
    ip_hash: Option<&str>,
    user_agent_hash: Option<&str>,
) -> Result<TryOutcome> {
    let mut tx = pool.begin().await.context("page_password::claim begin")?;
    advisory_xact_lock(
        &mut *tx,
        &format!(
            "page_password_try:{}:{}",
            page.page.0,
            ip_hash.unwrap_or("none")
        ),
    )
    .await
    .context("page_password::claim lock")?;
    let (counted, nth_newest): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT count(*), (array_agg(occurred_at ORDER BY occurred_at DESC))[$4] \
         FROM status_page_access_events \
         WHERE status_page_id = $1 AND org_id = $2 AND kind = $5 \
           AND ip_hash IS NOT DISTINCT FROM $3 \
           AND occurred_at > now() - make_interval(mins => $6)",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .bind(ip_hash)
    .bind(TRY_LIMIT as i32)
    .bind(AccessEventKind::PasswordFailed.as_db_str())
    .bind(TRY_WINDOW_MINUTES)
    .fetch_one(&mut *tx)
    .await
    .context("page_password::claim count")?;
    if counted >= TRY_LIMIT {
        tx.rollback().await.ok();
        let now = Utc::now();
        let frees_at = nth_newest.unwrap_or(now) + Duration::minutes(i64::from(TRY_WINDOW_MINUTES));
        return Ok(TryOutcome::Throttled {
            retry_after_secs: (frees_at - now).num_seconds().max(1),
        });
    }
    let (id, occurred_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO status_page_access_events \
             (status_page_id, org_id, kind, method, failure_reason, ip_hash, user_agent_hash) \
         VALUES ($1, $2, $3, $4, 'wrong_password', $5, $6) \
         RETURNING id, occurred_at",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .bind(AccessEventKind::PasswordFailed.as_db_str())
    .bind(AccessEventMethod::Password.as_db_str())
    .bind(ip_hash)
    .bind(user_agent_hash)
    .fetch_one(&mut *tx)
    .await
    .context("page_password::claim insert")?;
    tx.commit().await.context("page_password::claim commit")?;
    Ok(TryOutcome::Claimed(TryClaim { id, occurred_at }))
}

async fn release_try<'c, E: PgExecutor<'c>>(
    executor: E,
    page: PageRef,
    claim: TryClaim,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM status_page_access_events \
         WHERE id = $1 AND occurred_at = $2 AND status_page_id = $3 AND org_id = $4",
    )
    .bind(claim.id)
    .bind(claim.occurred_at)
    .bind(page.page.0)
    .bind(page.org.0)
    .execute(executor)
    .await
    .context("page_password::release")?;
    Ok(())
}

/// The hash to check a try or a feed link against, while the password lets
/// anyone in at all.
pub async fn live_hash(pool: &PgPool, page: PageRef) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT password_hash FROM status_page_access \
         WHERE status_page_id = $1 AND org_id = $2 AND protected AND password_enabled",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .fetch_optional(pool)
    .await
    .context("page_password::live_hash")?;
    Ok(row.and_then(|(h,)| h))
}

/// The hash a password session's feed link is cut from, read in one statement
/// with the session: a new password ends its sessions in the same commit, so a
/// session it ended never gets a link for it.
pub async fn session_hash(pool: &PgPool, page: PageRef, session: &str) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT a.password_hash FROM status_page_access a \
         JOIN status_page_sessions s \
              ON s.status_page_id = a.status_page_id AND s.org_id = a.org_id \
         WHERE a.status_page_id = $1 AND a.org_id = $2 AND a.protected AND a.password_enabled \
           AND s.id_hash = $3 AND s.method = $4 \
           AND (s.expires_at IS NULL OR s.expires_at > now()) \
           AND (a.session_days IS NULL \
                OR s.created_at > now() - make_interval(days => a.session_days))",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .bind(session)
    .bind(PageSessionMethod::Password.as_db_str())
    .fetch_optional(pool)
    .await
    .context("page_password::session_hash")?;
    Ok(row.and_then(|(h,)| h))
}

/// Opens a password session for a try that matched `checked_hash`, under a
/// shared lock on the access row: a change or turning the password off holds
/// it exclusively until it commits, so a sign-in that queued behind one sees
/// the new row and is refused, and one that went first is deleted by it.
/// `None` when the password changed or closed since the check; the claim
/// then stays, as a wrong try.
pub async fn open_session(
    pool: &PgPool,
    page: PageRef,
    checked_hash: &str,
    claim: TryClaim,
    replaces: Option<&str>,
    ip_hash: Option<&str>,
    user_agent_hash: Option<&str>,
) -> Result<Option<CreatedSession>> {
    let mut tx = pool.begin().await.context("page_password::open begin")?;
    lock_page_first(&mut tx, page).await?;
    let row: Option<(bool, bool, Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT protected, password_enabled, password_hash, session_days \
         FROM status_page_access WHERE status_page_id = $1 AND org_id = $2 FOR SHARE",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .fetch_optional(&mut *tx)
    .await
    .context("page_password::open lock")?;
    let Some((true, true, Some(hash), session_days)) = row else {
        tx.rollback().await.ok();
        return Ok(None);
    };
    if hash != checked_hash {
        tx.rollback().await.ok();
        return Ok(None);
    }
    if let Some(old) = replaces {
        sessions::delete(&mut *tx, page, old).await?;
    }
    let created = sessions::create(
        &mut *tx,
        page,
        Opener::Password,
        session_days,
        ip_hash,
        user_agent_hash,
    )
    .await?;
    release_try(&mut *tx, page, claim).await?;
    tx.commit().await.context("page_password::open commit")?;
    Ok(Some(created))
}

/// A grant under the access row's shared lock inserts a row whose foreign key
/// locks the page row. An owner's save locks the page row first and the
/// access row after it, so a grant takes the page row first too, in the
/// share mode its insert would take anyway, or the two could deadlock.
pub(crate) async fn lock_page_first(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    page: PageRef,
) -> Result<()> {
    sqlx::query("SELECT 1 FROM status_pages WHERE id = $1 AND org_id = $2 FOR KEY SHARE")
        .bind(page.page.0)
        .bind(page.org.0)
        .fetch_optional(&mut **tx)
        .await
        .context("page_password::lock page")?;
    Ok(())
}

/// "Sign them all out": the password sessions that exist now. The password
/// still works, so this takes no lock and promises nothing about a sign-in
/// in flight.
pub async fn end_sessions(
    pool: &PgPool,
    org: OrgId,
    page: StatusPageId,
    actor: UserId,
) -> Result<u64> {
    let mut tx = pool.begin().await.context("page_password::end begin")?;
    let ended = sessions::end_by_method(&mut *tx, org, page, PageSessionMethod::Password).await?;
    record_audit_tx(
        &mut tx,
        org,
        Some(actor),
        "status_page.password_sessions_ended",
        json!({ "status_page_id": page.0, "sessions": ended }),
    )
    .await?;
    tx.commit().await.context("page_password::end commit")?;
    Ok(ended)
}

#[derive(Default, sqlx::FromRow)]
struct SummaryRow {
    set_by: Option<String>,
    sessions: i64,
    last_sign_in_at: Option<DateTime<Utc>>,
    failed_last_24h: i64,
}

/// The line for a page whose access is `access`, as just loaded.
pub async fn summary(
    pool: &PgPool,
    page: PageRef,
    access: &PageAccess,
) -> Result<PagePasswordView> {
    let row: SummaryRow = sqlx::query_as(
        "SELECT u.email::text AS set_by, \
                (SELECT count(*) FROM status_page_sessions s \
                  WHERE s.status_page_id = a.status_page_id AND s.method = $3 \
                    AND (s.expires_at IS NULL OR s.expires_at > now()) \
                    AND (a.session_days IS NULL \
                         OR s.created_at > now() - make_interval(days => a.session_days)) \
                ) AS sessions, \
                (SELECT max(e.occurred_at) FROM status_page_access_events e \
                  WHERE e.status_page_id = a.status_page_id AND e.kind = $4 AND e.method = $5 \
                ) AS last_sign_in_at, \
                (SELECT count(*) FROM status_page_access_events e \
                  WHERE e.status_page_id = a.status_page_id AND e.kind = $6 \
                    AND e.occurred_at > now() - interval '24 hours' \
                ) AS failed_last_24h \
         FROM status_page_access a \
         LEFT JOIN users u ON u.id = a.password_set_by \
         WHERE a.status_page_id = $1 AND a.org_id = $2",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .bind(PageSessionMethod::Password.as_db_str())
    .bind(AccessEventKind::SignedIn.as_db_str())
    .bind(AccessEventMethod::Password.as_db_str())
    .bind(AccessEventKind::PasswordFailed.as_db_str())
    .fetch_optional(pool)
    .await
    .context("page_password::summary")?
    .unwrap_or_default();
    let subscriptions = match access.password_set_at {
        Some(set_at) => {
            let targets: Vec<(String,)> = sqlx::query_as(
                "SELECT target FROM status_page_subscribers \
                 WHERE status_page_id = $1 AND org_id = $2 AND channel = 'email' \
                   AND via_password_at = $3",
            )
            .bind(page.page.0)
            .bind(page.org.0)
            .bind(set_at)
            .fetch_all(pool)
            .await
            .context("page_password::summary subscriptions")?;
            targets
                .iter()
                .filter(|(t,)| !normalize_email(t).is_some_and(|e| access.admits(&e)))
                .count() as i64
        }
        None => 0,
    };
    Ok(PagePasswordView {
        enabled: access.settings.password_enabled,
        set_at: access.password_set_at,
        set_by: row.set_by,
        sessions: row.sessions,
        last_sign_in_at: row.last_sign_in_at,
        failed_last_24h: row.failed_last_24h,
        subscriptions,
    })
}
