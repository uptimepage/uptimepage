//! Members' calendar feeds of their own on-call shifts: one secret link per
//! membership, found by its token's hash, with a sealed copy so the page can
//! show the link again.

use anyhow::Context;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{OrgId, UserId};
use crate::error::Result;
use crate::security::Cipher;
use crate::storage::{capability_token, orgs};

/// Whose feed a token opens.
#[derive(Debug)]
pub struct FeedOwner {
    pub org: OrgId,
    pub user: UserId,
    pub org_name: String,
}

/// The link a calendar app fetches the feed `token` opens at.
pub fn feed_url(public_base_url: &str, token: &str) -> String {
    format!("{}/ical/{token}.ics", public_base_url.trim_end_matches('/'))
}

/// A member's feed as their own page shows it. No `Debug`: it holds the raw
/// token.
pub struct MemberFeed {
    pub token: String,
    pub org_name: String,
}

/// The member's feed. `None` when they have not made one, its sealed copy no
/// longer opens, or the org is deleted.
pub async fn member_feed(
    pool: &PgPool,
    cipher: Option<&Cipher>,
    org: OrgId,
    user: UserId,
) -> Result<Option<MemberFeed>> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT f.token_enc, o.name FROM on_call_feeds f \
         JOIN organizations o ON o.id = f.org_id AND o.deleted_at IS NULL \
         WHERE f.user_id = $1 AND f.org_id = $2",
    )
    .bind(user.0)
    .bind(org.0)
    .fetch_optional(pool)
    .await
    .context("on_call_feeds member_feed")?;
    Ok(row.and_then(|(sealed, org_name)| {
        capability_token::open(&sealed, cipher).map(|token| MemberFeed { token, org_name })
    }))
}

/// A new token for the member's feed. The link before it stops working.
pub async fn reset(
    pool: &PgPool,
    cipher: Option<&Cipher>,
    org: OrgId,
    user: UserId,
) -> Result<String> {
    let minted = capability_token::mint(cipher)?;
    let mut tx = pool.begin().await.context("on_call_feeds begin")?;
    sqlx::query(
        "INSERT INTO on_call_feeds (user_id, org_id, token_hash, token_enc) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (user_id, org_id) \
         DO UPDATE SET token_hash = EXCLUDED.token_hash, token_enc = EXCLUDED.token_enc",
    )
    .bind(user.0)
    .bind(org.0)
    .bind(&minted.hash)
    .bind(&minted.sealed)
    .execute(&mut *tx)
    .await
    .context("on_call_feeds reset")?;
    orgs::record_audit_tx(
        &mut tx,
        org,
        Some(user),
        "on_call_feed.reset",
        serde_json::json!({}),
    )
    .await?;
    tx.commit().await.context("on_call_feeds commit")?;
    Ok(minted.raw)
}

/// Whose feed `raw` opens. `None` alike for an unknown token and a deleted
/// org.
pub async fn resolve(pool: &PgPool, raw: &str) -> Result<Option<FeedOwner>> {
    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT f.org_id, f.user_id, o.name FROM on_call_feeds f \
         JOIN organizations o ON o.id = f.org_id AND o.deleted_at IS NULL \
         WHERE f.token_hash = $1",
    )
    .bind(capability_token::hash(raw))
    .fetch_optional(pool)
    .await
    .context("on_call_feeds resolve")?;
    Ok(row.map(|(org, user, org_name)| FeedOwner {
        org: OrgId(org),
        user: UserId(user),
        org_name,
    }))
}
