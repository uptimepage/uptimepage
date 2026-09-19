//! The set of provider accounts that can open one user's account. Each row is
//! a credential, so the account page lists them and the owner can take one
//! away.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::domain::UserId;
use crate::domain::{CredentialAction, CredentialOrigin, OauthProvider};
use crate::error::{AppError, Result};

/// `(provider, provider_user_id)`, as the lock query returns it.
type IdentityKey = (String, String);

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LinkedIdentity {
    pub provider: String,
    pub provider_user_id: String,
    pub provider_username: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: DateTime<Utc>,
}

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

/// What still opens the account. `magic_link` counts only where the mail can
/// actually be delivered — the default sender writes to the log. A linked
/// provider counts only where this deployment will complete a sign-in for it;
/// one switched off answers `/auth/{p}/login` with a 404.
#[derive(Debug, Clone)]
pub struct WaysIn {
    pub enabled_providers: Vec<OauthProvider>,
    pub email_is_a_way_back: bool,
    pub passkeys_open_the_account: bool,
}

impl WaysIn {
    pub fn from_config(cfg: &crate::config::AppConfig) -> Self {
        Self {
            enabled_providers: cfg.auth.enabled_login_providers(),
            email_is_a_way_back: cfg.auth.magic_link_enabled() && cfg.email.delivers(),
            passkeys_open_the_account: crate::auth::passkey::login_enabled(&cfg.auth),
        }
    }

    /// `passkeys` is how many usable credentials would survive the removal,
    /// counted apart from the slugs because no vendor can switch one off.
    pub fn reachable_with<'a>(
        &self,
        mut remaining: impl Iterator<Item = &'a str>,
        passkeys: usize,
    ) -> bool {
        self.email_is_a_way_back
            || (self.passkeys_open_the_account && passkeys > 0)
            || remaining.any(|slug| {
                OauthProvider::from_db_str(slug)
                    .is_some_and(|p| self.enabled_providers.contains(&p))
            })
    }

    /// Two vendors can mint the same subject, so a sibling is anything that is
    /// not this exact pair. Shared with [`unlink`] so the button and the guard
    /// behind it cannot answer differently.
    pub fn removable(&self, row: &LinkedIdentity, all: &[LinkedIdentity], passkeys: usize) -> bool {
        self.reachable_with(
            all.iter()
                .filter(|o| {
                    o.provider != row.provider || o.provider_user_id != row.provider_user_id
                })
                .map(|o| o.provider.as_str()),
            passkeys,
        )
    }

    /// The same question asked from the other side: taking one passkey away
    /// leaves the linked providers plus whatever passkeys remain.
    pub fn passkey_removable(&self, all: &[LinkedIdentity], surviving_passkeys: usize) -> bool {
        self.reachable_with(all.iter().map(|o| o.provider.as_str()), surviving_passkeys)
    }
}

/// `provider_user_id` narrows to one row when the same vendor is linked twice;
/// `None` removes every row for that provider. Returns the account's address,
/// so the caller can tell it a credential just left.
pub async fn unlink(
    pool: &PgPool,
    user: UserId,
    provider: OauthProvider,
    provider_user_id: Option<&str>,
    ways_in: &WaysIn,
    rp_id: Option<&str>,
    from: RequestOrigin<'_>,
) -> Result<String> {
    let mut tx = pool.begin().await.map_err(db("begin"))?;
    // FOR UPDATE below serialises this against another unlink, but not against
    // passkey removal, which counts under this key.
    crate::storage::locks::advisory_xact_lock(
        &mut *tx,
        &crate::storage::locks::user_lock_key(user),
    )
    .await
    .map_err(db("lock identities"))?;

    // Counted here rather than passed in, so it cannot be read before the lock.
    let surviving_passkeys = match rp_id {
        Some(rp) => crate::storage::passkeys::list_for_user(&mut *tx, user)
            .await?
            .iter()
            .filter(|row| row.usable_from(rp))
            .count(),
        None => 0,
    };

    // FOR UPDATE so two concurrent removals can't each see two rows and both
    // proceed, leaving none; ORDER BY so they queue instead of deadlocking.
    let rows: Vec<IdentityKey> = sqlx::query_as(
        "SELECT provider, provider_user_id FROM oauth_identities \
          WHERE user_id = $1 ORDER BY provider, provider_user_id FOR UPDATE",
    )
    .bind(user.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(db("lock identities"))?;

    let is_doomed = |r: &IdentityKey| {
        r.0 == provider.as_db_str() && provider_user_id.is_none_or(|id| r.1 == id)
    };
    let (doomed, surviving): (Vec<&IdentityKey>, Vec<&IdentityKey>) =
        rows.iter().partition(|r| is_doomed(r));

    if doomed.is_empty() {
        return Err(AppError::not_found(
            "SIGN_IN_METHOD_NOT_FOUND",
            "no such sign-in method on this account",
        ));
    }
    // What would be left, not what is there now: without a subject this
    // removes every row for the provider, and counting the total first would
    // wave through a call that empties the account.
    if !ways_in.reachable_with(surviving.iter().map(|r| r.0.as_str()), surviving_passkeys) {
        return Err(AppError::bad_request(
            "LAST_SIGN_IN_METHOD",
            "add another sign-in method before removing this one",
        ));
    }

    for (p, subject) in doomed.iter().copied() {
        sqlx::query(
            "DELETE FROM oauth_identities \
              WHERE user_id = $1 AND provider = $2 AND provider_user_id = $3",
        )
        .bind(user.0)
        .bind(p)
        .bind(subject)
        .execute(&mut *tx)
        .await
        .map_err(db("delete identity"))?;

        record_event_in_tx(
            &mut tx,
            user,
            CredentialEvent {
                provider: provider.as_db_str(),
                provider_user_id: subject,
                action: CredentialAction::Unlinked,
                origin: CredentialOrigin::Session,
                ip_hash: from.ip_hash,
                user_agent_hash: from.user_agent_hash,
            },
        )
        .await?;
    }

    let (email,): (String,) =
        sqlx::query_as("SELECT email::text FROM users WHERE id = $1 AND deleted_at IS NULL")
            .bind(user.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(db("account email"))?;

    tx.commit().await.map_err(db("commit"))?;

    // After commit: a rollback must not leave a counter and a log line
    // claiming a removal that never happened.
    for (_, subject) in doomed.iter().copied() {
        CredentialEvent {
            provider: provider.as_db_str(),
            provider_user_id: subject,
            action: CredentialAction::Unlinked,
            origin: CredentialOrigin::Session,
            ip_hash: from.ip_hash,
            user_agent_hash: from.user_agent_hash,
        }
        .announce(user);
    }
    Ok(email)
}

/// Salted like `login_attempts`, so the two trails can be compared.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestOrigin<'a> {
    pub ip_hash: Option<&'a str>,
    pub user_agent_hash: Option<&'a str>,
}

/// Grouped so the writers cannot disagree about what a row needs. `provider` is
/// the slug, not [`OauthProvider`]: a passkey has no vendor and still belongs.
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
            crate::observability::metrics::names::CREDENTIAL_CHANGES,
            "action" => self.action.as_db_str(),
            "origin" => self.origin.as_db_str(),
            "provider" => self.provider.to_string(),
        )
        .increment(1);
    }
}

