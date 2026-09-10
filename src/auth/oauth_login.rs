//! Provider-agnostic OAuth login materialisation (Phase C) plus the shared
//! capped-body fetch used by every provider's Phase B. Three-phase shape:
//!
//! 1. **Phase A** — consume `oauth_states` row in one statement, no upstream
//!    calls yet.
//! 2. **Phase B** — provider module exchanges `code` and fetches the remote
//!    profile into a [`RemoteIdentity`]. No DB connection held.
//! 3. **Phase C** — find-or-create user + identity, auto-create signup org
//!    for new users, all inside a fresh tx (here).

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper::body::Bytes;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::OauthProvider;
use crate::domain::{OrgId, UserId};
use crate::error::{AppError, Result};
use crate::http_outbound::OutboundHttpClient;
use crate::storage::users as users_store;

const MAX_RESPONSE_BYTES: usize = 256 * 1024;

pub(crate) const UA: &str = "uptimepage/auth";

#[derive(Debug, serde::Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: Option<String>,
    pub(crate) id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Parse an OAuth token-endpoint response body shared by every provider,
/// surfacing the endpoint's own error pair rather than a bare parse failure.
pub(crate) fn parse_token_response(body: &[u8], what: &'static str) -> Result<TokenResponse> {
    let parsed: TokenResponse = serde_json::from_slice(body)
        .map_err(|e| AppError::Other(anyhow::anyhow!("{what} parse: {e}")))?;
    if let Some(err) = parsed.error {
        let desc = parsed.error_description.unwrap_or_default();
        return Err(AppError::Other(anyhow::anyhow!("{what}: {err} ({desc})")));
    }
    Ok(parsed)
}

/// [`parse_token_response`] narrowed to the access token most providers want.
pub(crate) fn parse_access_token(body: &[u8], what: &'static str) -> Result<String> {
    parse_token_response(body, what)?
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("{what}: empty token")))
}

/// [`parse_token_response`] narrowed to the OIDC id_token.
pub(crate) fn parse_id_token(body: &[u8], what: &'static str) -> Result<String> {
    parse_token_response(body, what)?
        .id_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            AppError::Other(anyhow::anyhow!(
                "{what}: no id_token — the openid scope is missing"
            ))
        })
}

/// Payload only; the signature is deliberately unchecked. The token came back
/// over TLS from the provider's own token endpoint, so a JWKS fetch would
/// re-prove what the channel proves. Nothing reads a token from anywhere else.
pub(crate) fn decode_id_token_claims<T: serde::de::DeserializeOwned>(
    id_token: &str,
    what: &'static str,
) -> Result<T> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("{what}: not a JWT")))?;
    let raw = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| AppError::Other(anyhow::anyhow!("{what} decode: {e}")))?;
    serde_json::from_slice(&raw).map_err(|e| AppError::Other(anyhow::anyhow!("{what} parse: {e}")))
}

/// A strict bool would reject the string "true" some providers send. An
/// unrecognised shape reads as "not attested": these flags only widen trust,
/// so a surprise must cost one link, not the claim set it travels in.
pub(crate) fn de_bool_loose<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<bool>, D::Error> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Loose {
        B(bool),
        S(String),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Option::<Loose>::deserialize(d)? {
        Some(Loose::B(b)) => Some(b),
        Some(Loose::S(s)) if s.eq_ignore_ascii_case("true") => Some(true),
        Some(Loose::S(_) | Loose::Other(_)) | None => None,
    })
}

/// No attested address and no identity match: nothing to open, nothing that
/// may be created. The callback routes it back to the login page.
pub const NO_VERIFIED_EMAIL: &str = "NO_VERIFIED_EMAIL";

/// A link dance whose provider account is already somebody else's credential.
pub const IDENTITY_TAKEN: &str = "IDENTITY_TAKEN";

/// A concurrent dance claimed this provider account first — a double-clicked
/// sign-in. Routine, so the callback bounces to `/login`.
pub const IDENTITY_RACED: &str = "IDENTITY_RACED";

/// Bounds the insert/read loop against an identity being unlinked between the
/// two.
const LINK_INSERT_ATTEMPTS: u32 = 3;

