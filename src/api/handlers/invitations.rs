//! Organization invitations — owner-scoped CRUD + redeem.
//!
//! Five routes:
//! - `POST   /api/v1/orgs/{id}/invitations`     create
//! - `GET    /api/v1/orgs/{id}/invitations`     list pending
//! - `DELETE /api/v1/orgs/{id}/invitations/{i}` revoke
//! - `POST   /api/v1/invitations/accept`        accept (token in body)
//! - `POST   /api/v1/invitations/decline`       decline (token in body)
//!
//! The emailed-link HTML landing pages live in `web::views::invitations`
//! (GET accept redeems via [`inv::accept_for_user`]; GET decline renders a
//! confirm page that POSTs here). The login flows redeem carried invitation
//! ids through [`inv::try_auto_accept`]. Email-sending uses
//! [`AppState::email_sender`] so the provider stays config-driven.

use crate::request::json::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::email_norm;
use crate::auth::invitations as inv;
use crate::auth::url::token_link;
use crate::domain::{OrgId, Organization, Role, UserId};
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::request::{BrowserUser, CurrentUser, VerifiedBrowserUser};
use crate::storage::orgs as orgs_store;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateInvitationRequest {
    pub email: String,
    pub role: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InvitationView {
    pub id: Uuid,
    pub email: String,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

pub async fn create(
    State(state): State<AppState>,
    VerifiedBrowserUser(CurrentUser(user_id)): VerifiedBrowserUser,
    Path(org_id): Path<Uuid>,
    Json(req): Json<CreateInvitationRequest>,
) -> Result<(StatusCode, Json<InvitationView>)> {
    let pool = state.require_db()?;
    let org = OrgId(org_id);
    let email = validate_email(&req.email)?;
    let role = parse_role(&req.role)?;

    // Owner-only.
    if !orgs_store::is_owner(pool, user_id, org).await? {
        return Err(AppError::Forbidden);
    }

    // Org must exist and be active. is_owner returns false for soft-deleted
    // orgs too, but bail explicitly so the error is intelligible.
    let Some(org_row) = orgs_store::get_org(pool, org).await? else {
        return Err(AppError::not_found(codes::ORG_NOT_FOUND, "org not found"));
    };
    if org_row.deleted_at.is_some() {
        return Err(AppError::not_found(codes::ORG_DELETED, "org is deleted"));
    }

    // Already a member?
    if let Some(uid) = orgs_store::find_user_by_email(pool, email).await?
        && orgs_store::is_active_member(pool, uid, org).await?
    {
        return Err(AppError::conflict(
            codes::ALREADY_MEMBER,
            "user is already a member of this org",
        ));
    }
    // Last of the checks: this one costs a DNS round trip, and the cheaper ones
    // above own their own diagnostics.
    if let Some(risk) = state.undeliverable_email(email, "invite_send").await {
        return Err(risk.into_app_error("email"));
    }

    // Dedupe + pending-cap are enforced atomically inside `inv::create`
    // (one transaction, per-org advisory lock) — a pre-check here would
    // just be a racy duplicate of the real gate.
    // Cap from the plan (single source of truth). `inv::create` enforces it
    // atomically under the per-org advisory lock — same number, one gate.
    let max = u32::try_from(
        state
            .quotas
            .limit_for_org(org)
            .await?
            .max_pending_invitations,
    )
    .unwrap_or(u32::MAX);
    let expiry_hours = state.cfg.auth.invitations.expiry_hours;
    inv::ensure_send_window(pool, org, user_id).await?;
    let created = inv::create(pool, org, user_id, email, role, expiry_hours, max).await?;

    if let Err(err) = send_invitation_email(
        &state,
        pool,
        &org_row,
        user_id,
        created.row.id,
        email,
        &created.token,
        created.row.expires_at,
    )
    .await
    {
        // Roll back so the recipient isn't left with a row they can never
        // redeem (no email = no token in their inbox). The DB row is the
        // only place the token-hash lives, so deleting it removes the only
        // path to acceptance.
        if let Err(rev_err) = inv::revoke(pool, org, created.row.id).await {
            tracing::warn!(error = %rev_err, "invitation rollback failed after send error");
        }
        return Err(err);
    }

    Ok((
        StatusCode::CREATED,
        Json(InvitationView {
            id: created.row.id,
            email: created.row.email,
            role: created.row.role.as_db_str().to_string(),
            created_at: created.row.created_at,
            expires_at: created.row.expires_at,
        }),
    ))
}

pub async fn list(
    State(state): State<AppState>,
    CurrentUser(user_id): CurrentUser,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<InvitationView>>> {
    let pool = state.require_db()?;
    let org = OrgId(org_id);
    if !orgs_store::is_owner(pool, user_id, org).await? {
        return Err(AppError::Forbidden);
    }
    let rows = inv::list_pending_for_org(pool, org).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| InvitationView {
                id: r.id,
                email: r.email,
                role: r.role,
                created_at: r.created_at,
                expires_at: r.expires_at,
            })
            .collect(),
    ))
}

