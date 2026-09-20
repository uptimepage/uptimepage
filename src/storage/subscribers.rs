//! Public status-page subscribers, their double opt-in confirm tokens, and the
//! fan-out delivery log. Confirm tokens store only the SHA-256; unsubscribe is a
//! stateless HMAC so a stable link needs no persisted secret (revoke = delete).

use anyhow::Context;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::domain::{NewSubscriber, Subscriber, SubscriberChannel, public::auto_incident_title};
use crate::error::Result;
use crate::security::sha256_hex;
use crate::security::token_hash::generate_raw_token;
use crate::storage::status_pages::{PAGE_CUSTOM_DOMAIN_LIVE, PAGE_NOT_HELD, PAGE_PLAN_JOIN};

pub const CONFIRM_TTL_HOURS: i64 = 24;
/// Confirm mints per subscriber per 24 h — bounds re-subscribe mail spam.
pub const PER_SUBSCRIBER_DAILY_CAP: i64 = 3;
/// Confirm mints per page per 24 h — bounds a page fanned out across addresses.
pub const PER_PAGE_DAILY_CAP: i64 = 200;
/// Confirm mints per recipient address per 24 h across every page and org —
/// bounds mailing one victim by spreading subscribes over many pages.
pub const PER_ADDRESS_DAILY_CAP: i64 = 10;

#[derive(Debug, sqlx::FromRow)]
struct SubscriberRow {
    id: Uuid,
    status_page_id: Uuid,
    org_id: Uuid,
    channel: String,
    target: String,
    config: serde_json::Value,
    verified_at: Option<chrono::DateTime<Utc>>,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
}

impl TryFrom<SubscriberRow> for Subscriber {
    type Error = anyhow::Error;

