//! Magic-link sign-in tokens. Schema landed in v1; the request/verify endpoints
//! are gated by `auth.enabled_methods` and inert until `"magic_link"` is added.
//!
//! Tokens are 32 cryptographically random bytes encoded as 43-char base64url-
//! no-pad. Only the argon2id hash is persisted; presenting the raw token at the
//! verify endpoint is what proves possession.
//!
//! `token_prefix` (first [`TOKEN_PREFIX_LEN`] chars of the raw token) is
//! stored alongside the hash and indexed. The lookup narrows the candidate
//! set to ~1 row via the prefix and argon2-verifies that row — without the
//! prefix every verify request would argon2-hash every live row, a
//! CPU-amplification DoS at any sustained request rate.

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::security::token_hash::{self, slice_prefix};
use crate::storage::locks::{advisory_xact_lock, magic_link_send_lock_key};

/// Which half of the mail signed the reader in. The two variants are the only
/// values the `redeemed_via` check constraint admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemedVia {
    Link,
    Code,
}

impl RedeemedVia {
    pub fn as_str(self) -> &'static str {
        match self {
            RedeemedVia::Link => "link",
            RedeemedVia::Code => "code",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MagicLinkRow {
    pub id: Uuid,
    pub email: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub redirect_after: Option<String>,
    pub invitation_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct CreatedMagicLink {
    pub row: MagicLinkRow,
    /// Raw 43-char base64url token — embedded in the verify URL emailed to the
    /// user. Never persisted, never recoverable after this call returns.
    pub token: String,
    /// Printed in the same mail as the link.
    pub code: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NewMagicLink<'a> {
    pub email: &'a str,
    pub ip_hash: Option<&'a str>,
    pub expiry_minutes: u32,
    pub redirect_after: Option<&'a str>,
    pub invitation_id: Option<Uuid>,
    /// `None` for a link minted outside the sign-in form, which carries no
    /// redeemable code and which [`supersede_others`] leaves alone.
    pub nonce: Option<&'a str>,
}

/// INSERT a magic-link row. `expires_at = now() + expiry_minutes`. The raw
/// token only exists in the returned struct; the DB stores its argon2id hash
/// plus a 16-char prefix for indexed lookup.
pub async fn create(pool: &PgPool, new: NewMagicLink<'_>) -> Result<CreatedMagicLink> {
    let NewMagicLink {
        email,
        ip_hash,
        expiry_minutes,
        redirect_after,
        invitation_id,
        nonce,
    } = new;
    let raw = token_hash::generate_raw_token();
    let prefix = slice_prefix(&raw).to_string();
    let hash = token_hash::hash(&raw)?;
    let code = generate_code();
    let code_hash = token_hash::hash(&code)?;
    let expires_at = Utc::now() + Duration::minutes(i64::from(expiry_minutes));
    let row: (Uuid, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO magic_link_tokens \
             (email, token_hash, token_prefix, expires_at, ip_hash, redirect_after, \
              invitation_id, code_hash, nonce_hash) \
         VALUES ($1::citext, $2, $3, $4, $5, $6, $7, $8, $9) \
         RETURNING id, created_at, expires_at",
    )
    .bind(email)
    .bind(&hash)
    .bind(&prefix)
    .bind(expires_at)
    .bind(ip_hash)
    .bind(redirect_after)
    .bind(invitation_id)
    .bind(&code_hash)
    .bind(nonce.map(crate::security::sha256_hex))
    .fetch_one(pool)
    .await
    .context("magic_link::create")?;

    Ok(CreatedMagicLink {
        code,
        row: MagicLinkRow {
            id: row.0,
            email: email.to_string(),
            created_at: row.1,
            expires_at: row.2,
            redirect_after: redirect_after.map(str::to_string),
            invitation_id,
        },
        token: raw,
    })
}

/// Crockford base32: `I`, `L`, `O` and `U` are absent, and the first three
/// fold onto the digits they are mistaken for.
const CODE_ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const CODE_LEN: usize = 6;

fn generate_code() -> String {
    use rand::TryRng;
    let mut bytes = [0u8; CODE_LEN];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for a sign-in code");
    // 256 is a multiple of 32, so the modulo is uniform over the alphabet.
    bytes
        .iter()
        .map(|b| CODE_ALPHABET[usize::from(*b) % CODE_ALPHABET.len()] as char)
        .collect()
}

/// A pasted newline or a typed `O` must not read as a wrong guess.
pub fn normalize_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

/// Shape only, so a paste accident never spends the single attempt.
pub fn code_is_well_formed(normalized: &str) -> bool {
    normalized.len() == CODE_LEN && normalized.bytes().all(|b| CODE_ALPHABET.contains(&b))
}

/// First live (unused, unexpired, hash-matching) row for `raw_token`, or
/// `None`. Read-only: does NOT mark the row used. Lookup is bounded by the
/// indexed `token_prefix` (96-bit prefix entropy), then argon2-verified.
/// Shared by [`peek`] and [`consume`].
async fn find_live_candidate(pool: &PgPool, raw_token: &str) -> Result<Option<RawRow>> {
    let prefix = slice_prefix(raw_token);
    let candidates: Vec<RawRow> = sqlx::query_as(
        "SELECT id, email::text AS email, token_hash, created_at, expires_at, \
                redirect_after, invitation_id \
         FROM magic_link_tokens \
         WHERE token_prefix = $1 AND used_at IS NULL AND superseded_at IS NULL \
           AND expires_at > now()",
    )
    .bind(prefix)
    .fetch_all(pool)
    .await
    .context("magic_link: select candidates")?;
    Ok(candidates
        .into_iter()
        .find(|r| token_hash::verify(raw_token, &r.token_hash)))
}

/// Read-only validity check for the GET confirmation page: is there an unused,
/// unexpired token matching `raw_token`? Returns its row WITHOUT marking it
/// used, so a mail link-scanner's prefetch of the confirm page cannot burn the
/// token; authoritative single-use redemption is [`consume`], on the POST.
pub async fn peek(pool: &PgPool, raw_token: &str) -> Result<Option<MagicLinkRow>> {
    Ok(find_live_candidate(pool, raw_token)
        .await?
        .map(RawRow::into_row))
}

/// Find and atomically mark-used the row matching `raw_token`. Returns the
/// row on success; `None` for any of: nothing matched, expired, already used,
/// deleted. Callers must not distinguish; surface a single indistinguishable
/// invalid-link page.
///
/// Mark-used is a follow-up UPDATE keyed by `id` with a `used_at IS NULL`
/// guard so a concurrent verify of the same token loses the race.
pub async fn consume(pool: &PgPool, raw_token: &str) -> Result<Option<MagicLinkRow>> {
    let Some(r) = find_live_candidate(pool, raw_token).await? else {
        return Ok(None);
    };
    let updated = sqlx::query(
        "UPDATE magic_link_tokens SET used_at = now(), redeemed_via = $2 \
         WHERE id = $1 AND used_at IS NULL",
    )
    .bind(r.id)
    .bind(RedeemedVia::Link.as_str())
    .execute(pool)
    .await
    .context("magic_link::consume: mark used")?;
    if updated.rows_affected() == 0 {
        // Lost the mark-used race; treat as already consumed.
        return Ok(None);
    }
    Ok(Some(r.into_row()))
}

/// Stamp `sent_at` on this row only if no mail for the address went out inside
/// the window, and report whether it won. Serialised on the address, so
/// parallel requests elect a single sender between them rather than each
/// reading an empty window and all mailing. `window_seconds = 0` disables the
/// throttle but still stamps: an unstamped row's code is unredeemable.
///
/// Claimed before the send, not after: the alternative leaves the whole SMTP
/// round trip as a gap for the next request to walk through. A send that then
/// fails hands the window back via [`release_send`].
pub async fn claim_send(pool: &PgPool, id: Uuid, email: &str, window_seconds: u32) -> Result<bool> {
    if window_seconds == 0 {
        sqlx::query("UPDATE magic_link_tokens SET sent_at = now() WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .context("magic_link::claim_send: stamp")?;
        return Ok(true);
    }
    let mut tx = pool
        .begin()
        .await
        .context("magic_link::claim_send: begin")?;
    // Two UPDATEs on different rows take different row locks and never conflict,
    // so the subquery alone lets every concurrent request read an empty window.
    advisory_xact_lock(&mut *tx, &magic_link_send_lock_key(email))
        .await
        .context("magic_link::claim_send: lock")?;
    let claimed: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE magic_link_tokens SET sent_at = now() \
         WHERE id = $1 AND sent_at IS NULL \
           AND NOT EXISTS ( \
             SELECT 1 FROM magic_link_tokens \
             WHERE email = $2::citext \
               AND sent_at > now() - make_interval(secs => $3) \
           ) \
         RETURNING id",
    )
    .bind(id)
    .bind(email)
    .bind(i32::try_from(window_seconds).unwrap_or(i32::MAX))
    .fetch_optional(&mut *tx)
    .await
    .context("magic_link::claim_send")?;
    tx.commit()
        .await
        .context("magic_link::claim_send: commit")?;
    Ok(claimed.is_some())
}

/// Hand the window back when the mail never left, so a transient SMTP failure
/// does not suppress the reader's next attempt.
pub async fn release_send(pool: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("UPDATE magic_link_tokens SET sent_at = NULL WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .context("magic_link::release_send")?;
    Ok(())
}
#[derive(Debug, sqlx::FromRow)]
struct RawRow {
    id: Uuid,
    email: String,
    token_hash: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    redirect_after: Option<String>,
    invitation_id: Option<Uuid>,
}
impl RawRow {
    fn into_row(self) -> MagicLinkRow {
        MagicLinkRow {
            id: self.id,
            email: self.email,
            created_at: self.created_at,
            expires_at: self.expires_at,
            redirect_after: self.redirect_after,
            invitation_id: self.invitation_id,
        }
    }
}

/// Only rows strictly older than the keeper are retired, so two sends that
/// raced cannot retire each other and leave the reader with nothing live.
/// `created_at` is transaction-start time and ties are real, so `id` breaks
/// them into a total order rather than letting both sides match.
///
/// `nonce_hash IS NULL` spares links minted outside the sign-in form: the
/// first-run owner link is printed to the console once, and an anonymous
/// request for that address must not be able to retire it.
pub async fn supersede_others(pool: &PgPool, email: &str, keep: Uuid) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE magic_link_tokens SET superseded_at = now() \
         WHERE email = $1::citext AND id <> $2 \
           AND nonce_hash IS NOT NULL \
           AND used_at IS NULL AND superseded_at IS NULL \
           AND (created_at, id) \
               < (SELECT created_at, id FROM magic_link_tokens WHERE id = $2)",
    )
    .bind(email)
    .bind(keep)
    .execute(pool)
    .await
    .context("magic_link::supersede_others")?;
    Ok(res.rows_affected())
}

/// One answer for spent, unmatched, expired and wrong-browser, so entering
/// codes cannot probe.
pub enum CodeOutcome {
    Ok(MagicLinkRow),
    Refused,
    /// Not a code at all, so nothing was spent and the browser keeps its
    /// binding. Distinct from [`CodeOutcome::Refused`] only to the caller;
    /// the reader sees one answer either way.
    Malformed,
}

/// One attempt: a well-formed miss retires the code and leaves the link in
/// the same mail redeemable. `code_spent_at` marks the attempt, not its
/// outcome, so it is stamped before the guess is checked.
pub async fn consume_code(pool: &PgPool, nonce: &str, code: &str) -> Result<CodeOutcome> {
    let normalized = normalize_code(code);
    if !code_is_well_formed(&normalized) {
        return Ok(CodeOutcome::Malformed);
    }
    let Some(r) = sqlx::query_as::<_, RawCodeRow>(
        "SELECT id, email::text AS email, created_at, expires_at, \
                redirect_after, invitation_id \
         FROM magic_link_tokens \
         WHERE nonce_hash = $1 AND used_at IS NULL AND superseded_at IS NULL \
           AND code_spent_at IS NULL AND sent_at IS NOT NULL \
           AND code_hash IS NOT NULL AND expires_at > now() \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(crate::security::sha256_hex(nonce))
    .fetch_optional(pool)
    .await
    .context("magic_link::consume_code: lookup")?
    else {
        return Ok(CodeOutcome::Refused);
    };

    // Claimed before it is checked, so parallel guesses against one row get one
    // attempt between them rather than one each. Losing this race is the same
    // refusal as guessing wrong, and costs no argon2.
    let claimed: Option<(Option<String>,)> = sqlx::query_as(
        "UPDATE magic_link_tokens SET code_spent_at = now() \
         WHERE id = $1 AND code_spent_at IS NULL \
         RETURNING code_hash",
    )
    .bind(r.id)
    .fetch_optional(pool)
    .await
    .context("magic_link::consume_code: spend")?;
    let Some((Some(hash),)) = claimed else {
        return Ok(CodeOutcome::Refused);
    };
    if !token_hash::verify(&normalized, &hash) {
        return Ok(CodeOutcome::Refused);
    }

    let consumed = sqlx::query(
        "UPDATE magic_link_tokens SET used_at = now(), redeemed_via = $2 \
         WHERE id = $1 AND used_at IS NULL",
    )
    .bind(r.id)
    .bind(RedeemedVia::Code.as_str())
    .execute(pool)
    .await
    .context("magic_link::consume_code: consume")?;
    if consumed.rows_affected() == 0 {
        return Ok(CodeOutcome::Refused);
    }
    Ok(CodeOutcome::Ok(MagicLinkRow {
        id: r.id,
        email: r.email,
        created_at: r.created_at,
        expires_at: r.expires_at,
        redirect_after: r.redirect_after,
        invitation_id: r.invitation_id,
    }))
}

#[derive(Debug, sqlx::FromRow)]
struct RawCodeRow {
    id: Uuid,
    email: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    redirect_after: Option<String>,
    invitation_id: Option<Uuid>,
}
/// A row outlives its own expiry by [`RETENTION_DAYS`]: `redeemed_via` and
/// `superseded_at` answer "did anyone open this, and how" during a takeover
/// review, and a token that expires in minutes would take the answer with it.
/// Expiry alone decides redeemability; this only decides how long the record
/// survives it.
const RETENTION_DAYS: i32 = 7;

pub async fn purge_old(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "DELETE FROM magic_link_tokens \
         WHERE expires_at < now() - make_interval(days => $1)",
    )
    .bind(RETENTION_DAYS)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_generated_code_survives_its_own_normalizer() {
        for _ in 0..200 {
            // Folding runs on every entry, so an alphabet carrying a glyph the
            // folder rewrites would mint codes that can never verify.
            let code = generate_code();
            assert_eq!(normalize_code(&code), code, "{code}");
            assert!(code_is_well_formed(&code), "{code}");
        }
    }

    #[test]
    fn the_alphabet_leaves_out_the_glyphs_the_folder_rewrites() {
        // Anything but 32 puts modulo bias back into generate_code.
        assert_eq!(CODE_ALPHABET.len(), 32);
        for confusable in [b'I', b'L', b'O', b'U'] {
            assert!(
                !CODE_ALPHABET.contains(&confusable),
                "{}",
                confusable as char
            );
        }
    }

    #[test]
    fn the_generator_reaches_every_symbol() {
        let mut seen = HashSet::new();
        for _ in 0..500 {
            seen.extend(generate_code().into_bytes());
        }
        assert_eq!(seen.len(), CODE_ALPHABET.len());
    }

    #[test]
    fn a_reader_retyping_the_code_is_not_a_wrong_guess() {
        assert_eq!(normalize_code(" 4kp-9rt\n"), "4KP9RT");
        assert_eq!(normalize_code("OIL"), "011");
    }

    #[test]
    fn shape_alone_decides_whether_an_attempt_is_spent() {
        assert!(code_is_well_formed("4KP9RT"));
        assert!(!code_is_well_formed("4KP9R"));
        assert!(!code_is_well_formed("4KP9RT7"));
        assert!(!code_is_well_formed("4KP9RU"));
    }
}
