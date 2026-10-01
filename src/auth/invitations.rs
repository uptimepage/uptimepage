//! Organization invitations: token generation, persistence, accept/decline,
//! cleanup.
//!
//! Tokens are 32 cryptographically random bytes presented as a 43-char
//! base64url-no-pad string in the email link. Only the argon2id hash is
//! persisted; presenting the raw token at the redeem endpoint is what
//! proves possession.
//!
//! `token_prefix` (first [`TOKEN_PREFIX_LEN`] chars of the raw token) is
//! stored alongside the hash and indexed. Lookup narrows the candidate set
//! to ~1 row via the prefix and argon2-verifies the survivor — without it
//! the redeem path is a CPU-amplification DoS at scale (50 verifies per
//! org × N orgs).
//!
//! Invitations are single-use: the same row carries both `accepted_at` and
//! `declined_at`; either being non-NULL takes the row out of the "pending"
//! partial indexes.

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{OrgId, Organization, Role, UserId};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::quotas::QuotaService;
use crate::security::token_hash::{self, slice_prefix};
use crate::storage::locks::{account_lock_key, advisory_xact_lock};
use crate::storage::orgs as orgs_store;

/// Hash-friendly invitation record. `token_hash` is the encoded argon2id
/// PHC string; only the hash leaves this row.
#[derive(Debug, Clone)]
pub struct InvitationRow {
    pub id: Uuid,
    pub org_id: OrgId,
    pub inviter_id: UserId,
    pub email: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub declined_at: Option<DateTime<Utc>>,
}

/// Output of [`create`] — includes the raw token that must be embedded in the
/// outgoing email. Never persisted, never recoverable after this call returns.
#[derive(Debug, Clone)]
pub struct CreatedInvitation {
    pub row: InvitationRow,
    /// Raw 43-char base64url token. Goes into the email link only.
    pub token: String,
}

/// Counted from the audit trail, not from `invitations`: revoking deletes the
/// row, and revoke-then-resend is the bypass this closes.
pub const MAX_SENDS_PER_WINDOW: i64 = 25;
pub const SEND_WINDOW_HOURS: i64 = 24;

pub use crate::security::token_hash::generate_raw_token;

/// Refuses once the org has sent [`MAX_SENDS_PER_WINDOW`] inside the window.
/// Both the create and the resend path mail, so both ask.
pub async fn ensure_send_window(pool: &PgPool, org: OrgId, inviter: UserId) -> Result<()> {
    let (sent,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM org_audit_log \
         WHERE org_id = $1 AND action = 'invitation.sent' \
         AND occurred_at > now() - make_interval(hours => $2)",
    )
    .bind(org.0)
    .bind(i32::try_from(SEND_WINDOW_HOURS).unwrap_or(24))
    .fetch_one(pool)
    .await
    .context("invitations::ensure_send_window")?;
    if sent < MAX_SENDS_PER_WINDOW {
        return Ok(());
    }
    tracing::warn!(org_id = %org.0, inviter = %inviter.0, sent, "invitation send limit reached");
    crate::quotas::service::record_quota_event(
        Some(pool.clone()),
        Some(org),
        Some(inviter),
        "quota_exceeded",
        Some(crate::domain::quota::usage_keys::INVITATION_SENDS),
        serde_json::json!({ "current": sent, "limit": MAX_SENDS_PER_WINDOW }),
        None,
    );
    Err(AppError::conflict(
        crate::error::codes::INVITATION_SEND_LIMIT,
        format!("invitation send limit reached ({MAX_SENDS_PER_WINDOW} in {SEND_WINDOW_HOURS}h)"),
    ))
}

/// Append-only, so it survives the revoke that deletes the invitation row.
/// Written once the mail is away, so a failed send spends no budget.
pub async fn record_send(
    pool: &PgPool,
    org: OrgId,
    inviter: UserId,
    invitation: Uuid,
) -> Result<()> {
    let mut tx = pool.begin().await.context("record_send: begin")?;
    crate::storage::orgs::record_audit_tx(
        &mut tx,
        org,
        Some(inviter),
        "invitation.sent",
        serde_json::json!({ "invitation_id": invitation }),
    )
    .await?;
    tx.commit().await.context("record_send: commit")?;
    Ok(())
}

