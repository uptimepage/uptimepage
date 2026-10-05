//! The set of provider accounts that can open one user's account. Each row is
//! a credential, so the account page lists them and the owner can take one
//! away.

use crate::domain::{LinkedIdentity, UserId};
use crate::error::{AppError, Result};

pub async fn list_for_user<'c, E: sqlx::PgExecutor<'c>>(
    executor: E,
    user: UserId,
) -> Result<Vec<LinkedIdentity>> {
    sqlx::query_as(
        "SELECT provider, provider_user_id, provider_username, created_at, last_login_at \
         FROM oauth_identities WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user.0)
    .fetch_all(executor)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("list linked identities: {e}")))
}
