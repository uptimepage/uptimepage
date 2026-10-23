//! Public status-page subscribers, their double opt-in confirm tokens, and the
//! fan-out delivery log. Confirm tokens store only the SHA-256; unsubscribe is a
//! stateless HMAC so a stable link needs no persisted secret (revoke = delete).

use anyhow::Context;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::domain::{Locale, NewSubscriber, Subscriber, SubscriberChannel};
use crate::error::Result;
use crate::i18n::Tr;
use crate::security::sha256_hex;
use crate::security::token_hash::generate_raw_token;
use crate::storage::page_sql::{PAGE_PLAN_JOIN, PAGE_SERVED, PageRow, page_row_columns};

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

/// A signed-in viewer of a private page subscribing their own address. The
/// sign-in already proved the address, so it is verified at once.
///
/// The viewer row is held `FOR SHARE` to commit, so a block (which updates it)
/// either waits and then deletes this subscription, or goes first and is seen
/// here. `false` when the viewer is blocked or gone.
pub async fn subscribe_viewer(
    pool: &PgPool,
    page: crate::domain::PageRef,
    viewer: Uuid,
    email: &str,
) -> Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .context("subscribers::subscribe_viewer begin")?;
    let blocked: Option<(bool,)> = sqlx::query_as(
        "SELECT blocked_at IS NOT NULL FROM status_page_viewers \
         WHERE id = $1 AND status_page_id = $2 AND org_id = $3 FOR SHARE",
    )
    .bind(viewer)
    .bind(page.page.0)
    .bind(page.org.0)
    .fetch_optional(&mut *tx)
    .await
    .context("subscribers::subscribe_viewer lock")?;
    if blocked != Some((false,)) {
        tx.rollback().await.ok();
        return Ok(false);
    }
    // A subscription from before the page went private keeps the address as
    // it was typed; sign-in already tied it to this viewer, so reuse it.
    let claimed = sqlx::query(
        "UPDATE status_page_subscribers \
         SET verified_at = COALESCE(verified_at, now()), updated_at = now() \
         WHERE status_page_id = $1 AND org_id = $2 AND viewer_id = $3 AND channel = 'email'",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .bind(viewer)
    .execute(&mut *tx)
    .await
    .context("subscribers::subscribe_viewer claimed")?;
    if claimed.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO status_page_subscribers \
                 (status_page_id, org_id, channel, target, config, verified_at, viewer_id) \
             VALUES ($1, $2, 'email', $3, '{}'::jsonb, now(), $4) \
             ON CONFLICT (status_page_id, channel, target) DO UPDATE SET \
                 verified_at = COALESCE(status_page_subscribers.verified_at, now()), \
                 viewer_id = EXCLUDED.viewer_id, updated_at = now() \
             WHERE status_page_subscribers.org_id = EXCLUDED.org_id",
        )
        .bind(page.page.0)
        .bind(page.org.0)
        .bind(email)
        .bind(viewer)
        .execute(&mut *tx)
        .await
        .context("subscribers::subscribe_viewer")?;
    }
    tx.commit()
        .await
        .context("subscribers::subscribe_viewer commit")?;
    Ok(true)
}

