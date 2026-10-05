//! The durable trail of sign-in method changes. Every credential writer, OAuth
//! or passkey, records here, so one insert and one log line describe them all.

use sqlx::PgPool;

use crate::domain::UserId;
use crate::domain::{CredentialAction, CredentialOrigin};
use crate::error::{AppError, Result};

/// Salted like `login_attempts`, so the two trails can be compared.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestOrigin<'a> {
    pub ip_hash: Option<&'a str>,
    pub user_agent_hash: Option<&'a str>,
}

/// Grouped so the writers cannot disagree about what a row needs. `provider` is
/// the slug, not [`crate::domain::OauthProvider`]: a passkey has no vendor and
/// still belongs.
#[derive(Debug, Clone, Copy)]
pub struct CredentialEvent<'a> {
    pub provider: &'a str,
    pub provider_user_id: &'a str,
    pub action: CredentialAction,
    pub origin: CredentialOrigin,
    pub ip_hash: Option<&'a str>,
    pub user_agent_hash: Option<&'a str>,
}

impl CredentialEvent<'_> {
    /// On the event, not at the call sites, so no path can record a change
    /// without saying so while an operator is watching. `provider_user_id`
    /// stays out: it is the user's identifier at a third party.
    pub(crate) fn announce(&self, user: UserId) {
        tracing::info!(
            user_id = %user.0,
            provider = self.provider,
            action = self.action.as_db_str(),
            origin = self.origin.as_db_str(),
            "sign-in method changed"
        );
        metrics::counter!(
            crate::metric_names::CREDENTIAL_CHANGES,
            "action" => self.action.as_db_str(),
            "origin" => self.origin.as_db_str(),
            "provider" => self.provider.to_string(),
        )
        .increment(1);
    }
}

/// The mail announcing a change is best-effort; this is what is left when it
/// is not delivered.
pub async fn record(pool: &PgPool, user: UserId, event: CredentialEvent<'_>) {
    let written = sqlx::query(EVENT_INSERT)
        .bind(user.0)
        .bind(event.provider)
        .bind(event.provider_user_id)
        .bind(event.action.as_db_str())
        .bind(event.origin.as_db_str())
        .bind(event.ip_hash)
        .bind(event.user_agent_hash)
        .execute(pool)
        .await;
    match written {
        Ok(_) => event.announce(user),
        Err(e) => {
            tracing::warn!(error = %e, action = event.action.as_db_str(), "credential event not recorded")
        }
    }
}

const EVENT_INSERT: &str = "INSERT INTO credential_events \
     (user_id, provider, provider_user_id, action, origin, ip_hash, user_agent_hash) \
     VALUES ($1, $2, $3, $4, $5, $6, $7)";

pub(crate) async fn record_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: UserId,
    event: CredentialEvent<'_>,
) -> Result<()> {
    sqlx::query(EVENT_INSERT)
        .bind(user.0)
        .bind(event.provider)
        .bind(event.provider_user_id)
        .bind(event.action.as_db_str())
        .bind(event.origin.as_db_str())
        .bind(event.ip_hash)
        .bind(event.user_agent_hash)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("record credential event: {e}")))?;
    Ok(())
}