    fn try_from(r: SubscriberRow) -> std::result::Result<Self, Self::Error> {
        let channel = SubscriberChannel::from_db_str(&r.channel)
            .with_context(|| format!("unknown subscriber channel {:?}", r.channel))?;
        Ok(Subscriber {
            id: r.id,
            status_page_id: r.status_page_id,
            org_id: r.org_id,
            channel,
            target: r.target,
            config: r.config,
            verified_at: r.verified_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

/// Upsert a pending subscription. A repeat (page, channel, target) returns the
/// existing row untouched except `updated_at`, so re-subscribing re-mints a
/// confirm token against one row instead of duplicating it. Never verifies.
pub async fn subscribe(pool: &PgPool, new: &NewSubscriber) -> Result<Subscriber> {
    let row: SubscriberRow = sqlx::query_as(
        "INSERT INTO status_page_subscribers (status_page_id, org_id, channel, target, config)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (status_page_id, channel, target)
         DO UPDATE SET updated_at = now()
         RETURNING id, status_page_id, org_id, channel, target, config, verified_at,
                   created_at, updated_at",
    )
    .bind(new.status_page_id)
    .bind(new.org_id)
    .bind(new.channel.as_db_str())
    .bind(&new.target)
    .bind(&new.config)
    .fetch_one(pool)
    .await
    .context("subscribers::subscribe")?;
    Subscriber::try_from(row).map_err(Into::into)
}

/// Mark a subscriber verified. Used by the webhook confirmation-ping path,
/// which proves URL ownership by a live 2xx rather than a mailed token.
pub async fn mark_verified(pool: &PgPool, subscriber_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE status_page_subscribers
         SET verified_at = COALESCE(verified_at, now()), updated_at = now()
         WHERE id = $1",
    )
    .bind(subscriber_id)
    .execute(pool)
    .await
    .context("subscribers::mark_verified")?;
    Ok(())
}

pub enum ConfirmMint {
    /// Raw token — embedded in the confirm URL, never persisted.
    Created {
        token: String,
    },
    LimitReached,
}

/// Mint a confirm token, enforcing both daily caps in the same statement so
/// concurrent mints can't overshoot.
pub async fn mint_confirm_token(
    pool: &PgPool,
    subscriber_id: Uuid,
    org_id: Uuid,
    status_page_id: Uuid,
    target: &str,
) -> Result<ConfirmMint> {
    let raw = generate_raw_token();
    let expires_at = Utc::now() + Duration::hours(CONFIRM_TTL_HOURS);
    let inserted: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO status_page_subscriber_tokens (subscriber_id, org_id, target, token_hash, expires_at)
         SELECT $1, $2, $3, $4, $5
         WHERE (SELECT count(*) FROM status_page_subscriber_tokens
                WHERE subscriber_id = $1 AND created_at > now() - interval '24 hours') < $6
           AND (SELECT count(*) FROM status_page_subscriber_tokens t
                JOIN status_page_subscribers s ON s.id = t.subscriber_id
                WHERE s.status_page_id = $7 AND t.created_at > now() - interval '24 hours') < $8
           AND (SELECT count(*) FROM status_page_subscriber_tokens
                WHERE target = $3 AND created_at > now() - interval '24 hours') < $9
         RETURNING id",
    )
    .bind(subscriber_id)
    .bind(org_id)
    .bind(target)
    .bind(sha256_hex(&raw))
    .bind(expires_at)
    .bind(PER_SUBSCRIBER_DAILY_CAP)
    .bind(status_page_id)
    .bind(PER_PAGE_DAILY_CAP)
    .bind(PER_ADDRESS_DAILY_CAP)
    .fetch_optional(pool)
    .await
    .context("subscribers::mint_confirm_token")?;
    Ok(match inserted {
        Some(_) => ConfirmMint::Created { token: raw },
        None => ConfirmMint::LimitReached,
    })
}

/// Atomically consume `raw_token` and mark its subscriber verified. The
/// token's target must still match the subscriber's, so an address change
/// burns older tokens. `None` for unknown/expired/used/target-mismatch —
/// callers surface one generic invalid-link page.
pub async fn confirm(pool: &PgPool, raw_token: &str) -> Result<Option<Subscriber>> {
    let row: Option<SubscriberRow> = sqlx::query_as(
        "WITH consumed AS (
             UPDATE status_page_subscriber_tokens
             SET used_at = now()
             WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
             RETURNING subscriber_id, target
         )
         UPDATE status_page_subscribers s
         SET verified_at = COALESCE(s.verified_at, now()), updated_at = now()
         FROM consumed c
         WHERE s.id = c.subscriber_id AND s.target = c.target
         RETURNING s.id, s.status_page_id, s.org_id, s.channel, s.target, s.config,
                   s.verified_at, s.created_at, s.updated_at",
    )
    .bind(sha256_hex(raw_token))
    .fetch_optional(pool)
    .await
    .context("subscribers::confirm")?;
    row.map(Subscriber::try_from)
        .transpose()
        .map_err(Into::into)
}

/// Verified audience for a page, optionally narrowed to one channel — the
/// fan-out read when an incident moves.
pub async fn list_verified(
    pool: &PgPool,
    status_page_id: Uuid,
    channel: Option<SubscriberChannel>,
) -> Result<Vec<Subscriber>> {
    let rows: Vec<SubscriberRow> = sqlx::query_as(
        "SELECT id, status_page_id, org_id, channel, target, config, verified_at,
                created_at, updated_at
         FROM status_page_subscribers
         WHERE status_page_id = $1 AND verified_at IS NOT NULL
           AND ($2::text IS NULL OR channel = $2)
         ORDER BY created_at",
    )
    .bind(status_page_id)
    .bind(channel.map(|c| c.as_db_str()))
    .fetch_all(pool)
    .await
    .context("subscribers::list_verified")?;
    rows.into_iter()
        .map(Subscriber::try_from)
        .collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
}

/// Delete a subscription; `true` if a row was removed. Idempotent so a
/// double-clicked unsubscribe link is a no-op, not an error.
pub async fn unsubscribe(pool: &PgPool, subscriber_id: Uuid) -> Result<bool> {
    let res = sqlx::query("DELETE FROM status_page_subscribers WHERE id = $1")
        .bind(subscriber_id)
        .execute(pool)
        .await
        .context("subscribers::unsubscribe")?;
    Ok(res.rows_affected() > 0)
}

/// Periodic cleanup: expired tokens, and used tokens older than 7 days.
pub async fn purge_old_tokens(pool: &PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "DELETE FROM status_page_subscriber_tokens
         WHERE expires_at < now()
            OR (used_at IS NOT NULL AND used_at < now() - INTERVAL '7 days')",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// How far back the dispatcher will reach for an unsent public update, so a
/// page going public (or a subscriber confirming) can't resurrect a long tail
/// of historical updates as a notification burst.
pub const FANOUT_LOOKBACK_HOURS: i64 = 24;
/// Failed deliveries retried up to this many times before the row is parked.
pub const FANOUT_MAX_ATTEMPTS: i32 = 3;
/// A `queued` claim older than this is treated as an orphan (worker crashed
/// mid-send) and may be re-claimed.
pub(crate) const CLAIM_ORPHAN_MINUTES: i64 = 10;

#[derive(Debug, sqlx::FromRow)]
pub struct PendingUpdate {
    pub subscriber_id: Uuid,
    pub update_id: Uuid,
    pub org_id: Uuid,
    pub channel: String,
    pub target: String,
    pub phase: String,
    pub message: String,
    pub incident_id: Uuid,
    public_title: Option<String>,
    component_name: String,
    status_at_start: String,
    pub page_name: String,
    pub slug: String,
    pub custom_domain: Option<String>,
    pub custom_domain_verified: bool,
    pub signing_secret: Option<String>,
}

impl PendingUpdate {
    /// The page's own title for the incident, so a mail about an unnarrated
    /// monitor-opened incident is not headed "Status update".
    pub fn incident_title(&self) -> String {
        self.public_title
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| auto_incident_title(&self.component_name, &self.status_at_start))
    }
}

/// Verified subscribers (email or webhook) with an unclaimed update on an
/// ever-published incident, posted since they subscribed and within the
/// lookback. The incident→page link is `incident.target_id` ∈ the page's
/// curated components. Updates flow while the incident is public; once
/// unpublished, only the `resolved` closer still fans out, so an incident taken
/// off the page before it ends doesn't strand subscribers on its last update.
pub async fn list_pending(pool: &PgPool, limit: i64) -> Result<Vec<PendingUpdate>> {
    let sql = format!(
        "SELECT s.id AS subscriber_id, u.id AS update_id, s.org_id, s.channel, s.target,
                u.phase, u.message,
                i.id AS incident_id, i.public_title, i.status_at_start,
                COALESCE(NULLIF(c.public_name, ''), t.name) AS component_name,
                COALESCE(NULLIF(sp.public_display_name, ''), sp.name) AS page_name,
                sp.slug::text AS slug,
                sp.custom_domain::text AS custom_domain,
                {PAGE_CUSTOM_DOMAIN_LIVE} AS custom_domain_verified,
                s.config ->> 'signing_secret' AS signing_secret
         FROM status_page_subscribers s
         JOIN status_pages sp ON sp.id = s.status_page_id
         {PAGE_PLAN_JOIN}
         JOIN status_page_components c
              ON c.status_page_id = s.status_page_id AND c.org_id = s.org_id
         JOIN targets t ON t.id = c.target_id AND t.org_id = c.org_id
         JOIN incidents i
              ON i.target_id = c.target_id AND i.org_id = c.org_id
         JOIN incident_updates u ON u.incident_id = i.id AND u.org_id = i.org_id
              AND (i.visibility = 'public'
                   OR (u.phase = 'resolved'
                       AND EXISTS (SELECT 1 FROM incident_events e
                                   WHERE e.incident_id = i.id AND e.kind = 'published')))
         WHERE s.channel IN ('email', 'webhook') AND s.verified_at IS NOT NULL
           AND {PAGE_NOT_HELD}
           -- The page drops a held monitor from its component list, so mailing
           -- about one would send readers to a page that shows neither the
           -- component nor the incident.
           AND t.plan_hold_at IS NULL
           AND u.posted_at >= s.verified_at
           AND u.posted_at >= now() - make_interval(hours => $2)
           AND NOT EXISTS (
               SELECT 1 FROM status_page_subscriber_deliveries n
               WHERE n.subscriber_id = s.id AND n.event_kind = 'incident_update'
                 AND n.event_ref = u.id AND n.phase = ''
                 AND (n.status = 'sent'
                   OR (n.status = 'queued' AND n.created_at > now() - make_interval(mins => $3))
                   OR (n.status = 'failed' AND n.attempts >= $4)))
         ORDER BY u.posted_at
         LIMIT $1"
    );
    let rows = sqlx::query_as::<_, PendingUpdate>(&sql)
        .bind(limit)
        .bind(FANOUT_LOOKBACK_HOURS as i32)
        .bind(CLAIM_ORPHAN_MINUTES as i32)
        .bind(FANOUT_MAX_ATTEMPTS)
        .fetch_all(pool)
        .await
        .context("subscribers::list_pending")?;
    Ok(rows)
}

#[derive(Debug, sqlx::FromRow)]
pub struct OperatorSubscriber {
    pub id: Uuid,
    pub target: String,
    pub channel: String,
    pub verified: bool,
    pub created_at: chrono::DateTime<Utc>,
}

/// Subscribers of one page for the operator roster (newest first), verified and
/// pending. Org- and page-scoped so it can't read another tenant's list.
pub async fn list_for_page(
    pool: &PgPool,
    org_id: Uuid,
    status_page_id: Uuid,
) -> Result<Vec<OperatorSubscriber>> {
    let rows = sqlx::query_as::<_, OperatorSubscriber>(
        "SELECT id, target, channel, (verified_at IS NOT NULL) AS verified, created_at
         FROM status_page_subscribers
         WHERE org_id = $1 AND status_page_id = $2
         ORDER BY created_at DESC",
    )
    .bind(org_id)
    .bind(status_page_id)
    .fetch_all(pool)
    .await
    .context("subscribers::list_for_page")?;
    Ok(rows)
}

/// Operator removal of one subscriber, scoped to its org and page so a crafted
/// id can't delete across tenants. `true` if a row was removed.
pub async fn remove_for_page(
    pool: &PgPool,
    org_id: Uuid,
    status_page_id: Uuid,
    subscriber_id: Uuid,
) -> Result<bool> {
    let res = sqlx::query(
        "DELETE FROM status_page_subscribers
         WHERE id = $1 AND org_id = $2 AND status_page_id = $3",
    )
    .bind(subscriber_id)
    .bind(org_id)
    .bind(status_page_id)
    .execute(pool)
    .await
    .context("subscribers::remove_for_page")?;
    Ok(res.rows_affected() > 0)
}

/// Verified subscriber count per page for an org, for the operator pages list.
/// Pages with no verified subscribers are absent (caller defaults them to 0).
pub async fn verified_counts(pool: &PgPool, org_id: Uuid) -> Result<Vec<(Uuid, i64)>> {
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT status_page_id, count(*) FROM status_page_subscribers
         WHERE org_id = $1 AND verified_at IS NOT NULL
         GROUP BY status_page_id",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .context("subscribers::verified_counts")?;
    Ok(rows)
}

/// Remove every email subscription for `email` (already lowercased) across all
/// pages — called on a hard bounce or spam complaint so a dead or hostile
/// address stops receiving mail. Cascades its tokens and delivery-log rows.
pub async fn remove_by_email(pool: &PgPool, email: &str) -> Result<u64> {
    let res =
        sqlx::query("DELETE FROM status_page_subscribers WHERE channel = 'email' AND target = $1")
            .bind(email)
            .execute(pool)
            .await
            .context("subscribers::remove_by_email")?;
    Ok(res.rows_affected())
}

/// Stateless one-click unsubscribe proof: HMAC-SHA256 of the subscriber id
/// keyed by the app secret, hex-encoded. Embedded in every notification's
/// unsubscribe link alongside the id; reproducible at send time without
/// storing anything.
pub fn unsubscribe_token(secret: &str, subscriber_id: Uuid) -> String {
    crate::security::mac::hmac_sha256_hex(secret.as_bytes(), &[subscriber_id.as_bytes()])
}

/// Constant-time check of a presented unsubscribe token.
pub fn verify_unsubscribe(secret: &str, subscriber_id: Uuid, presented: &str) -> bool {
    let expected = unsubscribe_token(secret, subscriber_id);
    expected.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// The one unsubscribe link every subscriber mail carries, so the confirm,
/// incident, and maintenance senders can't drift on path or param names.
pub fn unsubscribe_url(secret: &str, origin: &str, subscriber_id: Uuid) -> String {
    let mac = unsubscribe_token(secret, subscriber_id);
    format!("{origin}/subscribe/unsubscribe?s={subscriber_id}&t={mac}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(public_title: Option<&str>) -> PendingUpdate {
        PendingUpdate {
            subscriber_id: Uuid::nil(),
            update_id: Uuid::nil(),
            org_id: Uuid::nil(),
            channel: "email".into(),
            target: "a@example.com".into(),
            phase: "investigating".into(),
            message: String::new(),
            incident_id: Uuid::nil(),
            public_title: public_title.map(str::to_owned),
            component_name: "Painel Cloud".into(),
            status_at_start: "down".into(),
            page_name: "acme".into(),
            slug: "acme".into(),
            custom_domain: None,
            custom_domain_verified: false,
            signing_secret: None,
        }
    }

    #[test]
    fn an_unnarrated_incident_is_titled_the_way_the_page_titles_it() {
        assert_eq!(pending(None).incident_title(), "Painel Cloud down");
        assert_eq!(pending(Some("")).incident_title(), "Painel Cloud down");
        assert_eq!(pending(Some("API errors")).incident_title(), "API errors");
    }
}
