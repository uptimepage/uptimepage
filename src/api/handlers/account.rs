//! GDPR account endpoints: data export and deletion.
//!
//! - `GET  /api/v1/me/data-export`      — everything we hold about the caller.
//! - `DELETE /api/v1/me`                — soft-delete + 30-day grace.
//! - `POST /api/v1/me/restore`          — separate from signing in, so the only
//!   route back into the product isn't also the one that undoes leaving it.
//!
//! Cross-user / cross-org redaction is deliberate here: the export is the one
//! place that legitimately reads other members' rows, so it never reuses the
//! operator-UI listing helpers (which return co-members' email). Credentials
//! are scrubbed via [`RedactedTarget`] — handing this path a decryptable
//! `Target` is a compile error, not a review miss.

use crate::api::json::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::error::ApiError;
use crate::app::AppState;
use crate::auth::account;
use crate::domain::{UserId, strip_served_stale};
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::error::{AppError, Result};
use crate::observability::metrics::names;
use crate::storage::postgres_secrets::{RawTargetRow, RedactedTarget};
use crate::web::{BrowserUser, CurrentUser};

// ---------------------------------------------------------------------------
// Data export
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct UserDataExport {
    pub exported_at: DateTime<Utc>,
    pub user: UserExport,
    pub oauth_identities: Vec<OAuthIdentityExport>,
    pub passkeys: Vec<PasskeyExport>,
    pub sessions: Vec<SessionMetadata>,
    pub api_tokens: Vec<ApiTokenMetadata>,
    pub owned_orgs: Vec<OwnedOrgExport>,
    pub memberships: Vec<MembershipExport>,
    pub login_history: Vec<LoginAttemptExport>,
    pub credential_changes: Vec<CredentialEventExport>,
    pub audit_entries: Vec<AuditEntryExport>,
    pub mcp_write_actions: Vec<McpAuditExport>,
}