/// Already-pending (non-accepted, non-declined, unexpired) invitation for the
/// given (org, email) pair. CITEXT email comparison.
pub async fn exists_pending_for_email(pool: &PgPool, org: OrgId, email: &str) -> Result<bool> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM invitations \
         WHERE org_id = $1 AND email = $2::citext \
         AND accepted_at IS NULL AND declined_at IS NULL \
         AND expires_at > now() \
         LIMIT 1",
    )
    .bind(org.0)
    .bind(email)
    .fetch_optional(pool)
    .await
    .context("invitations::exists_pending_for_email")?;
    Ok(row.is_some())
}

/// Issue an invitation, enforcing the per-email dedupe and the per-org
/// pending cap **atomically**. A per-org advisory lock held for the
/// transaction serialises concurrent invite creation for the same org, so
/// the dedupe and count checks below cannot race their own INSERT — without
/// it both are check-then-act under READ COMMITTED (two requests both see
/// "no duplicate" / "count < max" and both insert, double-sending to one
/// address and overshooting the cap). This mirrors the owner-org cap in
/// `storage::orgs::create_org_with_owner`. `ALREADY_INVITED` /
/// `INVITATIONS_LIMIT` are returned here rather than pre-checked in the
/// handler so there is exactly one place the rule lives.
pub async fn create(
    pool: &PgPool,
    org: OrgId,
    inviter: UserId,
    email: &str,
    role: Role,
    expiry_hours: u32,
    max_pending: u32,
) -> Result<CreatedInvitation> {
    let raw = generate_raw_token();
    let prefix = slice_prefix(&raw).to_string();
    let expires_at = Utc::now() + Duration::hours(i64::from(expiry_hours));

    let mut tx = pool.begin().await.context("invitations::create: begin")?;

    // Pending invitations are capped per account, so the guard serialises on
    // the account; the duplicate check below stays scoped to this org.
    let account = crate::storage::accounts::account_for_org(&mut *tx, org).await?;
    advisory_xact_lock(&mut *tx, &account_lock_key(account))
        .await
        .context("invitations::create: advisory lock")?;

    // No expires_at filter: matches idx_invitations_one_pending's predicate so
    // an expired-unredeemed row is a clean ALREADY_INVITED, not a unique-
    // violation 500 (the owner resends to revive it).
    let duplicate: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM invitations \
         WHERE org_id = $1 AND email = $2::citext \
         AND accepted_at IS NULL AND declined_at IS NULL \
         LIMIT 1",
    )
    .bind(org.0)
    .bind(email)
    .fetch_optional(&mut *tx)
    .await
    .context("invitations::create: dedupe")?;
    if duplicate.is_some() {
        tx.rollback().await.ok();
        return Err(AppError::conflict(
            crate::error::codes::ALREADY_INVITED,
            "there is already a pending invitation for this email",
        ));
    }

    let (pending,): (i64,) = sqlx::query_as(&crate::storage::count_sql::pending_invitations())
        .bind(account.0)
        .fetch_one(&mut *tx)
        .await
        .context("invitations::create: count pending")?;
    if u32::try_from(pending).unwrap_or(u32::MAX) >= max_pending {
        tx.rollback().await.ok();
        crate::quotas::service::record_quota_event(
            Some(pool.clone()),
            Some(org),
            Some(inviter),
            "quota_exceeded",
            Some("max_pending_invitations"),
            serde_json::json!({ "current": pending, "limit": i64::from(max_pending) }),
            None,
        );
        return Err(AppError::conflict(
            crate::error::codes::INVITATIONS_LIMIT,
            format!("pending invitation limit reached ({max_pending})"),
        ));
    }

    // Argon2 only after the cheap dedupe/cap rejects: hashing first would
    // make every blocked abuse-path request pay ~150 ms of CPU for a token
    // that's discarded — the exact cost the cap exists to bound.
    let hash = token_hash::hash(&raw)?;

    let row: (Uuid, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO invitations \
            (org_id, inviter_id, email, role, token_hash, token_prefix, expires_at) \
         VALUES ($1, $2, $3::citext, $4, $5, $6, $7) \
         RETURNING id, created_at, expires_at",
    )
    .bind(org.0)
    .bind(inviter.0)
    .bind(email)
    .bind(role.as_db_str())
    .bind(&hash)
    .bind(&prefix)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await
    .context("invitations::create: insert")?;

    tx.commit().await.context("invitations::create: commit")?;

    Ok(CreatedInvitation {
        row: InvitationRow {
            id: row.0,
            org_id: org,
            inviter_id: inviter,
            email: email.to_string(),
            role,
            created_at: row.1,
            expires_at: row.2,
            accepted_at: None,
            declined_at: None,
        },
        token: raw,
    })
}