/// `AlreadyLinked` covers the double-submit and the user who forgot they had
/// linked it — both benign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked,
    AlreadyLinked,
}

/// Aggregated upstream result of Phase B. `verified_email` must be None
/// unless the provider attested ownership — the email-link guard against
/// account takeover lives in that field, not in Phase C.
#[derive(Debug, Clone)]
pub struct RemoteIdentity {
    pub provider_user_id: String,
    pub provider_username: Option<String>,
    pub verified_email: Option<String>,
    pub display_name: Option<String>,
}

/// Phase C result: the resolved user + the org id their session should land
/// on. `signup_org_id` is the user's oldest active membership — for a
/// brand-new user that's the just-created signup org; for an existing user
/// it's whatever they already had. The callback stuffs this into
/// `session.active_org_id` so every subsequent request resolves a real org
/// without any global "default" fallback.
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub user_id: UserId,
    /// `None` for a fresh user whose org was deferred, and for an existing
    /// account that holds no live membership.
    pub signup_org_id: Option<OrgId>,
    pub is_new_user: bool,
    /// `deleted_at` when this sign-in landed on a soft-deleted account. The
    /// sign-in does not undo it; the caller routes to the restore choice.
    pub pending_deletion: Option<DateTime<Utc>>,
    /// This sign-in gave an existing account a credential it did not have
    /// before, so the caller mails it — an email match is invisible otherwise.
    pub newly_linked: bool,
}

/// Capped-body HTTP fetch shared by the provider modules' Phase B calls.
pub(crate) async fn fetch_limited(
    http: &OutboundHttpClient,
    req: Request<Full<Bytes>>,
    what: &'static str,
) -> Result<bytes::Bytes> {
    let resp = http
        .request(req)
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("{what} request: {e}")))?;
    let status = resp.status();
    let limited = Limited::new(resp.into_body(), MAX_RESPONSE_BYTES);
    let collected = limited
        .collect()
        .await
        .map_err(|e| AppError::Other(anyhow::anyhow!("{what} body read: {e}")))?
        .to_bytes();
    if !status.is_success() {
        let snippet = String::from_utf8_lossy(&collected);
        return Err(AppError::Other(anyhow::anyhow!(
            "{what} upstream {status}: {snippet}"
        )));
    }
    Ok(collected)
}

/// Whether a brand-new user leaves phase C owning a personal org.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignupOrg {
    Create,
    /// The sign-in carries an invitation: the org they join is the one they
    /// asked for, and a personal org is created afterwards only if the accept
    /// does not land.
    Defer,
}