/// The credential itself never leaves: it is a public key bound to this host.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct PasskeyExport {
    pub nickname: Option<String>,
    pub rp_id: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct UserExport {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct OAuthIdentityExport {
    pub provider: String,
    pub provider_username: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct SessionMetadata {
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_hash: Option<String>,
    pub user_agent_hash: Option<String>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct ApiTokenMetadata {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OwnedOrgExport {
    pub organization: OrgExport,
    pub targets: Vec<RedactedTarget>,
    pub incidents: Vec<IncidentExport>,
    pub maintenance_windows: Vec<MaintenanceExport>,
    pub status_pages: Vec<StatusPageExport>,
    pub status_page_components: Vec<StatusPageComponentExport>,
    pub members: Vec<MemberMetadata>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct OrgExport {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct IncidentExport {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub target_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub severity: String,
    pub status_at_start: String,
    pub check_count: i32,
    pub error_sample: Option<String>,
    pub duration_secs: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct MaintenanceExport {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct StatusPageExport {
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub enabled: bool,
    pub public_display_name: Option<String>,
    pub public_about: Option<String>,
    pub public_brand_color: Option<String>,
    pub public_style: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One curated monitor on a page, with the operator-authored per-page overrides.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct StatusPageComponentExport {
    #[schema(value_type = String, format = "uuid")]
    pub status_page_id: Uuid,
    #[schema(value_type = String, format = "uuid")]
    pub target_id: Uuid,
    pub public_name: Option<String>,
    pub public_description: Option<String>,
    pub public_group: Option<String>,
    pub sort_order: i32,
}

/// Co-member of an owned org. Name + role ONLY — never email. Other members'
/// addresses are third-party PII and must not leak through one user's export.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct MemberMetadata {
    pub display_name: Option<String>,
    pub role: String,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct MembershipExport {
    #[schema(value_type = String, format = "uuid")]
    pub org_id: Uuid,
    pub slug: String,
    pub role: String,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct LoginAttemptExport {
    pub method: String,
    pub success: bool,
    pub ip_hash: Option<String>,
    pub user_agent_hash: Option<String>,
    pub failure_reason: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

/// When a sign-in method was added or removed. Outlives the identity row, so
/// a removal is still answerable for after the credential is gone.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct CredentialEventExport {
    pub provider: String,
    pub provider_user_id: String,
    pub action: String,
    /// `email_match` means nobody clicked add.
    pub origin: String,
    pub ip_hash: Option<String>,
    pub user_agent_hash: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AuditEntryExport {
    #[schema(value_type = String, format = "uuid")]
    pub org_id: Uuid,
    pub action: String,
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
}

/// One write an AI assistant made on this user's behalf over MCP. `token_id`
/// is deliberately absent: the token belongs to the connection, and the
/// `api_tokens` block above already carries what the user may see of it.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct McpAuditExport {
    #[schema(value_type = Option<String>, format = "uuid")]
    pub org_id: Option<Uuid>,
    pub tool: String,
    #[schema(value_type = Object)]
    pub arguments: serde_json::Value,
    pub outcome: String,
    pub detail: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[utoipa::path(
    get,
    path = "/api/v1/me/data-export",
    tag = "account",
    summary = "Export all personal data associated with the calling user",
    description = "Returns a JSON document containing all data linked to the \
                   authenticated user: account info, OAuth identities, session \
                   and API-token metadata (never raw values), owned orgs with \
                   their targets (credentials redacted), incidents, \
                   maintenance, status pages and their curated components, \
                   memberships, login history and audit entries. \
                   Excludes data from orgs where the user is a member but not \
                   owner.",
    responses(
        (status = 200, body = UserDataExport),
        (status = 401, body = ApiError),
    ),
)]
pub async fn data_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    user: std::result::Result<CurrentUser, AppError>,
) -> Result<Response> {
    // Export is a real browser-navigated download link, so an expired session
    // must land on login (with a return path), not dump the JSON error in the
    // tab. Programmatic callers (API token / XHR) still get the 401.
    let CurrentUser(user_id) = match user {
        Ok(u) => u,
        Err(err) => {
            if crate::api::middleware::is_browser_navigation(&headers) {
                return Ok(Redirect::to("/login?redirect_after=/settings/account").into_response());
            }
            return Err(err);
        }
    };
    let pool = state.require_db()?;
    let export = build_export(pool, user_id).await?;
    let filename = format!(
        "uptimepage-export-{}-{}.json",
        user_id.0,
        Utc::now().format("%Y-%m-%d")
    );
    let body = serde_json::to_vec_pretty(&export)
        .map_err(|e| AppError::Other(anyhow::anyhow!("data-export serialize: {e}")))?;
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/json; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        body,
    )
        .into_response())
}

/// Retention runs longer, but the whole export is serialized in memory.
const EXPORT_ACTIVITY_DAYS: u32 = 90;

/// Build the export. Export-specific queries deliberately do NOT inherit the
/// universal `deleted_at IS NULL` filter — within the purge grace a
/// soft-deleted org is still personal data the controller holds, so it is
/// included and stamped with its `deleted_at`.
async fn build_export(pool: &sqlx::PgPool, user_id: UserId) -> Result<UserDataExport> {
    let user: UserExport = sqlx::query_as(
        "SELECT id, email::text AS email, display_name, created_at, updated_at, deleted_at \
         FROM users WHERE id = $1",
    )
    .bind(user_id.0)
    .fetch_optional(pool)
    .await
    .map_err(db_err("user"))?
    .ok_or(AppError::Unauthorized)?;

    let oauth_identities: Vec<OAuthIdentityExport> = sqlx::query_as(
        "SELECT provider, provider_username, created_at, last_login_at \
         FROM oauth_identities WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("oauth_identities"))?;

    let sessions: Vec<SessionMetadata> = sqlx::query_as(
        "SELECT created_at, last_used_at, expires_at, ip_hash, user_agent_hash \
         FROM sessions WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("sessions"))?;

    let api_tokens: Vec<ApiTokenMetadata> = sqlx::query_as(
        "SELECT id, name, created_at, last_used_at, expires_at \
         FROM api_tokens WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("api_tokens"))?;

    // Orgs the caller owns — including soft-deleted-but-not-purged ones.
    let owned: Vec<OrgExport> = sqlx::query_as(
        "SELECT o.id, o.slug::text AS slug, o.name, o.created_at, o.updated_at, o.deleted_at \
         FROM organizations o \
         JOIN memberships m ON m.org_id = o.id \
         WHERE m.user_id = $1 AND m.role = 'owner' \
         ORDER BY o.created_at",
    )
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("owned_orgs"))?;

    let mut owned_orgs = Vec::with_capacity(owned.len());
    for org in owned {
        owned_orgs.push(build_owned_org(pool, org).await?);
    }

    // Memberships where the user is NOT an owner (owner orgs are exported in
    // full above). org_id + slug + role only.
    let memberships: Vec<MembershipExport> = sqlx::query_as(
        "SELECT o.id AS org_id, o.slug::text AS slug, m.role \
         FROM memberships m \
         JOIN organizations o ON o.id = m.org_id \
         WHERE m.user_id = $1 AND m.role <> 'owner' \
         ORDER BY o.created_at",
    )
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("memberships"))?;

    let passkeys: Vec<PasskeyExport> = sqlx::query_as(
        "SELECT nickname, rp_id, created_at, last_used_at \
         FROM webauthn_credentials WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("passkeys"))?;

    let login_history: Vec<LoginAttemptExport> = sqlx::query_as(&format!(
        "SELECT method, success, ip_hash, user_agent_hash, failure_reason, occurred_at \
         FROM login_attempts \
         WHERE user_id = $1 AND occurred_at > now() - INTERVAL '{EXPORT_ACTIVITY_DAYS} days' \
         ORDER BY occurred_at DESC"
    ))
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("login_history"))?;

    let credential_changes: Vec<CredentialEventExport> = sqlx::query_as(&format!(
        "SELECT provider, provider_user_id, action, origin, ip_hash, user_agent_hash, occurred_at \
         FROM credential_events \
         WHERE user_id = $1 AND occurred_at > now() - INTERVAL '{EXPORT_ACTIVITY_DAYS} days' \
         ORDER BY occurred_at DESC"
    ))
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("credential_changes"))?;

    let audit_entries: Vec<AuditEntryExport> = sqlx::query_as(&format!(
        "SELECT org_id, action, metadata, occurred_at \
         FROM org_audit_log \
         WHERE actor_id = $1 AND occurred_at > now() - INTERVAL '{EXPORT_ACTIVITY_DAYS} days' \
         ORDER BY occurred_at DESC"
    ))
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("audit_entries"))?;

    let mcp_write_actions: Vec<McpAuditExport> = sqlx::query_as(&format!(
        "SELECT org_id, tool, arguments, outcome, detail, created_at \
         FROM mcp_audit \
         WHERE user_id = $1 AND created_at > now() - INTERVAL '{EXPORT_ACTIVITY_DAYS} days' \
         ORDER BY created_at DESC"
    ))
    .bind(user_id.0)
    .fetch_all(pool)
    .await
    .map_err(db_err("mcp_write_actions"))?;

    Ok(UserDataExport {
        exported_at: Utc::now(),
        user,
        oauth_identities,
        passkeys,
        sessions,
        api_tokens,
        owned_orgs,
        memberships,
        login_history,
        credential_changes,
        audit_entries,
        mcp_write_actions,
    })
}

async fn build_owned_org(pool: &sqlx::PgPool, org: OrgExport) -> Result<OwnedOrgExport> {
    // Raw target columns — `check_spec` is whatever sits at rest (encrypted
    // envelope or no-KEK plaintext) and is NEVER decrypted; `from_row`
    // redacts it. The org's `deleted_at` stamps every target, since targets
    // carry no soft-delete column of their own.
    let target_rows: Vec<RawTargetRow> = sqlx::query_as(
        "SELECT id, name, check_spec, interval_secs, enabled, tags, \
                group_name, owner_user_id, created_at, updated_at \
         FROM targets WHERE org_id = $1 ORDER BY created_at",
    )
    .bind(org.id)
    .fetch_all(pool)
    .await
    .map_err(db_err("targets"))?;
    let targets = target_rows
        .into_iter()
        .map(|row| RedactedTarget::from_row(row, org.deleted_at))
        .collect();

    let mut incidents: Vec<IncidentExport> = sqlx::query_as(
        "SELECT id, target_id, started_at, ended_at, severity, status_at_start, \
                check_count, error_sample, duration_secs, created_at, updated_at \
         FROM incidents WHERE org_id = $1 ORDER BY started_at DESC",
    )
    .bind(org.id)
    .fetch_all(pool)
    .await
    .map_err(db_err("incidents"))?;
    for inc in &mut incidents {
        if let Some(e) = inc.error_sample.take() {
            inc.error_sample = strip_served_stale(&e).map(str::to_owned);
        }
    }

    let maintenance_windows: Vec<MaintenanceExport> = sqlx::query_as(
        "SELECT id, title, description, starts_at, ends_at, created_at, updated_at \
         FROM maintenance_windows WHERE org_id = $1 ORDER BY starts_at DESC",
    )
    .bind(org.id)
    .fetch_all(pool)
    .await
    .map_err(db_err("maintenance_windows"))?;

    let status_pages: Vec<StatusPageExport> = sqlx::query_as(
        "SELECT id, slug, name, enabled, public_display_name, public_about, \
                public_brand_color, public_style, created_at, updated_at \
         FROM status_pages WHERE org_id = $1 ORDER BY created_at",
    )
    .bind(org.id)
    .fetch_all(pool)
    .await
    .map_err(db_err("status_pages"))?;

    let status_page_components: Vec<StatusPageComponentExport> = sqlx::query_as(
        "SELECT status_page_id, target_id, public_name, public_description, \
                public_group, sort_order \
         FROM status_page_components WHERE org_id = $1 \
         ORDER BY status_page_id, sort_order",
    )
    .bind(org.id)
    .fetch_all(pool)
    .await
    .map_err(db_err("status_page_components"))?;

    // Co-members: display_name + role ONLY. A dedicated query, never the
    // operator-UI `list_members` helper (that returns email — third-party
    // PII the export must not leak).
    let members: Vec<MemberMetadata> = sqlx::query_as(
        "SELECT u.display_name, m.role \
         FROM memberships m JOIN users u ON u.id = m.user_id \
         WHERE m.org_id = $1 ORDER BY m.created_at",
    )
    .bind(org.id)
    .fetch_all(pool)
    .await
    .map_err(db_err("members"))?;

    Ok(OwnedOrgExport {
        organization: org,
        targets,
        incidents,
        maintenance_windows,
        status_pages,
        status_page_components,
        members,
    })
}

fn db_err(what: &'static str) -> impl Fn(sqlx::Error) -> AppError {
    move |e| AppError::Other(anyhow::anyhow!("data-export {what}: {e}"))
}

// ---------------------------------------------------------------------------
// Account deletion
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct DeletionConfirmation {
    pub scheduled_purge_at: DateTime<Utc>,
    /// The account can be restored (sign in, then confirm) until this instant.
    pub can_recover_until: DateTime<Utc>,
}

#[utoipa::path(
    delete,
    path = "/api/v1/me",
    tag = "account",
    summary = "Delete the calling user's account (soft delete + 30-day grace)",
    description = "Immediately deactivates the account and schedules a \
                   permanent purge after the grace period. Cancels sessions, \
                   deletes API tokens, declines pending invitations, and \
                   tombstones organisations the user solely owns. Rejects with \
                   422 OWNS_SHARED_ORGS if the user solely owns organisations \
                   that still have other members. Within the grace window the \
                   account can be restored by signing in and confirming at \
                   POST /api/v1/me/restore; signing in alone does not cancel \
                   the deletion.",
    responses(
        (status = 200, body = DeletionConfirmation),
        (status = 409, body = ApiError, description = "Account already scheduled for deletion"),
        (status = 422, body = ApiError, description = "User solely owns orgs with other members"),
    ),
)]
pub async fn delete_account(
    State(state): State<AppState>,
    cookies: tower_cookies::Cookies,
    BrowserUser(CurrentUser(user_id)): BrowserUser,
) -> Result<Json<DeletionConfirmation>> {
    let pool = state.require_db()?;
    let grace_days = state.cfg.tenancy.deletion_grace_period_days;

    let outcome = account::request_deletion(pool, user_id, grace_days).await?;

    // Nothing else in the stack observes a customer leaving.
    metrics::counter!(names::ACCOUNT_DELETIONS_REQUESTED).increment(1);
    tracing::warn!(
        user_id = %user_id.0,
        scheduled_purge_at = %outcome.grace_deadline,
        "account deletion requested"
    );

    // The deletion dropped every session, so the confirmation page has no
    // other way to read the purge date.
    crate::web::deletion_receipt::set(
        &cookies,
        outcome.grace_deadline,
        state.cfg.auth.session.cookie_secure,
        &state.cfg.auth.session.cookie_domain,
    );

    // Notify post-commit: a mail failure must not roll back the deletion the
    // user asked for. No link — restoring runs through a signed-in choice.
    let outgoing = TransactionalEmail {
        from: EmailAddress::new(
            state.cfg.email.from_address.clone(),
            state.cfg.email.from_name.clone(),
        ),
        to: EmailAddress::new(outcome.email.clone(), outcome.email.clone()),
        template: EmailTemplate::AccountDeletion {
            scheduled_purge_at: outcome.grace_deadline,
        },
    };
    if let Err(err) = state.email_sender.send(outgoing).await {
        tracing::warn!(error = %err, "account-deletion confirmation email send failed");
    }

    Ok(Json(DeletionConfirmation {
        scheduled_purge_at: outcome.grace_deadline,
        can_recover_until: outcome.grace_deadline,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/me/restore",
    tag = "account",
    summary = "Cancel a pending account deletion",
    description = "Clears the deletion stamp on the caller's account and lifts \
                   the tombstone on every organisation they solely own. \
                   Requires a session for the soft-deleted account — that is \
                   what signing in during the grace window produces. Returns \
                   409 if the account is not scheduled for deletion.",
    responses(
        (status = 204, description = "Restored"),
        (status = 401, body = ApiError, description = "No session for a soft-deleted account"),
        (status = 409, body = ApiError, description = "Account is not scheduled for deletion"),
    ),
)]
pub async fn restore_account(
    State(state): State<AppState>,
    cookies: tower_cookies::Cookies,
    pending: crate::web::PendingDeletionUser,
) -> Result<StatusCode> {
    let pool = state.require_db()?;
    let Some(outcome) = account::restore_account(pool, pending.user_id).await? else {
        return Err(AppError::conflict(
            crate::api::error::codes::ACCOUNT_NOT_DELETED,
            "This account is not scheduled for deletion.",
        ));
    };

    // Sessions minted while every org was tombstoned carry no active org and
    // would 401 on `CurrentOrg` forever; `active_org_id` is fixed at insert
    // time, so the restore has to go back and fill them in. Everything from
    // here is post-commit follow-up: the restore already happened, so no
    // failure below may turn it into an error response.
    match crate::storage::users::resolve_signup_org(pool, pending.user_id).await {
        Ok(Some(org)) => {
            match crate::auth::session::adopt_org_for_orphan_sessions(pool, pending.user_id, org)
                .await
            {
                Ok(n) => tracing::debug!(sessions = n, "restore: adopted org for orphan sessions"),
                Err(err) => tracing::warn!(error = %err, "restore: org adoption failed"),
            }
        }
        Ok(None) => {}
        Err(err) => tracing::warn!(error = %err, "restore: org resolve failed"),
    }

    tracing::info!(
        user_id = %pending.user_id.0,
        orgs = outcome.orgs.len(),
        "account restored from a pending deletion"
    );

    crate::web::flash::set(
        &cookies,
        &crate::web::flash::Flash {
            restored: true,
            invite_missed: false,
            ..Default::default()
        },
        state.cfg.auth.session.cookie_secure,
        &state.cfg.auth.session.cookie_domain,
    );

    let outgoing = TransactionalEmail {
        from: EmailAddress::new(
            state.cfg.email.from_address.clone(),
            state.cfg.email.from_name.clone(),
        ),
        to: EmailAddress::new(outcome.email.clone(), outcome.email.clone()),
        template: EmailTemplate::AccountRestored,
    };
    if let Err(err) = state.email_sender.send(outgoing).await {
        tracing::warn!(error = %err, "account-restored confirmation email send failed");
    }

    Ok(StatusCode::NO_CONTENT)
}
