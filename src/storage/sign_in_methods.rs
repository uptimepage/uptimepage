//! Taking a sign-in method away. Each removal reads both kinds of credential
//! under the same per-user lock, so two removals cannot each count the other
//! surviving and leave the account with no way in.

use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::UserId;
use crate::domain::{CredentialAction, CredentialOrigin, OauthProvider, WaysIn};
use crate::error::{AppError, Result};
use crate::storage::credential_events::{self, CredentialEvent, RequestOrigin};
use crate::storage::{oauth_identities, passkeys};

/// `(provider, provider_user_id)`, as the lock query returns it.
type IdentityKey = (String, String);

/// `provider_user_id` narrows to one row when the same vendor is linked twice;
/// `None` removes every row for that provider. Returns the account's address,
/// so the caller can tell it a credential just left.
pub async fn unlink_identity(
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
        Some(rp) => passkeys::list_for_user(&mut *tx, user)
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

        credential_events::record_in_tx(
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

    let email = crate::storage::users::live_email(&mut *tx, user)
        .await?
        .ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "unlink identity (account email): no live user"
            ))
        })?;

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

fn db(what: &'static str) -> impl Fn(sqlx::Error) -> AppError {
    move |e| AppError::Other(anyhow::anyhow!("unlink identity ({what}): {e}"))
}

/// Refuses when it is the last thing that opens the account. Returns the address
/// so the caller can say a credential just left.
pub async fn remove_passkey(
    pool: &PgPool,
    user: UserId,
    id: Uuid,
    rp_id: Option<&str>,
    ways_in: &WaysIn,
    from: RequestOrigin<'_>,
) -> Result<String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("begin passkey removal: {e}")))?;
    // Counting outside this lock lets two removals of different credentials
    // each see the other surviving, and an account with two ways in and no
    // third loses both. Same key `passkeys::ensure_room` takes, so adds and
    // removes serialise against each other too.
    crate::storage::locks::advisory_xact_lock(
        &mut *tx,
        &crate::storage::locks::user_lock_key(user),
    )
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("lock passkey removal: {e}")))?;

    let held = passkeys::list_for_user(&mut *tx, user).await?;
    let Some(doomed) = held.iter().find(|row| row.id == id) else {
        return Err(AppError::not_found(
            "PASSKEY_NOT_FOUND",
            "no such passkey on this account",
        ));
    };
    // Same rule the account page asked before it drew the button, so the two
    // cannot answer differently.
    let surviving = held
        .iter()
        .filter(|row| row.id != id)
        .filter(|row| rp_id.is_some_and(|rp| row.usable_from(rp)))
        .count();
    let linked = oauth_identities::list_for_user(&mut *tx, user).await?;
    if !ways_in.passkey_removable(&linked, surviving) {
        return Err(AppError::bad_request(
            "LAST_SIGN_IN_METHOD",
            "that is the only thing that still opens this account",
        ));
    }

    let label = passkeys::credential_label(&doomed.credential_id);
    let email: Option<(String,)> = sqlx::query_as(
        "DELETE FROM webauthn_credentials WHERE id = $1 AND user_id = $2 \
         RETURNING (SELECT email::text FROM users WHERE id = $2)",
    )
    .bind(id)
    .bind(user.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("delete passkey: {e}")))?;
    let Some(email) = email else {
        tx.rollback().await.ok();
        return Err(AppError::not_found(
            "PASSKEY_NOT_FOUND",
            "no such passkey on this account",
        ));
    };
    let change = passkeys::event(&label, CredentialAction::Unlinked, from);
    credential_events::record_in_tx(&mut tx, user, change).await?;
    tx.commit()
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("commit passkey removal: {e}")))?;
    change.announce(user);
    Ok(email.0)
}