pub async fn revoke(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    Path((org_id, invitation_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let org = OrgId(org_id);
    if !orgs_store::is_owner(pool, user_id, org).await? {
        return Err(AppError::Forbidden);
    }
    let removed = inv::revoke(pool, org, invitation_id).await?;
    if !removed {
        return Err(AppError::not_found(
            codes::INVITATION_INVALID,
            "invitation not found",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Re-send a still-pending invitation: mails a fresh link and rotates the
/// token so the previous link stops working. Saves the owner the
/// revoke-then-re-invite two-step and revives an invite whose link expired.
/// Verified-owner gated like `create` (the email goes out under the org's
/// name); the pending cap isn't re-checked because this reissues an existing
/// invite, not a new address.
pub async fn resend(
    State(state): State<AppState>,
    VerifiedBrowserUser(CurrentUser(user_id)): VerifiedBrowserUser,
    Path((org_id, invitation_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let org = OrgId(org_id);
    if !orgs_store::is_owner(pool, user_id, org).await? {
        return Err(AppError::Forbidden);
    }
    let Some(org_row) = orgs_store::get_org(pool, org).await? else {
        return Err(AppError::not_found(codes::ORG_NOT_FOUND, "org not found"));
    };
    if org_row.deleted_at.is_some() {
        return Err(AppError::not_found(codes::ORG_DELETED, "org is deleted"));
    }
    // Cheap existence check before the token mint's argon2 cost.
    let Some(target) = inv::pending_for_resend(pool, org, invitation_id).await? else {
        return Err(AppError::not_found(
            codes::INVITATION_INVALID,
            "invitation not found",
        ));
    };
    // Joined via another path since the invite was created — nothing to resend.
    if let Some(uid) = orgs_store::find_user_by_email(pool, &target.email).await?
        && orgs_store::is_active_member(pool, uid, org).await?
    {
        return Err(AppError::conflict(
            codes::ALREADY_MEMBER,
            "user is already a member of this org",
        ));
    }

    // Mint + email FIRST, persist the rotation only on send success: a transient
    // mail error then leaves the existing link untouched instead of bricking it
    // (old hash kept, no new link delivered). The original inviter is preserved
    // so the "invited by" attribution stays stable across resends.
    inv::ensure_send_window(pool, org, user_id).await?;
    let raw = inv::generate_raw_token();
    let expires_at =
        Utc::now() + chrono::Duration::hours(i64::from(state.cfg.auth.invitations.expiry_hours));
    send_invitation_email(
        &state,
        pool,
        &org_row,
        target.inviter_id,
        invitation_id,
        &target.email,
        &raw,
        expires_at,
    )
    .await?;
    if !inv::persist_resend(pool, org, invitation_id, &raw, expires_at).await? {
        // Raced to consumed/deleted between the check and here. The fresh email
        // is out but its token was never stored, so the new link simply won't
        // verify — no membership or data effect.
        return Err(AppError::not_found(
            codes::INVITATION_INVALID,
            "invitation not found",
        ));
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenBody {
    pub token: String,
}

pub async fn accept(
    State(state): State<AppState>,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
    Json(body): Json<TokenBody>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let Some(row) = inv::find_pending_by_token(pool, body.token.trim()).await? else {
        return Err(AppError::not_found(
            codes::INVITATION_INVALID,
            "invitation is invalid or has expired",
        ));
    };
    inv::accept_for_user(pool, &state.quotas, user_id, row).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Decline doesn't require auth — anyone holding the token (the recipient
/// from the email) can decline. We never reveal whether the token matches.
pub async fn decline(
    State(state): State<AppState>,
    Json(body): Json<TokenBody>,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let Some(row) = inv::find_pending_by_token(pool, body.token.trim()).await? else {
        return Err(AppError::not_found(
            codes::INVITATION_INVALID,
            "invitation is invalid or has expired",
        ));
    };
    if !inv::mark_declined(pool, row.org_id, row.id).await? {
        return Err(AppError::not_found(
            codes::INVITATION_INVALID,
            "invitation is invalid or has expired",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn parse_role(s: &str) -> Result<Role> {
    Role::from_db_str(s.trim()).ok_or_else(|| {
        AppError::bad_request_field(
            codes::INVALID_ROLE,
            "role must be 'owner' or 'member'",
            "role",
        )
    })
}

fn validate_email(raw: &str) -> Result<&str> {
    email_norm::normalize(raw).ok_or_else(|| {
        AppError::bad_request_field(
            codes::INVALID_EMAIL,
            "email must contain '@' and be 1-254 chars",
            "email",
        )
    })
}

/// Build + send the invitation email. Shared by create and resend; the
/// caller owns the failure policy (create rolls the new row back, resend
/// leaves the pre-existing row pending).
#[allow(clippy::too_many_arguments)]
async fn send_invitation_email(
    state: &AppState,
    pool: &sqlx::PgPool,
    org_row: &Organization,
    inviter_id: UserId,
    invitation_id: Uuid,
    email: &str,
    token: &str,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    let inviter = inviter_display(pool, inviter_id).await?;
    let accept_url = action_url(state, "accept", token);
    let decline_url = action_url(state, "decline", token);
    let from = EmailAddress::new(
        state.cfg.email.from_address.clone(),
        state.cfg.email.from_name.clone(),
    );
    let to = EmailAddress::new(email.to_string(), email.to_string());
    let outgoing = TransactionalEmail {
        from,
        to,
        template: EmailTemplate::Invitation {
            org_name: org_row.name.clone(),
            inviter_display: inviter,
            accept_url,
            decline_url,
            expires_at,
        },
    };
    state.email_sender.send(outgoing).await.map_err(|err| {
        tracing::warn!(error = %err, org = %org_row.id.0, "invitation send failed");
        AppError::Other(anyhow::anyhow!("invitation send failed: {err}"))
    })?;
    if let Err(err) = inv::record_send(pool, org_row.id, inviter_id, invitation_id).await {
        tracing::warn!(error = %err, org = %org_row.id.0, "invitation send not recorded");
    }
    Ok(())
}

async fn inviter_display(pool: &sqlx::PgPool, user: crate::domain::UserId) -> Result<String> {
    let row: Option<(Option<String>, String)> = sqlx::query_as(
        "SELECT display_name, email::text FROM users WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(user.0)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("inviter lookup: {e}")))?;
    Ok(match row {
        Some((Some(name), email)) => format!("{name} <{email}>"),
        Some((None, email)) => email,
        None => "Someone".to_string(),
    })
}

// GET /invitations/accept?token=... (or /decline) is the landing-page link;
// the JSON endpoints accept the token in the body.
fn action_url(state: &AppState, kind: &str, token: &str) -> String {
    token_link(
        &state.cfg.auth.public_base_url,
        &format!("/invitations/{kind}"),
        token,
    )
}
