//! Stored passkeys and the short-lived state of a ceremony in flight.
//!
//! A credential is one row and one way into an account, so it sits beside the
//! linked identities on the account page and answers the same last-way-in
//! question before it can be taken away.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::domain::UserId;
use crate::domain::{CredentialAction, CredentialOrigin};
use crate::error::{AppError, Result};
use crate::storage::oauth_identities::{CredentialEvent, RequestOrigin};

/// Not an [`crate::domain::OauthProvider`]: that enum is pinned to the
/// `oauth_identities` CHECK, and no vendor is involved here.
pub const PROVIDER_SLUG: &str = "passkey";

/// Flat, not a plan tier: a passkey is not a metered resource, so the only job
/// is to stop one account writing rows without end.
pub const MAX_PER_USER: i64 = 10;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredPasskey {
    pub id: Uuid,
    pub credential_id: Vec<u8>,
    pub nickname: Option<String>,
    pub rp_id: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

impl StoredPasskey {
    /// A credential minted for another host cannot answer a ceremony on this
    /// one, so it is a row in the table rather than a way in.
    pub fn usable_from(&self, rp_id: &str) -> bool {
        self.rp_id == rp_id
    }
}

pub async fn list_for_user<'c, E: sqlx::PgExecutor<'c>>(
    executor: E,
    user: UserId,
) -> Result<Vec<StoredPasskey>> {
    sqlx::query_as(
        "SELECT id, credential_id, nickname, rp_id, created_at, last_used_at \
         FROM webauthn_credentials WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(user.0)
    .fetch_all(executor)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("list passkeys: {e}")))
}

/// Credential ids already on the account, so a registration can exclude them
/// and the authenticator offers to replace rather than silently duplicate.
pub async fn credential_ids(pool: &PgPool, user: UserId) -> Result<Vec<CredentialID>> {
    Ok(list_for_user(pool, user)
        .await?
        .into_iter()
        .map(|row| CredentialID::from(row.credential_id))
        .collect())
}

/// Keyed by user id, because that is what a discoverable assertion carries.
pub async fn passkeys_for_user(pool: &PgPool, user: UserId) -> Result<Vec<Passkey>> {
    let rows: Vec<(serde_json::Value,)> =
        sqlx::query_as("SELECT credential FROM webauthn_credentials WHERE user_id = $1")
            .bind(user.0)
            .fetch_all(pool)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("load passkeys: {e}")))?;
    rows.into_iter()
        .map(|(v,)| {
            serde_json::from_value(v)
                .map_err(|e| AppError::Other(anyhow::anyhow!("decode stored passkey: {e}")))
        })
        .collect()
}

/// Locked before counting, so two ceremonies finishing at once cannot both read
/// the same count. Must run in the caller's transaction, which is what holds it.
pub async fn ensure_room(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: UserId,
    rp_id: &str,
) -> Result<()> {
    crate::storage::locks::advisory_xact_lock(
        &mut **tx,
        &crate::storage::locks::user_lock_key(user),
    )
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("lock passkey insert: {e}")))?;
    // Only credentials this host can use count: refusing the replacement that
    // orphaned rows exist to prompt would be the wrong way round.
    let (held,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM webauthn_credentials WHERE user_id = $1 AND rp_id = $2",
    )
    .bind(user.0)
    .bind(rp_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("count passkeys: {e}")))?;
    if held >= MAX_PER_USER {
        return Err(AppError::bad_request(
            "TOO_MANY_PASSKEYS",
            format!("this account already holds {MAX_PER_USER} passkeys; remove one first"),
        ));
    }
    Ok(())
}

pub async fn insert(
    pool: &PgPool,
    user: UserId,
    passkey: &Passkey,
    rp_id: &str,
    nickname: Option<&str>,
    from: RequestOrigin<'_>,
) -> Result<()> {
    let credential = serde_json::to_value(passkey)
        .map_err(|e| AppError::Other(anyhow::anyhow!("encode passkey: {e}")))?;
    let credential_id: &[u8] = passkey.cred_id().as_ref();
    let label = credential_label(credential_id);
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("begin passkey insert: {e}")))?;
    if let Err(e) = ensure_room(&mut tx, user, rp_id).await {
        tx.rollback().await.ok();
        return Err(e);
    }
    sqlx::query(
        "INSERT INTO webauthn_credentials (user_id, credential_id, credential, rp_id, nickname) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(user.0)
    .bind(credential_id)
    .bind(&credential)
    .bind(rp_id)
    .bind(nickname)
    .execute(&mut *tx)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("insert passkey: {e}")))?;
    let change = event(&label, CredentialAction::Linked, from);
    crate::storage::oauth_identities::record_event_in_tx(&mut tx, user, change).await?;
    tx.commit()
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("commit passkey insert: {e}")))?;
    // After the commit, so nothing claims a credential arrived that did not.
    change.announce(user);
    Ok(())
}

/// The signature counter and the backup flags live inside the stored
/// credential, so a successful assertion writes the whole value back.
pub async fn record_use(pool: &PgPool, credential_id: &[u8], passkey: &Passkey) -> Result<()> {
    let credential = serde_json::to_value(passkey)
        .map_err(|e| AppError::Other(anyhow::anyhow!("encode passkey: {e}")))?;
    sqlx::query(
        "UPDATE webauthn_credentials SET credential = $2, last_used_at = now() \
         WHERE credential_id = $1",
    )
    .bind(credential_id)
    .bind(&credential)
    .execute(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("record passkey use: {e}")))?;
    Ok(())
}