/// The mail announcing a change is best-effort; this is what is left when it
/// is not delivered.
pub async fn record_event(pool: &PgPool, user: UserId, event: CredentialEvent<'_>) {
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

pub(crate) async fn record_event_in_tx(
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
        .map_err(db("record event"))?;
    Ok(())
}

fn db(what: &'static str) -> impl Fn(sqlx::Error) -> AppError {
    move |e| AppError::Other(anyhow::anyhow!("unlink identity ({what}): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn row(provider: &str, subject: &str) -> LinkedIdentity {
        LinkedIdentity {
            provider: provider.into(),
            provider_user_id: subject.into(),
            provider_username: None,
            created_at: Utc::now(),
            last_login_at: Utc::now(),
        }
    }

    fn ways_in(enabled: &[OauthProvider], email: bool) -> WaysIn {
        WaysIn {
            enabled_providers: enabled.to_vec(),
            email_is_a_way_back: email,
            passkeys_open_the_account: true,
        }
    }

    #[test]
    fn two_vendors_sharing_a_subject_are_told_apart() {
        // Excluding a sibling by subject alone drops both rows from "what is
        // left", and every method then reads as the only one.
        let all = vec![row("github", "12345"), row("google", "12345")];
        let w = ways_in(&[OauthProvider::Github, OauthProvider::Google], false);
        assert!(w.removable(&all[0], &all, 0));
        assert!(w.removable(&all[1], &all, 0));
    }

    #[test]
    fn the_only_method_stays_unless_email_is_a_way_back() {
        let all = vec![row("github", "1")];
        assert!(!ways_in(&[OauthProvider::Github], false).removable(&all[0], &all, 0));
        assert!(ways_in(&[OauthProvider::Github], true).removable(&all[0], &all, 0));
    }

    #[test]
    fn a_method_this_deployment_cannot_sign_in_with_is_not_a_way_in() {
        // GitHub is switched off, so counting it would let the account drop
        // the one method that still works.
        let all = vec![row("github", "1"), row("gitlab", "2")];
        let w = ways_in(&[OauthProvider::Gitlab], false);
        assert!(
            !w.removable(&all[1], &all, 0),
            "gitlab is all that opens it"
        );
        assert!(w.removable(&all[0], &all, 0), "github opens nothing anyway");
    }
    #[test]
    fn a_passkey_keeps_the_last_provider_removable() {
        // Without counting it the page hides the remove button on the only
        // provider, from someone who already has another way in.
        let all = vec![row("github", "1")];
        let w = ways_in(&[OauthProvider::Github], false);
        assert!(!w.removable(&all[0], &all, 0));
        assert!(w.removable(&all[0], &all, 1));
    }

    #[test]
    fn a_passkey_does_not_count_where_the_deployment_switched_them_off() {
        let all = vec![row("github", "1")];
        let mut w = ways_in(&[OauthProvider::Github], false);
        w.passkeys_open_the_account = false;
        assert!(!w.removable(&all[0], &all, 3), "none of them can sign in");
    }

    #[test]
    fn the_last_passkey_stays_unless_something_else_opens_the_account() {
        let none: Vec<LinkedIdentity> = Vec::new();
        let w = ways_in(&[], false);
        assert!(!w.passkey_removable(&none, 0), "nothing would be left");
        assert!(w.passkey_removable(&none, 1), "a sibling passkey remains");

        let linked = vec![row("github", "1")];
        let w = ways_in(&[OauthProvider::Github], false);
        assert!(w.passkey_removable(&linked, 0), "github still opens it");
    }
}
