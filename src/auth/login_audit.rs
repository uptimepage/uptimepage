//! Records every authentication attempt — success or failure — to power the
//! user's "recent activity" page and credential-stuffing detection.
//!
//! Writes are best-effort: a failure is logged here and never blocks a request
//! that has otherwise authenticated correctly.

use sqlx::PgPool;

use crate::domain::UserId;

#[derive(Debug, Clone, Copy)]
pub enum LoginMethod {
    GithubOauth,
    GoogleOauth,
    MicrosoftOauth,
    GitlabOauth,
    Passkey,
    ApiToken,
    MagicLink,
}

impl LoginMethod {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::GithubOauth => "github_oauth",
            Self::GoogleOauth => "google_oauth",
            Self::MicrosoftOauth => "microsoft_oauth",
            Self::GitlabOauth => "gitlab_oauth",
            Self::Passkey => "passkey",
            Self::ApiToken => "api_token",
            Self::MagicLink => "magic_link",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LoginAttempt<'a> {
    pub user_id: Option<UserId>,
    pub success: bool,
    pub ip_hash: Option<&'a str>,
    pub user_agent_hash: Option<&'a str>,
    pub failure_reason: Option<&'a str>,
}

/// The anonymous-failure shape every callback's "couldn't identify the user"
/// path writes.
pub async fn record_failure_anon(
    pool: &PgPool,
    method: LoginMethod,
    ip_hash: Option<&str>,
    user_agent_hash: Option<&str>,
    reason: &'static str,
) {
    record(
        pool,
        method,
        LoginAttempt {
            user_id: None,
            success: false,
            ip_hash,
            user_agent_hash,
            failure_reason: Some(reason),
        },
    )
    .await;
}

/// Best effort: a missed audit row is logged, never surfaced, because the
/// sign-in it describes is already decided and must not fail over it.
pub async fn record(pool: &PgPool, method: LoginMethod, attempt: LoginAttempt<'_>) {
    let written = sqlx::query(
        "INSERT INTO login_attempts \
            (user_id, method, success, ip_hash, user_agent_hash, failure_reason) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(attempt.user_id.map(|u| u.0))
    .bind(method.as_db_str())
    .bind(attempt.success)
    .bind(attempt.ip_hash)
    .bind(attempt.user_agent_hash)
    .bind(attempt.failure_reason)
    .execute(pool)
    .await;
    if let Err(err) = written {
        tracing::warn!(
            error = %err,
            method = method.as_db_str(),
            success = attempt.success,
            reason = attempt.failure_reason,
            "login_audit write failed (non-fatal)"
        );
    }
}