/// A visitor signed in with the page password subscribing by email. Nothing
/// proved the address, so it waits for the double opt-in like a public
/// subscription. The row records which password let it in; an address that
/// already has one keeps its own.
///
/// Under a shared lock on the access row, the visitor's session is read
/// again by a statement after it: a new password or turning it off holds the
/// row exclusively and deletes those sessions, so a subscribe that queued
/// behind it finds none, and one that went first is pruned by it. `None` when
/// the session or the password way in is gone.
pub async fn subscribe_with_password(
    pool: &PgPool,
    page: crate::domain::PageRef,
    session_hash: &str,
    new: &NewSubscriber,
) -> Result<Option<Subscriber>> {
    let mut tx = pool
        .begin()
        .await
        .context("subscribers::subscribe_with_password begin")?;
    crate::storage::page_access::password::lock_page_first(&mut tx, page).await?;
    let access: Option<(bool, bool, Option<chrono::DateTime<Utc>>)> = sqlx::query_as(
        "SELECT protected, password_enabled, password_set_at FROM status_page_access \
         WHERE status_page_id = $1 AND org_id = $2 FOR SHARE",
    )
    .bind(page.page.0)
    .bind(page.org.0)
    .fetch_optional(&mut *tx)
    .await
    .context("subscribers::subscribe_with_password lock")?;
    let Some((true, true, Some(set_at))) = access else {
        tx.rollback().await.ok();
        return Ok(None);
    };
    let (live,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM status_page_sessions \
                        WHERE id_hash = $1 AND status_page_id = $2 AND org_id = $3 \
                          AND method = 'password' \
                          AND (expires_at IS NULL OR expires_at > now()))",
    )
    .bind(session_hash)
    .bind(page.page.0)
    .bind(page.org.0)
    .fetch_one(&mut *tx)
    .await
    .context("subscribers::subscribe_with_password session")?;
    if !live {
        tx.rollback().await.ok();
        return Ok(None);
    }
    let row: Option<SubscriberRow> = sqlx::query_as(
        "INSERT INTO status_page_subscribers \
             (status_page_id, org_id, channel, target, config, via_password_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (status_page_id, channel, target) DO UPDATE SET updated_at = now() \
         WHERE status_page_subscribers.org_id = EXCLUDED.org_id \
         RETURNING id, status_page_id, org_id, channel, target, config, verified_at, \
                   created_at, updated_at",
    )
    .bind(new.status_page_id)
    .bind(new.org_id)
    .bind(new.channel.as_db_str())
    .bind(&new.target)
    .bind(&new.config)
    .bind(set_at)
    .fetch_optional(&mut *tx)
    .await
    .context("subscribers::subscribe_with_password insert")?;
    tx.commit()
        .await
        .context("subscribers::subscribe_with_password commit")?;
    row.map(Subscriber::try_from)
        .transpose()
        .map_err(Into::into)
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

/// The language of the page a subscriber follows, which may no longer be
/// published on any host.
pub async fn page_locale(pool: &PgPool, subscriber_id: Uuid) -> Result<Option<Locale>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT sp.public_locale FROM status_page_subscribers s
         JOIN status_pages sp ON sp.id = s.status_page_id AND sp.org_id = s.org_id
         WHERE s.id = $1",
    )
    .bind(subscriber_id)
    .fetch_optional(pool)
    .await
    .context("subscribers::page_locale")?;
    Ok(row.map(|(l,)| Locale::from_db(&l)))
}

/// Periodic cleanup: subscriptions left unconfirmed for a day with no live
/// link, expired tokens, and used tokens older than 7 days.
pub async fn purge_old_tokens(pool: &PgPool) -> sqlx::Result<u64> {
    let unconfirmed = sqlx::query(
        "DELETE FROM status_page_subscribers s
         WHERE s.verified_at IS NULL
           AND s.updated_at < now() - INTERVAL '1 day'
           AND NOT EXISTS (SELECT 1 FROM status_page_subscriber_tokens t
                            WHERE t.subscriber_id = s.id AND t.expires_at >= now())",
    )
    .execute(pool)
    .await?;
    let tokens = sqlx::query(
        "DELETE FROM status_page_subscriber_tokens
         WHERE expires_at < now()
            OR (used_at IS NOT NULL AND used_at < now() - INTERVAL '7 days')",
    )
    .execute(pool)
    .await?;
    Ok(unconfirmed.rows_affected() + tokens.rows_affected())
}

/// How far back the planner reaches for something to tell, so a page going
/// public (or a subscriber confirming) can't resurrect a long tail of history
/// as a notification burst.
pub const FANOUT_LOOKBACK_HOURS: i64 = 24;

/// An incident update a page's subscribers have not been told yet.
#[derive(Debug, sqlx::FromRow)]
pub struct UpdateNoticeRow {
    #[sqlx(flatten)]
    pub page: PageRow,
    pub update_id: Uuid,
    pub phase: String,
    pub message: String,
    pub generated: bool,
    pub incident_id: Uuid,
    pub origin: String,
    /// Reaches the page through one of its components, as opposed to a page
    /// that kept the incident after its monitor left.
    via_component: bool,
    /// One of the page's outages tells this incident's end.
    via_outage: bool,
    public_title: Option<String>,
    component_name: String,
    status_at_start: String,
}