/// Phase C of the callback. Find-or-create the user, link the identity, and —
/// for fresh users on [`SignupOrg::Create`] — create the signup org plus the
/// owner membership. A soft-deleted account stays deleted here and is
/// reported via `pending_deletion`. Caller follows with `session::create`.
/// All work runs inside one tx, no upstream calls.
pub async fn upsert_identity_and_signup_org(
    pool: &PgPool,
    provider: OauthProvider,
    identity: &RemoteIdentity,
    admission: crate::security::Admission,
    signup_org: SignupOrg,
) -> Result<ResolvedIdentity> {
    let mut tx = pool.begin().await.context("phase C: begin tx")?;

    // deleted_at travels with the lookup so a soft-deleted account routes to
    // the restore choice instead of into the app.
    let existing: Option<(Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT oi.user_id, u.deleted_at \
         FROM oauth_identities oi JOIN users u ON u.id = oi.user_id \
         WHERE oi.provider = $1 AND oi.provider_user_id = $2",
    )
    .bind(provider.as_db_str())
    .bind(&identity.provider_user_id)
    .fetch_optional(&mut *tx)
    .await
    .context("phase C: identity lookup")?;

    if let Some((user_id, deleted_at)) = existing {
        sqlx::query(
            "UPDATE oauth_identities SET last_login_at = now(), provider_username = $3 \
             WHERE provider = $1 AND provider_user_id = $2",
        )
        .bind(provider.as_db_str())
        .bind(&identity.provider_user_id)
        .bind(&identity.provider_username)
        .execute(&mut *tx)
        .await
        .context("phase C: bump last_login_at")?;
        tx.commit().await.context("phase C: commit (existing)")?;
        let signup_org_id = users_store::resolve_signup_org(pool, UserId(user_id)).await?;
        return Ok(ResolvedIdentity {
            user_id: UserId(user_id),
            signup_org_id,
            is_new_user: false,
            pending_deletion: deleted_at,
            newly_linked: false,
        });
    }

    let Some(email) = identity.verified_email.as_ref() else {
        return Err(AppError::bad_request(
            NO_VERIFIED_EMAIL,
            format!("{provider:?} callback: no verified email and no identity match"),
        ));
    };

    // 2. An attested address that already has an account: the provider becomes
    //    a credential for it, so someone who signed up with GitHub can use
    //    Google later. Safe to do without asking only because it is not
    //    silent — `newly_linked` mails the account, and /settings/account
    //    lists every credential with a way to remove one.
    //
    //    ::citext cast is load-bearing — sqlx binds &str as TEXT, which would
    //    select the case-sensitive operator. Tombstones included: a verified
    //    email proves ownership, so a soft-deleted account reached via a new
    //    provider resolves to that row instead of spawning a duplicate (the
    //    email unique index is partial). Active row first, then newest
    //    tombstone.
    let by_email: Option<(Uuid, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, deleted_at FROM users WHERE email = $1::citext \
          ORDER BY (deleted_at IS NULL) DESC, created_at DESC LIMIT 1",
    )
    .bind(email)
    .fetch_optional(&mut *tx)
    .await
    .context("phase C: user-by-email")?;

    if let Some((user_id, deleted_at)) = by_email {
        // Same race, and the same answer, as [`link_identity_to_user`]: a
        // conflict means a concurrent dance for this provider account got in
        // first — usually a double-clicked sign-in. Swallowing it outright
        // would sign this one in as the email-matched user while the row points
        // elsewhere, so the owner is read back; a row that has gone by then was
        // unlinked in between and the insert is worth another go.
        let mut newly_linked = false;
        let mut settled = false;
        for _ in 0..LINK_INSERT_ATTEMPTS {
            let claimed: Option<(Uuid,)> = sqlx::query_as(
                "INSERT INTO oauth_identities (user_id, provider, provider_user_id, provider_username) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (provider, provider_user_id) DO NOTHING \
                 RETURNING user_id",
            )
            .bind(user_id)
            .bind(provider.as_db_str())
            .bind(&identity.provider_user_id)
            .bind(&identity.provider_username)
            .fetch_optional(&mut *tx)
            .await
            .context("phase C: link identity")?;
            if claimed.is_some() {
                newly_linked = true;
                settled = true;
                break;
            }

            let owner: Option<(Uuid,)> = sqlx::query_as(
                "SELECT user_id FROM oauth_identities \
                  WHERE provider = $1 AND provider_user_id = $2",
            )
            .bind(provider.as_db_str())
            .bind(&identity.provider_user_id)
            .fetch_optional(&mut *tx)
            .await
            .context("phase C: link owner")?;
            match owner.map(|(o,)| o) {
                Some(o) if o == user_id => {
                    settled = true;
                    break;
                }
                Some(_) => {
                    return Err(AppError::bad_request(
                        IDENTITY_RACED,
                        format!("{provider:?} identity was claimed by another account"),
                    ));
                }
                None => continue,
            }
        }
        if !settled {
            return Err(AppError::bad_request(
                IDENTITY_RACED,
                format!("identity claimed and released {LINK_INSERT_ATTEMPTS} times running"),
            ));
        }

        sqlx::query(
            "UPDATE users SET email_verified_at = now() \
              WHERE id = $1 AND email_verified_at IS NULL",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .context("phase C: backfill verified_at")?;

        tx.commit().await.context("phase C: commit (linked)")?;
        let signup_org_id = users_store::resolve_signup_org(pool, UserId(user_id)).await?;
        return Ok(ResolvedIdentity {
            user_id: UserId(user_id),
            signup_org_id,
            is_new_user: false,
            pending_deletion: deleted_at,
            newly_linked,
        });
    }

    // 3. Brand-new user. Insert user, identity, and (unless deferred) signup
    //    org + owner membership all in this tx so a rollback leaves zero
    //    orphans.
    //
    //    The admission verdict is spent here and nowhere earlier: branches 1
    //    and 2 above are existing accounts, and an address that joined before
    //    its domain was listed must keep signing in.
    let email_risk = admission.allow_new("email", "signup_oauth")?;
    let (new_user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, display_name, email_verified_at, \
                            terms_version, privacy_version, email_risk) \
         VALUES ($1, $2, now(), $3, $4, $5) RETURNING id",
    )
    .bind(email)
    .bind(identity.display_name.as_deref())
    .bind(crate::auth::consent::TERMS_VERSION)
    .bind(crate::auth::consent::PRIVACY_VERSION)
    .bind(email_risk.map(crate::security::EmailRisk::as_db_str))
    .fetch_one(&mut *tx)
    .await
    .context("phase C: insert user")?;

    sqlx::query(
        "INSERT INTO oauth_identities (user_id, provider, provider_user_id, provider_username) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(new_user_id)
    .bind(provider.as_db_str())
    .bind(&identity.provider_user_id)
    .bind(&identity.provider_username)
    .execute(&mut *tx)
    .await
    .context("phase C: insert identity")?;

    let signup_org_id = match signup_org {
        SignupOrg::Create => {
            let org_id =
                crate::storage::orgs::create_signup_org_in_tx(&mut tx, UserId(new_user_id)).await?;
            sqlx::query("UPDATE users SET signup_org_id = $1 WHERE id = $2")
                .bind(org_id.0)
                .bind(new_user_id)
                .execute(&mut *tx)
                .await
                .context("phase C: set signup_org_id")?;
            Some(org_id)
        }
        SignupOrg::Defer => None,
    };

    tx.commit().await.context("phase C: commit (new user)")?;
    Ok(ResolvedIdentity {
        user_id: UserId(new_user_id),
        signup_org_id,
        is_new_user: true,
        pending_deletion: None,
        newly_linked: false,
    })
}

/// Attaches a provider identity to the user whose session started the dance.
/// Nothing is resolved from the provider's email here — the session already
/// proved who this is, which is the whole reason a link is safe and an
/// email match is not.
pub async fn link_identity_to_user(
    pool: &PgPool,
    provider: OauthProvider,
    identity: &RemoteIdentity,
    user_id: UserId,
) -> Result<LinkOutcome> {
    // One statement so two concurrent callbacks can't both believe they won;
    // the loser reads the owner back and finds out whose it is. A row that has
    // gone by then was unlinked in between, so the insert is worth retrying —
    // reporting it as somebody else's would be a lie about a free identity.
    for _ in 0..LINK_INSERT_ATTEMPTS {
        let inserted: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO oauth_identities (user_id, provider, provider_user_id, provider_username) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (provider, provider_user_id) DO NOTHING \
             RETURNING user_id",
        )
        .bind(user_id.0)
        .bind(provider.as_db_str())
        .bind(&identity.provider_user_id)
        .bind(&identity.provider_username)
        .fetch_optional(pool)
        .await
        .context("link: insert identity")?;

        if inserted.is_some() {
            return Ok(LinkOutcome::Linked);
        }

        let owner: Option<(Uuid,)> = sqlx::query_as(
            "SELECT user_id FROM oauth_identities WHERE provider = $1 AND provider_user_id = $2",
        )
        .bind(provider.as_db_str())
        .bind(&identity.provider_user_id)
        .fetch_optional(pool)
        .await
        .context("link: owner lookup")?;

        match owner {
            Some((owner,)) if owner == user_id.0 => return Ok(LinkOutcome::AlreadyLinked),
            Some(_) => {
                return Err(AppError::bad_request(
                    IDENTITY_TAKEN,
                    "that provider account already signs in to a different account",
                ));
            }
            None => continue,
        }
    }
    Err(AppError::Other(anyhow::anyhow!(
        "link: identity claimed and released {LINK_INSERT_ATTEMPTS} times running"
    )))
}