/// Find the unique pending invitation that matches `raw_token`. Narrows the
/// candidate set via the indexed `token_prefix` column (96-bit prefix
/// entropy), then argon2-verifies the surviving rows.
///
/// Returns `None` for any of: nothing matched, expired, accepted/declined,
/// row deleted. The handler must not distinguish these to the caller —
/// "INVITATION_INVALID" covers them all (anti-enumeration).
/// Login-flow edge resolver: raw invitation token (login page query param) →
/// pending row id, trimmed. Unknown/expired tokens fall through to None
/// silently — the post-login redirect just lands at `/` and the operator can
/// re-issue. Single owner of that policy for the OAuth and magic-link starts.
pub async fn resolve_pending_invitation_id(
    pool: &PgPool,
    raw: Option<&str>,
) -> Result<Option<Uuid>> {
    match raw.map(str::trim) {
        Some(t) if !t.is_empty() => Ok(find_pending_by_token(pool, t).await?.map(|r| r.id)),
        _ => Ok(None),
    }
}

/// Pending-row lookup by id — for login flows whose `oauth_states` /
/// `magic_link_tokens` row carries a resolved invitation id (possession was
/// proven at login start, so no argon2 here).
pub async fn find_pending_by_id(pool: &PgPool, id: Uuid) -> Result<Option<InvitationRow>> {
    let row: Option<RawRow> = sqlx::query_as(
        "SELECT id, org_id, inviter_id, email::text AS email, role, token_hash, \
                created_at, expires_at, accepted_at, declined_at \
         FROM invitations \
         WHERE id = $1 \
         AND accepted_at IS NULL AND declined_at IS NULL \
         AND expires_at > now()",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("invitations::find_pending_by_id")?;
    row.map(InvitationRow::try_from).transpose()
}

pub async fn find_pending_by_token(
    pool: &PgPool,
    raw_token: &str,
) -> Result<Option<InvitationRow>> {
    let prefix = slice_prefix(raw_token);
    let rows: Vec<RawRow> = sqlx::query_as(
        "SELECT id, org_id, inviter_id, email::text AS email, role, token_hash, \
                created_at, expires_at, accepted_at, declined_at \
         FROM invitations \
         WHERE token_prefix = $1 \
         AND accepted_at IS NULL AND declined_at IS NULL \
         AND expires_at > now()",
    )
    .bind(prefix)
    .fetch_all(pool)
    .await
    .context("invitations::find_pending_by_token")?;

    for r in rows {
        if token_hash::verify(raw_token, &r.token_hash) {
            return Ok(Some(InvitationRow::try_from(r)?));
        }
    }
    Ok(None)
}

impl TryFrom<RawRow> for InvitationRow {
    type Error = AppError;

    fn try_from(r: RawRow) -> Result<Self> {
        Ok(Self {
            id: r.id,
            org_id: OrgId(r.org_id),
            inviter_id: UserId(r.inviter_id),
            email: r.email,
            role: Role::from_db_str(&r.role).ok_or_else(|| {
                AppError::Other(anyhow::anyhow!(
                    "invitation row {} has unknown role {}",
                    r.id,
                    r.role
                ))
            })?,
            created_at: r.created_at,
            expires_at: r.expires_at,
            accepted_at: r.accepted_at,
            declined_at: r.declined_at,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct RawRow {
    id: Uuid,
    org_id: Uuid,
    inviter_id: Uuid,
    email: String,
    role: String,
    token_hash: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    accepted_at: Option<DateTime<Utc>>,
    declined_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InvitationListing {
    pub id: Uuid,
    pub email: String,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub declined_at: Option<DateTime<Utc>>,
}

pub async fn list_pending_for_org(pool: &PgPool, org: OrgId) -> Result<Vec<InvitationListing>> {
    let rows: Vec<InvitationListing> = sqlx::query_as(
        "SELECT id, email::text AS email, role, created_at, expires_at, accepted_at, declined_at \
         FROM invitations \
         WHERE org_id = $1 \
         AND accepted_at IS NULL AND declined_at IS NULL \
         AND expires_at > now() \
         ORDER BY created_at DESC",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .context("invitations::list_pending_for_org")?;
    Ok(rows)
}

/// Who/what to re-mail for a resend, fetched cheaply before any token mint.
pub struct ResendTarget {
    pub email: String,
    /// Original inviter — the "invited by" attribution stays stable on resend.
    pub inviter_id: UserId,
}

/// Cheap pre-check for a resend: returns the recipient + original inviter when
/// a non-consumed invite with that (id, org) exists, else `None`. Reads no
/// hash, so the caller can reject a bogus id before paying the argon2 cost.
/// Expired-but-unconsumed rows qualify — resend revives them.
pub async fn pending_for_resend(
    pool: &PgPool,
    org: OrgId,
    id: Uuid,
) -> Result<Option<ResendTarget>> {
    let row: Option<(String, Uuid)> = sqlx::query_as(
        "SELECT email::text, inviter_id FROM invitations \
         WHERE id = $1 AND org_id = $2 \
         AND accepted_at IS NULL AND declined_at IS NULL",
    )
    .bind(id)
    .bind(org.0)
    .fetch_optional(pool)
    .await
    .context("invitations::pending_for_resend")?;
    Ok(row.map(|(email, inviter_id)| ResendTarget {
        email,
        inviter_id: UserId(inviter_id),
    }))
}

/// Persist a re-issued token + refreshed expiry onto a still-pending invite.
/// The caller mints `raw_token` and **emails it first**, calling this only on
/// send success — so a failed send never overwrites the hash and the prior
/// link stays valid. `created_at` is untouched. Returns false if the row was
/// consumed/deleted in the race window between [`pending_for_resend`] and here.
pub async fn persist_resend(
    pool: &PgPool,
    org: OrgId,
    id: Uuid,
    raw_token: &str,
    expires_at: DateTime<Utc>,
) -> Result<bool> {
    let prefix = slice_prefix(raw_token).to_string();
    let hash = token_hash::hash(raw_token)?;
    let res = sqlx::query(
        "UPDATE invitations \
            SET token_hash = $3, token_prefix = $4, expires_at = $5 \
         WHERE id = $1 AND org_id = $2 \
           AND accepted_at IS NULL AND declined_at IS NULL",
    )
    .bind(id)
    .bind(org.0)
    .bind(&hash)
    .bind(&prefix)
    .bind(expires_at)
    .execute(pool)
    .await
    .context("invitations::persist_resend")?;
    Ok(res.rows_affected() > 0)
}

/// Hard-delete a still-pending invitation by id. The (id, org_id) tuple is
/// required so a sibling-org owner can't revoke another org's invitation.
pub async fn revoke(pool: &PgPool, org: OrgId, id: Uuid) -> Result<bool> {
    let res = sqlx::query(
        "DELETE FROM invitations \
         WHERE id = $1 AND org_id = $2 \
         AND accepted_at IS NULL AND declined_at IS NULL",
    )
    .bind(id)
    .bind(org.0)
    .execute(pool)
    .await
    .context("invitations::revoke")?;
    Ok(res.rows_affected() > 0)
}

/// Mark `accepted_at = now()`. Returns false if the row no longer qualified
/// (already accepted, declined, expired, or deleted) — caller surfaces that
/// as `INVITATION_INVALID`. The (id, org_id) tuple ties the mutation to the
/// org the token resolved to; a request-supplied id alone could never flip a
/// pending invite in another tenant.
pub async fn mark_accepted<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    org: OrgId,
    id: Uuid,
) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE invitations SET accepted_at = now() \
         WHERE id = $1 AND org_id = $2 \
         AND accepted_at IS NULL AND declined_at IS NULL \
         AND expires_at > now()",
    )
    .bind(id)
    .bind(org.0)
    .execute(exec)
    .await
    .context("invitations::mark_accepted")?;
    Ok(res.rows_affected() > 0)
}

pub async fn mark_declined(pool: &PgPool, org: OrgId, id: Uuid) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE invitations SET declined_at = now() \
         WHERE id = $1 AND org_id = $2 \
         AND accepted_at IS NULL AND declined_at IS NULL \
         AND expires_at > now()",
    )
    .bind(id)
    .bind(org.0)
    .execute(pool)
    .await
    .context("invitations::mark_declined")?;
    Ok(res.rows_affected() > 0)
}

/// Periodic cleanup: drop rows expired more than `keep_history_days` days ago.
/// Accepted/declined rows older than the same window are also pruned — the UI
/// surfaces recent history only. Days are bound via `make_interval` so the
/// query plan is stable and the bind avoids string concatenation.
pub async fn purge_old(pool: &PgPool, keep_history_days: i64) -> Result<u64> {
    let res = sqlx::query(
        "/* SAFE: cross-tenant retention sweep */ \
         DELETE FROM invitations \
         WHERE (accepted_at IS NOT NULL AND accepted_at < now() - make_interval(days => $1)) \
            OR (declined_at IS NOT NULL AND declined_at < now() - make_interval(days => $1)) \
            OR (accepted_at IS NULL AND declined_at IS NULL \
                AND expires_at < now() - make_interval(days => $1))",
    )
    .bind(i32::try_from(keep_history_days).unwrap_or(i32::MAX))
    .execute(pool)
    .await
    .context("invitations::purge_old")?;
    Ok(res.rows_affected())
}

/// Outcome of a successful accept — enough for the `/?joined=<slug>` bounce.
#[derive(Debug, Clone)]
pub struct AcceptedInvitation {
    pub org_id: OrgId,
    pub org_slug: String,
}

impl AcceptedInvitation {
    /// The dashboard, carrying the slug for its joined banner.
    pub fn landing_url(&self) -> String {
        format!("/?joined={}", crate::auth::url::url_encode(&self.org_slug))
    }
}

/// Org liveness + member-cap pre-flight, shared by every accept path and by
/// the magic-link bootstrap (which must fail BEFORE creating a user row —
/// `user` is None there). Returns the org + plan for the accept tail.
pub async fn validate_acceptable(
    pool: &PgPool,
    quotas: &QuotaService,
    row: &InvitationRow,
    user: Option<UserId>,
) -> Result<(Organization, std::sync::Arc<crate::domain::Plan>)> {
    // Refuse on soft-deleted org. The owner could have soft-deleted the org
    // after the invite was sent; silently adding a membership to a tombstoned
    // org would mask itself in `list_orgs_for_user`.
    let Some(org_row) = orgs_store::get_org(pool, row.org_id).await? else {
        return Err(AppError::not_found(codes::ORG_NOT_FOUND, "org not found"));
    };
    if org_row.deleted_at.is_some() {
        return Err(AppError::not_found(codes::ORG_DELETED, "org is deleted"));
    }
    // Member cap, friendly pre-check: reject an over-cap accept *before*
    // marking the invitation consumed (and before the bootstrap path creates
    // a user row), so on the common path the recipient keeps their token.
    let plan = quotas.limit_for_org(row.org_id).await?;
    quotas.check_can_add_member(row.org_id, user).await?;
    Ok((org_row, plan))
}

/// Single owner of "user X redeems pending invitation row": liveness +
/// email match + quota pre-check + mark_accepted + add_member. Used by the
/// POST endpoint, the GET landing page, and both post-login auto-accepts.
pub async fn accept_for_user(
    pool: &PgPool,
    quotas: &QuotaService,
    user_id: UserId,
    row: InvitationRow,
) -> Result<AcceptedInvitation> {
    let (org_row, plan) = validate_acceptable(pool, quotas, &row, Some(user_id)).await?;

    // Caller's email must match the invitation.
    let Some(caller_email) = crate::storage::users::live_email(pool, user_id).await? else {
        return Err(AppError::Unauthorized);
    };
    if !caller_email.eq_ignore_ascii_case(&row.email) {
        return Err(AppError::forbidden_code(
            codes::INVITATION_EMAIL_MISMATCH,
            "this invitation is for a different email address",
        ));
    }

    // One transaction for the stamp and the membership: a sign-in racing this
    // accept sees either a pending invitation or a committed membership, never
    // the gap between, and a refused seat rolls the stamp back with it. The
    // account lock comes before the row lock because account deletion takes
    // them in that order and then deletes the inviter's pending invitations.
    let mut tx = pool.begin().await.context("accept: begin")?;
    let account = crate::storage::accounts::account_for_org(&mut *tx, row.org_id).await?;
    crate::storage::locks::advisory_xact_lock(
        &mut *tx,
        &crate::storage::locks::account_lock_key(account),
    )
    .await
    .context("accept: account lock")?;
    if !mark_accepted(&mut *tx, row.org_id, row.id).await? {
        tx.rollback().await.ok();
        return Err(AppError::not_found(
            codes::INVITATION_INVALID,
            "invitation is invalid or has expired",
        ));
    }
    // actor = the redeeming user (self-onboard via invitation token). The
    // advisory-locked count in add_member is the race-safe backstop on the
    // same plan number; it only fires if a concurrent accept slipped past
    // the lockless pre-check in validate_acceptable.
    let max_members = u32::try_from(plan.max_members).unwrap_or(u32::MAX);
    let added =
        orgs_store::add_member_in_tx(&mut tx, row.org_id, user_id, user_id, row.role, max_members)
            .await?;
    if let orgs_store::AddMemberOutcome::LimitReached { current, limit } = added {
        tx.rollback().await.ok();
        // Same audit shape every quota block uses — go through the one place
        // that owns it rather than re-assembling the event by hand.
        quotas.record_block(row.org_id, Some(user_id), "max_members", current, limit);
        return Err(AppError::quota_exceeded(
            "max_members",
            current,
            limit,
            plan.id.clone(),
        ));
    }
    tx.commit().await.context("accept: commit")?;
    Ok(AcceptedInvitation {
        org_id: row.org_id,
        org_slug: org_row.slug,
    })
}

/// Login-flow wrapper: a stale/raced/over-quota invitation must never break
/// the sign-in itself.
pub async fn try_auto_accept(
    pool: &PgPool,
    quotas: &QuotaService,
    user_id: UserId,
    invitation_id: uuid::Uuid,
) -> Option<AcceptedInvitation> {
    match find_pending_by_id(pool, invitation_id).await {
        Ok(Some(row)) => match accept_for_user(pool, quotas, user_id, row).await {
            Ok(accepted) => Some(accepted),
            Err(err) => {
                tracing::warn!(error = %err, %invitation_id, "post-login invitation accept failed");
                None
            }
        },
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(error = %err, %invitation_id, "post-login invitation lookup failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_token_is_43_chars_base64url_nopad() {
        let t = generate_raw_token();
        assert_eq!(t.len(), 43);
        assert!(!t.contains('=') && !t.contains('+') && !t.contains('/'));
    }
}