impl UpdateNoticeRow {
    pub fn locale(&self) -> Locale {
        Locale::from_db(&self.page.public_locale)
    }

    /// A monitor incident's own words come from the platform; a page that
    /// shows its monitor, or already holds it in an outage, tells them through
    /// the outage mail, and only webhooks get each update. Everything a person
    /// wrote, every update of a declared incident, and what a page that kept
    /// the incident with no outage hears goes to both.
    pub fn email_too(&self) -> bool {
        !(self.origin == "monitor" && self.generated && (self.via_component || self.via_outage))
    }

    pub fn incident_title(&self) -> String {
        incident_title(
            self.locale(),
            self.public_title.as_deref(),
            &self.component_name,
            &self.status_at_start,
        )
    }
}

/// The page's own title for an incident, so a mail about an unnarrated
/// monitor-opened incident is not headed "Status update".
pub(crate) fn incident_title(
    locale: Locale,
    public_title: Option<&str>,
    component_name: &str,
    status_at_start: &str,
) -> String {
    public_title
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Tr::new(locale).auto_incident_title(component_name, status_at_start))
}

/// Updates inside the lookback that a served page shows and has no notice
/// for yet, whether or not the page has a subscriber today, so a notice is
/// written when its update is posted and someone subscribing later never gets
/// the backlog. The
/// incident→page link is `incident.target_id` ∈ the page's curated components,
/// or for an incident with no monitor, the pages it was published to or, once
/// its monitor was deleted, kept on. A page that kept a monitor's incident
/// after the monitor left it hears, once it closed, the closing update and what
/// came after, such as the postmortem: what was posted at or after
/// `resolved_at`, which the close writes in the same transaction as its update.
/// What was said before, an early "resolved" included, stays in its history.
/// Updates flow while the incident is public; once unpublished, only the
/// `resolved` closer still goes out, so an incident taken off the page before it
/// ends doesn't strand subscribers on its last update.
pub async fn pending_update_notices(
    conn: &mut sqlx::PgConnection,
    limit: i64,
) -> Result<Vec<UpdateNoticeRow>> {
    let sql = format!(
        "SELECT {page},
                u.id AS update_id, u.phase, u.message, u.generated,
                i.id AS incident_id, i.origin, i.public_title, i.status_at_start,
                (t.id IS NOT NULL AND t.plan_hold_at IS NULL) AS via_component,
                -- A member that left the page while down is not told by its
                -- outage's resolved mail, so its own closer still is.
                EXISTS (SELECT 1 FROM status_page_outage_members m
                        WHERE m.status_page_id = sp.id AND m.incident_id = i.id
                          AND m.org_id = i.org_id
                          AND NOT (m.closed_at IS NOT NULL AND m.ended_at IS NULL)) AS via_outage,
                COALESCE(NULLIF(c.public_name, ''), t.name,
                         (SELECT isp.component_name FROM incident_status_pages isp
                          WHERE isp.incident_id = i.id AND isp.org_id = i.org_id
                            AND isp.status_page_id = sp.id), '') AS component_name
         FROM incident_updates u
         JOIN incidents i ON i.id = u.incident_id AND i.org_id = u.org_id
         JOIN status_pages sp ON sp.org_id = i.org_id
         {PAGE_PLAN_JOIN}
         LEFT JOIN status_page_components c
              ON c.status_page_id = sp.id AND c.org_id = sp.org_id AND c.target_id = i.target_id
         LEFT JOIN targets t ON t.id = c.target_id AND t.org_id = c.org_id
         WHERE u.posted_at >= now() - make_interval(hours => $2)
           AND {PAGE_SERVED}
           AND (i.visibility = 'public'
                OR (u.phase = 'resolved'
                    AND EXISTS (SELECT 1 FROM incident_events e
                                WHERE e.incident_id = i.id AND e.org_id = i.org_id
                                  AND e.kind = 'published')))
           -- The page drops a held monitor from its component list, so mailing
           -- about one would send readers to a page that shows neither the
           -- component nor the incident.
           AND ((t.id IS NOT NULL AND t.plan_hold_at IS NULL)
                OR ((i.target_id IS NULL OR u.posted_at >= i.resolved_at)
                    AND EXISTS (
                    SELECT 1 FROM incident_status_pages isp
                    WHERE isp.incident_id = i.id AND isp.org_id = i.org_id
                      AND isp.status_page_id = sp.id)))
           AND NOT EXISTS (
               SELECT 1 FROM status_page_notices n
               WHERE n.status_page_id = sp.id AND n.kind = 'incident_update'
                 AND n.source_ref = u.id AND n.phase = '')
         ORDER BY u.posted_at
         LIMIT $1",
        page = page_row_columns(),
    );
    let rows = sqlx::query_as::<_, UpdateNoticeRow>(&sql)
        .bind(limit)
        .bind(FANOUT_LOOKBACK_HOURS as i32)
        .fetch_all(conn)
        .await
        .context("subscribers::pending_update_notices")?;
    Ok(rows)
}