/// Credentials minted for some other relying-party id. A passkey only answers
/// to the id that created it, so these are dead weight the owner cannot use.
pub async fn orphaned_by_rp_id(pool: &PgPool, rp_id: &str) -> Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM webauthn_credentials WHERE rp_id <> $1")
            .bind(rp_id)
            .fetch_one(pool)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("count orphaned passkeys: {e}")))?;
    Ok(n)
}

/// Only a hardware authenticator keeps a counter; a synced passkey reports zero
/// forever, so `Some(0)` means "no counter" rather than "never used".
pub async fn stored_counter(pool: &PgPool, credential_id: &[u8]) -> Result<Option<i64>> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT (credential->'cred'->>'counter')::bigint FROM webauthn_credentials \
         WHERE credential_id = $1",
    )
    .bind(credential_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("read passkey counter: {e}")))?;
    Ok(row.and_then(|(c,)| c))
}

/// Shared with the OAuth writers so one insert and one log line describe every
/// credential change, whoever it belongs to.
fn event<'a>(
    label: &'a str,
    action: CredentialAction,
    from: RequestOrigin<'a>,
) -> CredentialEvent<'a> {
    CredentialEvent {
        provider: PROVIDER_SLUG,
        provider_user_id: label,
        action,
        origin: CredentialOrigin::Session,
        ip_hash: from.ip_hash,
        user_agent_hash: from.user_agent_hash,
    }
}

/// The trail keeps naming a credential after its row is gone, and that column
/// is text, so the raw id travels as base64url rather than as bytes.
fn credential_label(credential_id: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(credential_id)
}

/// Refuses when it is the last thing that opens the account. Returns the address
/// so the caller can say a credential just left.
pub async fn remove(
    pool: &PgPool,
    user: UserId,
    id: Uuid,
    rp_id: Option<&str>,
    ways_in: &crate::storage::oauth_identities::WaysIn,
    from: RequestOrigin<'_>,
) -> Result<String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("begin passkey removal: {e}")))?;
    // Counting outside this lock lets two removals of different credentials
    // each see the other surviving, and an account with two ways in and no
    // third loses both. Same key `ensure_room` takes, so adds and removes
    // serialise against each other too.
    crate::storage::locks::advisory_xact_lock(
        &mut *tx,
        &crate::storage::locks::user_lock_key(user),
    )
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("lock passkey removal: {e}")))?;

    let held = list_for_user(&mut *tx, user).await?;
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
    let linked = crate::storage::oauth_identities::list_for_user(&mut *tx, user).await?;
    if !ways_in.passkey_removable(&linked, surviving) {
        return Err(AppError::bad_request(
            "LAST_SIGN_IN_METHOD",
            "that is the only thing that still opens this account",
        ));
    }

    let label = credential_label(&doomed.credential_id);
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
    let change = event(&label, CredentialAction::Unlinked, from);
    crate::storage::oauth_identities::record_event_in_tx(&mut tx, user, change).await?;
    tx.commit()
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("commit passkey removal: {e}")))?;
    change.announce(user);
    Ok(email.0)
}

// ---------------------------------------------------------------------------
// Ceremony state
// ---------------------------------------------------------------------------

/// Hashed at rest for the same reason OAuth state is: the row is a credential
/// while it lives.
pub fn generate_handle() -> String {
    crate::auth::oauth_state::generate_state()
}

pub async fn put_state<S: serde::Serialize>(
    pool: &PgPool,
    handle: &str,
    user: Option<UserId>,
    state: &S,
) -> Result<()> {
    let encoded = serde_json::to_value(state)
        .map_err(|e| AppError::Other(anyhow::anyhow!("encode ceremony state: {e}")))?;
    sqlx::query(
        "INSERT INTO webauthn_states (state_hash, user_id, state, expires_at) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(crate::auth::sha256_hex(handle))
    .bind(user.map(|u| u.0))
    .bind(&encoded)
    .bind(Utc::now() + Duration::seconds(crate::auth::passkey::CEREMONY_TTL_SECONDS))
    .execute(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("store ceremony state: {e}")))?;
    Ok(())
}

/// Deletes and returns in one statement, so two answers to the same challenge
/// cannot both proceed.
pub async fn take_state<S: serde::de::DeserializeOwned>(
    pool: &PgPool,
    handle: &str,
) -> Result<Option<(Option<UserId>, S)>> {
    let row: Option<(Option<Uuid>, serde_json::Value)> = sqlx::query_as(
        "DELETE FROM webauthn_states WHERE state_hash = $1 AND expires_at > now() \
         RETURNING user_id, state",
    )
    .bind(crate::auth::sha256_hex(handle))
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("take ceremony state: {e}")))?;
    let Some((user, state)) = row else {
        return Ok(None);
    };
    let state = serde_json::from_value(state)
        .map_err(|e| AppError::Other(anyhow::anyhow!("decode ceremony state: {e}")))?;
    Ok(Some((user.map(UserId), state)))
}

/// An abandoned ceremony is never consumed, so without this the table grows
/// forever.
pub async fn purge_expired(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query("DELETE FROM webauthn_states WHERE expires_at < now()")
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}