#[derive(Debug, sqlx::FromRow)]
pub struct OperatorSubscriber {
    pub id: Uuid,
    pub target: String,
    pub channel: String,
    pub verified: bool,
    pub created_at: chrono::DateTime<Utc>,
    /// Set when a visitor signed in with the page password made it: that
    /// password's `password_set_at`.
    pub via_password_at: Option<chrono::DateTime<Utc>>,
}

/// Subscribers of one page for the operator roster (newest first), verified and
/// pending. Org- and page-scoped so it can't read another tenant's list.
pub async fn list_for_page(
    pool: &PgPool,
    org_id: Uuid,
    status_page_id: Uuid,
) -> Result<Vec<OperatorSubscriber>> {
    let rows = sqlx::query_as::<_, OperatorSubscriber>(
        "SELECT id, target, channel, (verified_at IS NOT NULL) AS verified, created_at,
                via_password_at
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

    fn pending(public_title: Option<&str>, public_locale: &str) -> UpdateNoticeRow {
        UpdateNoticeRow {
            page: PageRow {
                page_id: Uuid::nil(),
                org_id: Uuid::nil(),
                page_name: "acme".into(),
                public_locale: public_locale.into(),
                slug: "acme".into(),
                custom_domain: None,
                custom_domain_published: false,
            },
            update_id: Uuid::nil(),
            phase: "investigating".into(),
            message: String::new(),
            generated: false,
            incident_id: Uuid::nil(),
            origin: "monitor".into(),
            via_component: true,
            via_outage: true,
            public_title: public_title.map(str::to_owned),
            component_name: "Painel Cloud".into(),
            status_at_start: "down".into(),
        }
    }

    #[test]
    fn only_a_monitor_incidents_own_words_skip_the_inbox() {
        let mut row = pending(None, "en");
        row.generated = true;
        assert!(!row.email_too(), "the outage mail covers it");
        row.generated = false;
        assert!(row.email_too(), "a person wrote it");
        row.origin = "manual".into();
        row.generated = true;
        assert!(
            row.email_too(),
            "a declared incident is told update by update"
        );
        row.origin = "monitor".into();
        row.via_component = false;
        assert!(!row.email_too(), "its outage still tells it");
        row.via_outage = false;
        assert!(
            row.email_too(),
            "a page that kept it has no outage to tell it"
        );
    }

    #[test]
    fn an_unnarrated_incident_is_titled_the_way_the_page_titles_it() {
        assert_eq!(pending(None, "en").incident_title(), "Painel Cloud down");
        assert_eq!(
            pending(Some(""), "en").incident_title(),
            "Painel Cloud down"
        );
        assert_eq!(
            pending(Some("API errors"), "en").incident_title(),
            "API errors"
        );
    }

    #[test]
    fn an_unnarrated_incident_is_titled_in_the_page_language() {
        assert_eq!(
            pending(None, "de").incident_title(),
            "Painel Cloud: ausgefallen"
        );
        assert_eq!(
            pending(Some("API-Fehler"), "de").incident_title(),
            "API-Fehler"
        );
    }
}
