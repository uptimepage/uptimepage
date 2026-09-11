//! Storage for per-org `notification_channels`.
//!
//! Every method is org-scoped (`org: OrgId`), mirroring [`super::TargetStore`].
//! One tenant can never read or mutate another's channels.
//! The transport secrets in `config` are sealed at rest by the credentials
//! KEK — the same `{"$enc":"v1:…"}` envelope convention used for
//! `targets.check_spec` (see [`super::postgres_secrets`]) — and opened back to
//! a plaintext [`ChannelConfig`] at the DB edge so callers never see ciphertext.

use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde_json::Value;
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::api::error::codes;
use crate::domain::{
    ChannelConfig, ChannelKind, NewNotificationChannel, NotificationChannel,
    NotificationChannelUpdate, OrgId, UserId, WriteSource,
};
use crate::error::{AppError, Result};
use crate::security::{Cipher, envelope_str, wrap_envelope};
use crate::storage::accounts;
use crate::storage::locks::{account_lock_key, advisory_xact_lock};

/// Serialize + (optionally) seal a config for storage. With no KEK the JSON is
/// stored as-is (no-KEK self-host); with a KEK the whole blob becomes
/// `{"$enc":"v1:…"}`.
fn seal(cfg: &ChannelConfig, cipher: Option<&Cipher>) -> Result<Value> {
    let plain = serde_json::to_value(cfg)
        .map_err(|e| AppError::Other(anyhow!("encode channel config: {e}")))?;
    match cipher {
        None => Ok(plain),
        Some(c) => {
            let bytes = serde_json::to_vec(&plain)
                .map_err(|e| AppError::Other(anyhow!("encode channel config: {e}")))?;
            let env = c
                .encrypt(&bytes)
                .map_err(|e| AppError::Other(anyhow!("seal channel config: {e}")))?;
            Ok(wrap_envelope(env))
        }
    }
}

/// Inverse of [`seal`]. Both mode mismatches — sealed row in no-KEK mode,
/// plaintext row in KEK mode — are loud errors. The plaintext-in-KEK case
/// closes the asymmetric gap a manual-INSERT path (test helper, ad-hoc SQL,
/// future demo seam) would otherwise drive through silently, since the
/// plaintext schema deserializes cleanly into `ChannelConfig` without it.
fn open(value: Value, cipher: Option<&Cipher>) -> Result<ChannelConfig> {
    match (cipher, envelope_str(&value)) {
        (Some(c), Some(env)) => {
            let bytes = c
                .decrypt(env)
                .map_err(|e| AppError::Other(anyhow!("open channel config: {e}")))?;
            serde_json::from_slice(&bytes)
                .map_err(|e| AppError::Other(anyhow!("decode channel config: {e}")))
        }
        (None, None) => serde_json::from_value(value)
            .map_err(|e| AppError::Other(anyhow!("decode channel config: {e}"))),
        (None, Some(_)) => Err(AppError::Other(anyhow!(
            "notification channel is sealed but no credentials KEK is configured"
        ))),
        (Some(_), None) => Err(AppError::Other(anyhow!(
            "notification channel is plaintext but a credentials KEK is configured — write path mismatch"
        ))),
    }
}

/// How long a reported run stays reported. Long enough that a flapping
/// endpoint cannot mail the owners on every cycle, short enough that a claim
/// stranded by a crash mid-send is owed again the same day.
pub const FAILURE_ALERT_COOLDOWN: Duration = Duration::hours(24);

/// A channel's delivery run after one finished delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChannelHealth {
    /// Exhausted deliveries in a row; `0` once a send lands.
    pub consecutive_failures: i32,
    /// Start of the current run.
    pub failing_since: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait NotificationChannelStore: Send + Sync {
    /// Atomically capped at `max_channels` for `org`. A breach returns
    /// `CHANNEL_QUOTA_EXCEEDED`; a duplicate name (within the org)
    /// `CHANNEL_NAME_TAKEN`.
    async fn create(
        &self,
        org: OrgId,
        new: NewNotificationChannel,
        source: WriteSource,
        max_channels: i64,
        actor: Option<UserId>,
    ) -> Result<NotificationChannel>;
    /// `Ok(None)` when the org is at its channel cap or already holds this
    /// address: a signup must not fail over either.
    async fn seed_owner_email(
        &self,
        org: OrgId,
        address: &str,
        actor: UserId,
        max_channels: i64,
    ) -> Result<Option<NotificationChannel>>;
    async fn list(&self, org: OrgId) -> Result<Vec<NotificationChannel>>;
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<NotificationChannel>>;
    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: NotificationChannelUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<NotificationChannel>>;
    async fn delete(&self, org: OrgId, id: Uuid, actor: Option<UserId>) -> Result<bool>;
    /// Disable every channel of `kind` carrying `external_ref` and clear its
    /// verification stamp; returns how many flipped. Deliberately NOT
    /// org-scoped — the triggering event comes from the transport's provider
    /// (a bot kick, a hard bounce) and means the destination is dead for
    /// every org pointed at it; `kind` partitions the ref namespace, so an
    /// email address can never collide with a chat id.
    async fn disable_by_external_ref(
        &self,
        kind: ChannelKind,
        external_ref: &str,
        reason: &str,
    ) -> Result<u64>;
    /// Channels of `kind` (any org) still carrying `external_ref`.
    async fn count_by_external_ref(&self, kind: ChannelKind, external_ref: &str) -> Result<i64>;
    /// Compare-and-swap on `from`. Verification, failure run and
    /// `write_source` are left alone: the operator changed nothing.
    async fn follow_chat_migration(
        &self,
        org: OrgId,
        id: Uuid,
        from: &str,
        to: &str,
    ) -> Result<bool>;
    /// Cross-org like [`Self::disable_by_external_ref`] and for the same
    /// reason. Returns how many moved.
    async fn follow_linked_chat_migration(&self, from: &str, to: &str) -> Result<u64>;
    /// Fold one finished delivery into the channel's failure run. One that
    /// lands clears it and stamps `last_delivered_at`; one that used up every
    /// retry extends it. The report stamp is left alone either way, so the
    /// cooldown on [`Self::claim_failure_alert`] survives a recovery.
    async fn record_delivery_outcome(
        &self,
        org: OrgId,
        id: Uuid,
        delivered: bool,
    ) -> Result<ChannelHealth>;
    /// Claim the alert owed for the current run, returning the stamp taken.
    /// A run reported less than [`FAILURE_ALERT_COOLDOWN`] ago claims nothing,
    /// so an endpoint that fails, recovers and fails again cannot mail the
    /// owners on every cycle, and a claim stranded by a crash frees itself.
    async fn claim_failure_alert(&self, org: OrgId, id: Uuid) -> Result<Option<DateTime<Utc>>>;
    /// Hand back an unsent claim, identified by the stamp
    /// [`Self::claim_failure_alert`] returned, so a late release cannot clear
    /// a claim some other run has since taken.
    async fn release_failure_alert(
        &self,
        org: OrgId,
        id: Uuid,
        claimed: DateTime<Utc>,
    ) -> Result<()>;
    /// Subset of `ids` that exist in `org`. Mirrors
    /// [`crate::storage::MaintenanceStore::existing_target_ids`] so the
    /// "ids belong to the caller's org" idiom is uniform — used to validate
    /// target alert bindings in one query instead of N point lookups, and to
    /// close the cross-tenant IDOR where a target binds another org's channel.
    async fn existing_channel_ids(&self, org: OrgId, ids: &[Uuid]) -> Result<Vec<Uuid>>;
    /// Stamp `verified_at` on an email channel; `false` when the channel is
    /// gone, not an email kind, or was modified after `expected_updated_at`
    /// (a config swap racing the verify click must not transfer the proof).
    async fn set_verified(
        &self,
        org: OrgId,
        id: Uuid,
        expected_updated_at: DateTime<Utc>,
    ) -> Result<bool>;
    /// Disable one channel by id with no org scope: the signed one-click stop
    /// link is the authority. Clears verification so it cannot silently resume.
    /// Idempotent.
    async fn disable_self_service(&self, channel_id: Uuid, reason: &str) -> Result<bool>;
    /// Ids of the channels whose tag rule covers a monitor carrying `tags`.
    /// Reads no config, so paging never unseals a secret to learn who to
    /// page. Empty `tags` matches nothing.
    async fn auto_bound_ids(&self, org: OrgId, tags: &[String]) -> Result<Vec<Uuid>>;
}

/// Every channel that should hear about `target`: bound first, then rule
/// matches, deduped.
///
/// Who, not what to do when the lookup fails — that is the caller's, because
/// their second chances differ. A page that errors here writes no notification
/// row at all, so the reconcile scan picks the episode up and retries it whole;
/// a silence notice has no such scan behind it and falls back to the bound
/// channels rather than saying nothing.
pub async fn paging_channel_ids(
    store: &dyn NotificationChannelStore,
    org: OrgId,
    target: &crate::domain::Target,
) -> Result<Vec<Uuid>> {
    let mut ids: Vec<Uuid> = target.alerts.iter().map(|b| b.channel_id).collect();
    for id in store.auto_bound_ids(org, &target.tags).await? {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// One-click stop proof: HMAC of the channel id, scoped to that channel so a
/// click can reach no other. Reproduced at send time, nothing persisted.
pub fn channel_stop_token(secret: &str, channel_id: Uuid) -> String {
    crate::auth::mac::hmac_sha256_hex(secret.as_bytes(), &[channel_id.as_bytes()])
}

pub fn verify_channel_stop(secret: &str, channel_id: Uuid, presented: &str) -> bool {
    channel_stop_token(secret, channel_id)
        .as_bytes()
        .ct_eq(presented.as_bytes())
        .into()
}

/// Absolute one-click stop/decline link for a channel; `None` when the base URL
/// or stop secret is unset so no dead link is ever mailed.
pub fn channel_stop_url(base_url: &str, secret: &str, channel_id: Uuid) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() || secret.is_empty() {
        return None;
    }
    let mac = channel_stop_token(secret, channel_id);
    Some(format!("{base}/alert-channel/stop?c={channel_id}&t={mac}"))
}

// ── Postgres impl ────────────────────────────────────────────────────────

/// Org-scoped Postgres store. Every query binds the caller-supplied `org` so
/// one tenant can never read or mutate another's channels.
pub struct PgNotificationChannelStore {
    pool: PgPool,
    cipher: Option<Arc<Cipher>>,
}

impl PgNotificationChannelStore {
    pub fn new(pool: PgPool, cipher: Option<Arc<Cipher>>) -> Self {
        Self { pool, cipher }
    }

    async fn rewrite_chat(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        org: OrgId,
        id: Uuid,
        config: Value,
        from: &str,
        to: &str,
    ) -> Result<bool> {
        let mut cfg = open(config, self.cipher.as_deref())?;
        if !cfg.follow_chat_migration(from, to) {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE notification_channels SET config = $3, external_ref = $4, updated_at = now() \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .bind(seal(&cfg, self.cipher.as_deref())?)
        .bind(cfg.lifecycle_ref())
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("follow chat migration: {e}")))?;
        crate::storage::orgs::record_audit_tx(
            tx,
            org,
            None,
            "channel.chat_migrated",
            serde_json::json!({ "channel_id": id, "kind": cfg.kind().as_db_str(), "from": from, "to": to }),
        )
        .await?;
        Ok(true)
    }
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: Uuid,
    name: String,
    config: Value,
    enabled: bool,
    disabled_reason: Option<String>,
    verified_at: Option<DateTime<Utc>>,
    consecutive_failures: i32,
    failing_since: Option<DateTime<Utc>>,
    last_delivered_at: Option<DateTime<Utc>>,
    auto_bind_tags: Vec<String>,
    write_source: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl ChannelRow {
    fn into_channel(self, cipher: Option<&Cipher>) -> Result<NotificationChannel> {
        let config = open(self.config, cipher)?;
        Ok(NotificationChannel {
            id: self.id,
            name: self.name,
            // The decrypted config is the single source of truth for kind;
            // the `kind` text column exists only for indexing/debugging.
            kind: config.kind(),
            config,
            enabled: self.enabled,
            disabled_reason: self.disabled_reason,
            verified_at: self.verified_at,
            consecutive_failures: self.consecutive_failures,
            failing_since: self.failing_since,
            last_delivered_at: self.last_delivered_at,
            auto_bind_tags: self.auto_bind_tags,
            write_source: WriteSource::from_db(&self.write_source),
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.is_unique_violation())
}

#[async_trait]
impl NotificationChannelStore for PgNotificationChannelStore {
    async fn create(
        &self,
        org: OrgId,
        new: NewNotificationChannel,
        source: WriteSource,
        max_channels: i64,
        actor: Option<UserId>,
    ) -> Result<NotificationChannel> {
        let sealed = seal(&new.config, self.cipher.as_deref())?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow!("begin: {e}")))?;
        // The count-subquery + INSERT is not race-safe on its own under READ
        // COMMITTED — two creates at count == cap-1 each see `< cap` and both
        // insert, overshooting the cap. A per-account advisory lock (the same
        // key every other pooled-cap writer uses) serialises them; it is held
        // until this transaction commits or rolls back.
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(|e| AppError::Other(anyhow!("advisory lock: {e}")))?;
        let pool_orgs = accounts::live_orgs("$10");
        let row: Option<ChannelRow> = sqlx::query_as(
            &format!(r#"INSERT INTO notification_channels (org_id, name, kind, config, external_ref, enabled, write_source, auto_bind_tags)
               SELECT $1, $2, $3, $4, $5, $6, $8, $9
               WHERE (SELECT count(*) FROM notification_channels WHERE org_id IN ({pool_orgs})) < $7
               RETURNING id, name, config, enabled, disabled_reason, verified_at, consecutive_failures, failing_since, last_delivered_at, auto_bind_tags, write_source, created_at, updated_at"#),
        )
        .bind(org.0)
        .bind(&new.name)
        .bind(new.config.kind().as_db_str())
        .bind(&sealed)
        .bind(new.config.lifecycle_ref())
        .bind(new.enabled)
        .bind(max_channels)
        .bind(source.as_str())
        .bind(&new.auto_bind_tags)
        .bind(account.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::unprocessable(
                    codes::CHANNEL_NAME_TAKEN,
                    "a notification channel with this name already exists",
                )
            } else {
                AppError::Other(anyhow!("insert notification channel: {e}"))
            }
        })?;
        let Some(row) = row else {
            tx.rollback().await.ok();
            return Err(AppError::unprocessable(
                codes::CHANNEL_QUOTA_EXCEEDED,
                "notification channel limit reached for this plan",
            ));
        };
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            actor,
            "channel.created",
            serde_json::json!({ "channel_id": row.id, "kind": new.config.kind().as_db_str() }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow!("commit: {e}")))?;
        row.into_channel(self.cipher.as_deref())
    }

    async fn seed_owner_email(
        &self,
        org: OrgId,
        address: &str,
        actor: UserId,
        max_channels: i64,
    ) -> Result<Option<NotificationChannel>> {
        // EmailConfig::validate rejects uppercase, and bounces are reported
        // against the lowercase form the send used.
        let address = address.trim().to_ascii_lowercase();
        let config = ChannelConfig::Email(crate::domain::EmailConfig {
            to: address.clone(),
        });
        let sealed = seal(&config, self.cipher.as_deref())?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow!("begin: {e}")))?;
        // Same lock the other cap-checked writers take, so a signup racing a
        // manual create cannot overshoot the plan cap.
        let account = accounts::account_for_org(&mut *tx, org).await?;
        advisory_xact_lock(&mut *tx, &account_lock_key(account))
            .await
            .map_err(|e| AppError::Other(anyhow!("advisory lock: {e}")))?;
        let pool_orgs = accounts::live_orgs("$5");
        let row: Option<ChannelRow> = sqlx::query_as(
            &format!(r#"INSERT INTO notification_channels /* SAFE: seeded pre-verified — the signup that reached here already proved control of this inbox (OAuth verified_email, or a claimed magic link), which is the same proof the verification mail collects */
               (org_id, name, kind, config, external_ref, enabled, verified_at, write_source)
               SELECT $1, $2, 'email', $3, $2, true, now(), 'ui'
               WHERE (SELECT count(*) FROM notification_channels WHERE org_id IN ({pool_orgs})) < $4
               ON CONFLICT (org_id, name) DO NOTHING
               RETURNING id, name, config, enabled, disabled_reason, verified_at, consecutive_failures, failing_since, last_delivered_at, auto_bind_tags, write_source, created_at, updated_at"#),
        )
        .bind(org.0)
        .bind(&address)
        .bind(&sealed)
        .bind(max_channels)
        .bind(account.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("seed owner email channel: {e}")))?;
        let Some(row) = row else {
            tx.rollback().await.ok();
            return Ok(None);
        };
        crate::storage::orgs::record_audit_tx(
            &mut tx,
            org,
            Some(actor),
            "channel.created",
            serde_json::json!({ "channel_id": row.id, "kind": "email", "seeded": true }),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow!("commit: {e}")))?;
        row.into_channel(self.cipher.as_deref()).map(Some)
    }

    async fn list(&self, org: OrgId) -> Result<Vec<NotificationChannel>> {
        let rows: Vec<ChannelRow> = sqlx::query_as(
            r#"SELECT id, name, config, enabled, disabled_reason, verified_at, consecutive_failures, failing_since, last_delivered_at, auto_bind_tags, write_source, created_at, updated_at
               FROM notification_channels
               WHERE org_id = $1
               ORDER BY name"#,
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("list notification channels: {e}")))?;
        rows.into_iter()
            .map(|r| r.into_channel(self.cipher.as_deref()))
            .collect()
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<NotificationChannel>> {
        let row: Option<ChannelRow> = sqlx::query_as(
            r#"SELECT id, name, config, enabled, disabled_reason, verified_at, consecutive_failures, failing_since, last_delivered_at, auto_bind_tags, write_source, created_at, updated_at
               FROM notification_channels WHERE id = $1 AND org_id = $2"#,
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("get notification channel: {e}")))?;
        row.map(|r| r.into_channel(self.cipher.as_deref()))
            .transpose()
    }

    async fn auto_bound_ids(&self, org: OrgId, tags: &[String]) -> Result<Vec<Uuid>> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        // Matching folds case, and one Rust comparison owns that fold, so the
        // database's collation can never page a different set than the console
        // shows. Bounded by the org, which is a handful of rows.
        let rows: Vec<(Uuid, Vec<String>)> = sqlx::query_as(
            r#"SELECT id, auto_bind_tags FROM notification_channels
               WHERE org_id = $1 AND auto_bind_tags <> '{}'
               ORDER BY created_at"#,
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("auto-bound channels: {e}")))?;
        let folded: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
        Ok(rows
            .into_iter()
            .filter(|(_, rule)| crate::domain::matches_folded(rule, &folded))
            .map(|(id, _)| id)
            .collect())
    }

    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: NotificationChannelUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<Option<NotificationChannel>> {
        // A config identical to the stored one is treated as omitted, so the
        // verification stamp survives a no-op replace. Best-effort: a stored
        // config that won't unseal (cipher mismatch) is replaced, not blocking.
        let config = match update.config {
            Some(c) => match self.get(org, id).await {
                Ok(Some(current)) if current.config == c => None,
                _ => Some(c),
            },
            None => None,
        };
        // Re-seal only when the config is being changed; `kind` and the
        // lifecycle ref follow it.
        let (sealed, kind) = match &config {
            Some(c) => (
                Some(seal(c, self.cipher.as_deref())?),
                Some(c.kind().as_db_str()),
            ),
            None => (None, None),
        };
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow!("begin: {e}")))?;
        let row: Option<ChannelRow> = sqlx::query_as(
            r#"UPDATE notification_channels
               SET name        = COALESCE($2, name),
                   kind        = COALESCE($3, kind),
                   config      = COALESCE($4, config),
                   enabled     = COALESCE($5, enabled),
                   -- Re-enabling clears the platform's disable note.
                   disabled_reason = CASE WHEN $5 THEN NULL ELSE disabled_reason END,
                   -- Only the disabled -> enabled transition clears the run;
                   -- every ordinary save carries enabled = true.
                   consecutive_failures = CASE WHEN $5 AND NOT enabled THEN 0 ELSE consecutive_failures END,
                   failing_since = CASE WHEN $5 AND NOT enabled THEN NULL ELSE failing_since END,
                   failing_notified_at = CASE WHEN $5 AND NOT enabled THEN NULL ELSE failing_notified_at END,
                   -- A replaced config must re-verify its address.
                   verified_at = CASE WHEN $4::jsonb IS NOT NULL THEN NULL ELSE verified_at END,
                   external_ref = CASE WHEN $4::jsonb IS NOT NULL THEN $8 ELSE external_ref END,
                   auto_bind_tags = COALESCE($9, auto_bind_tags),
                   write_source = $7,
                   updated_at  = now()
               WHERE id = $1 AND org_id = $6
               RETURNING id, name, config, enabled, disabled_reason, verified_at, consecutive_failures, failing_since, last_delivered_at, auto_bind_tags, write_source, created_at, updated_at"#,
        )
        .bind(id)
        .bind(update.name.as_ref())
        .bind(kind)
        .bind(sealed)
        .bind(update.enabled)
        .bind(org.0)
        .bind(source.as_str())
        .bind(config.as_ref().and_then(|c| c.lifecycle_ref()))
        .bind(update.auto_bind_tags.as_ref())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::unprocessable(
                    codes::CHANNEL_NAME_TAKEN,
                    "a notification channel with this name already exists",
                )
            } else {
                AppError::Other(anyhow!("update notification channel: {e}"))
            }
        })?;
        let channel = row
            .map(|r| r.into_channel(self.cipher.as_deref()))
            .transpose()?;
        if let Some(ch) = &channel {
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                "channel.updated",
                serde_json::json!({ "channel_id": ch.id, "kind": ch.kind.as_db_str() }),
            )
            .await?;
        }
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow!("commit: {e}")))?;
        Ok(channel)
    }

    async fn delete(&self, org: OrgId, id: Uuid, actor: Option<UserId>) -> Result<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow!("begin: {e}")))?;
        let removed: Option<(Uuid, String)> = sqlx::query_as(
            r#"DELETE FROM notification_channels WHERE id = $1 AND org_id = $2 RETURNING id, kind"#,
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("delete notification channel: {e}")))?;
        if let Some((channel_id, kind)) = &removed {
            crate::storage::orgs::record_audit_tx(
                &mut tx,
                org,
                actor,
                "channel.deleted",
                serde_json::json!({ "channel_id": channel_id, "kind": kind }),
            )
            .await?;
        }
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow!("commit: {e}")))?;
        Ok(removed.is_some())
    }

    async fn disable_by_external_ref(
        &self,
        kind: ChannelKind,
        external_ref: &str,
        reason: &str,
    ) -> Result<u64> {
        let result = sqlx::query(
            r#"UPDATE notification_channels /* SAFE: provider lifecycle — the event (bot kick, address bounce) comes from the transport's provider and means the destination is dead for every org pointed at it; kind partitions the ref namespace */
               SET enabled = false, disabled_reason = $3, verified_at = NULL, updated_at = now()
               WHERE kind = $1 AND external_ref = $2 AND enabled"#,
        )
        .bind(kind.as_db_str())
        .bind(external_ref)
        .bind(reason)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("disable by external ref: {e}")))?;
        Ok(result.rows_affected())
    }

    async fn count_by_external_ref(&self, kind: ChannelKind, external_ref: &str) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as(
            r#"SELECT count(*) FROM notification_channels /* SAFE: operator lifecycle — counts any org's links to a chat before the bot leaves it */
               WHERE kind = $1 AND external_ref = $2"#,
        )
        .bind(kind.as_db_str())
        .bind(external_ref)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("count by external ref: {e}")))?;
        Ok(n)
    }

    async fn follow_chat_migration(
        &self,
        org: OrgId,
        id: Uuid,
        from: &str,
        to: &str,
    ) -> Result<bool> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow!("begin: {e}")))?;
        let row: Option<(Value,)> = sqlx::query_as(
            "SELECT config FROM notification_channels WHERE id = $1 AND org_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("lock notification channel: {e}")))?;
        let Some((config,)) = row else {
            return Ok(false);
        };
        let moved = self
            .rewrite_chat(&mut tx, org, id, config, from, to)
            .await?;
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow!("commit: {e}")))?;
        Ok(moved)
    }

    async fn follow_linked_chat_migration(&self, from: &str, to: &str) -> Result<u64> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Other(anyhow!("begin: {e}")))?;
        let rows: Vec<(Uuid, Uuid, Value)> = sqlx::query_as(
            r#"SELECT id, org_id, config FROM notification_channels /* SAFE: provider lifecycle — the chat moved for every org linked to it; kind partitions the ref namespace */
               WHERE kind = $1 AND external_ref = $2 FOR UPDATE"#,
        )
        .bind(ChannelKind::TelegramApp.as_db_str())
        .bind(from)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| AppError::Other(anyhow!("lock linked telegram channels: {e}")))?;
        let mut moved = 0;
        for (id, org_id, config) in rows {
            // One row that will not open must not hold the others on the
            // dead id.
            match self
                .rewrite_chat(&mut tx, OrgId(org_id), id, config, from, to)
                .await
            {
                Ok(true) => moved += 1,
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    channel_id = %id,
                    error = %err,
                    "linked telegram channel did not follow the chat migration"
                ),
            }
        }
        tx.commit()
            .await
            .map_err(|e| AppError::Other(anyhow!("commit: {e}")))?;
        Ok(moved)
    }

    async fn record_delivery_outcome(
        &self,
        org: OrgId,
        id: Uuid,
        delivered: bool,
    ) -> Result<ChannelHealth> {
        // Bookkeeping is not an edit: `set_verified` reads `updated_at` to spot
        // a config swap racing a verify click, so none of these touch it.
        let sql = if delivered {
            // Nothing to write while the channel is healthy and its stamp is
            // fresh; the console reads that stamp at hour granularity.
            r#"UPDATE notification_channels
               SET consecutive_failures = 0,
                   failing_since = NULL,
                   last_delivered_at = now()
               WHERE id = $1 AND org_id = $2
                 AND (consecutive_failures <> 0
                      OR last_delivered_at IS NULL
                      OR last_delivered_at < now() - interval '1 hour')
               RETURNING consecutive_failures, failing_since"#
        } else {
            r#"UPDATE notification_channels
               SET consecutive_failures = consecutive_failures + 1,
                   failing_since = COALESCE(failing_since, now())
               WHERE id = $1 AND org_id = $2
               RETURNING consecutive_failures, failing_since"#
        };
        let row: Option<(i32, Option<DateTime<Utc>>)> = sqlx::query_as(sql)
            .bind(id)
            .bind(org.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::Other(anyhow!("record delivery outcome: {e}")))?;
        Ok(
            row.map_or(ChannelHealth::default(), |(n, since)| ChannelHealth {
                consecutive_failures: n,
                failing_since: since,
            }),
        )
    }

    async fn claim_failure_alert(&self, org: OrgId, id: Uuid) -> Result<Option<DateTime<Utc>>> {
        // The stamp comparison is the claim: of two racing sweeps, one wins.
        let claimed: Option<(DateTime<Utc>,)> = sqlx::query_as(
            r#"UPDATE notification_channels
               SET failing_notified_at = now()
               WHERE id = $1 AND org_id = $2
                 AND failing_since IS NOT NULL
                 AND (failing_notified_at IS NULL OR failing_notified_at < $3)
               RETURNING failing_notified_at"#,
        )
        .bind(id)
        .bind(org.0)
        .bind(Utc::now() - FAILURE_ALERT_COOLDOWN)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("claim failure alert: {e}")))?;
        Ok(claimed.map(|(at,)| at))
    }

    async fn release_failure_alert(
        &self,
        org: OrgId,
        id: Uuid,
        claimed: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            r#"UPDATE notification_channels
               SET failing_notified_at = NULL
               WHERE id = $1 AND org_id = $2 AND failing_notified_at = $3"#,
        )
        .bind(id)
        .bind(org.0)
        .bind(claimed)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("release failure alert: {e}")))?;
        Ok(())
    }

    async fn existing_channel_ids(&self, org: OrgId, ids: &[Uuid]) -> Result<Vec<Uuid>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"SELECT id FROM notification_channels
               WHERE id = ANY($1::uuid[]) AND org_id = $2"#,
        )
        .bind(ids)
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("existing_channel_ids: {e}")))?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    async fn set_verified(
        &self,
        org: OrgId,
        id: Uuid,
        expected_updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let result = sqlx::query(
            r#"UPDATE notification_channels
               SET verified_at = now(), updated_at = now()
               WHERE id = $1 AND org_id = $2 AND kind = 'email'
                 AND updated_at = $3"#,
        )
        .bind(id)
        .bind(org.0)
        .bind(expected_updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("set verified: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn disable_self_service(&self, channel_id: Uuid, reason: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"UPDATE notification_channels /* SAFE: recipient self-service stop — the signed one-click link proves control of the destination inbox; scoped to this single channel by id */
               SET enabled = false, verified_at = NULL, disabled_reason = $2, updated_at = now()
               WHERE id = $1 AND enabled"#,
        )
        .bind(channel_id)
        .bind(reason)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Other(anyhow!("disable self-service: {e}")))?;
        Ok(result.rows_affected() > 0)
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────

/// Org-aware in-memory store: entries are tagged with their `OrgId` and every
/// method filters on it, so cross-tenant isolation can be asserted without a
/// Postgres backend.
struct MemEntry {
    org: OrgId,
    external_ref: Option<String>,
    notified_at: Option<DateTime<Utc>>,
    ch: NotificationChannel,
}

#[derive(Default)]
pub struct InMemoryNotificationChannelStore {
    inner: Mutex<Vec<MemEntry>>,
}

impl InMemoryNotificationChannelStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total entries across every org (test introspection only).
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    fn move_chat(e: &mut MemEntry, from: &str, to: &str) -> bool {
        if !e.ch.config.follow_chat_migration(from, to) {
            return false;
        }
        e.external_ref = e.ch.config.lifecycle_ref().map(str::to_owned);
        e.ch.updated_at = Utc::now();
        true
    }
}

#[async_trait]
impl NotificationChannelStore for InMemoryNotificationChannelStore {
    async fn create(
        &self,
        org: OrgId,
        new: NewNotificationChannel,
        source: WriteSource,
        max_channels: i64,
        _actor: Option<UserId>,
    ) -> Result<NotificationChannel> {
        let mut g = self.inner.lock();
        if g.iter().any(|e| e.org == org && e.ch.name == new.name) {
            return Err(AppError::unprocessable(
                codes::CHANNEL_NAME_TAKEN,
                "a notification channel with this name already exists",
            ));
        }
        if g.iter().filter(|e| e.org == org).count() as i64 >= max_channels {
            return Err(AppError::unprocessable(
                codes::CHANNEL_QUOTA_EXCEEDED,
                "notification channel limit reached for this plan",
            ));
        }
        let now = Utc::now();
        let ch = NotificationChannel {
            id: Uuid::now_v7(),
            name: new.name,
            kind: new.config.kind(),
            config: new.config,
            enabled: new.enabled,
            disabled_reason: None,
            verified_at: None,
            consecutive_failures: 0,
            failing_since: None,
            last_delivered_at: None,
            auto_bind_tags: new.auto_bind_tags,
            write_source: source,
            created_at: now,
            updated_at: now,
        };
        g.push(MemEntry {
            org,
            external_ref: ch.config.lifecycle_ref().map(str::to_owned),
            notified_at: None,
            ch: ch.clone(),
        });
        Ok(ch)
    }

    async fn seed_owner_email(
        &self,
        org: OrgId,
        address: &str,
        _actor: UserId,
        max_channels: i64,
    ) -> Result<Option<NotificationChannel>> {
        let address = address.trim().to_ascii_lowercase();
        let mut g = self.inner.lock();
        if g.iter().any(|e| e.org == org && e.ch.name == address)
            || g.iter().filter(|e| e.org == org).count() as i64 >= max_channels
        {
            return Ok(None);
        }
        let now = Utc::now();
        let ch = NotificationChannel {
            id: Uuid::now_v7(),
            name: address.clone(),
            kind: ChannelKind::Email,
            config: ChannelConfig::Email(crate::domain::EmailConfig { to: address }),
            enabled: true,
            disabled_reason: None,
            verified_at: Some(now),
            consecutive_failures: 0,
            failing_since: None,
            last_delivered_at: None,
            auto_bind_tags: Vec::new(),
            write_source: WriteSource::Ui,
            created_at: now,
            updated_at: now,
        };
        g.push(MemEntry {
            org,
            external_ref: ch.config.lifecycle_ref().map(str::to_owned),
            notified_at: None,
            ch: ch.clone(),
        });
        Ok(Some(ch))
    }

    async fn list(&self, org: OrgId) -> Result<Vec<NotificationChannel>> {
        let mut v: Vec<NotificationChannel> = self
            .inner
            .lock()
            .iter()
            .filter(|e| e.org == org)
            .map(|e| e.ch.clone())
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<NotificationChannel>> {
        Ok(self
            .inner
            .lock()
            .iter()
            .find(|e| e.org == org && e.ch.id == id)
            .map(|e| e.ch.clone()))
    }

    async fn auto_bound_ids(&self, org: OrgId, tags: &[String]) -> Result<Vec<Uuid>> {
        Ok(self
            .inner
            .lock()
            .iter()
            .filter(|e| e.org == org && e.ch.auto_binds(tags))
            .map(|e| e.ch.id)
            .collect())
    }

    async fn update(
        &self,
        org: OrgId,
        id: Uuid,
        update: NotificationChannelUpdate,
        source: WriteSource,
        _actor: Option<UserId>,
    ) -> Result<Option<NotificationChannel>> {
        let mut g = self.inner.lock();
        if let Some(name) = &update.name
            && g.iter()
                .any(|e| e.org == org && e.ch.id != id && &e.ch.name == name)
        {
            return Err(AppError::unprocessable(
                codes::CHANNEL_NAME_TAKEN,
                "a notification channel with this name already exists",
            ));
        }
        let Some(entry) = g.iter_mut().find(|e| e.org == org && e.ch.id == id) else {
            return Ok(None);
        };
        if let Some(name) = update.name {
            entry.ch.name = name;
        }
        if let Some(cfg) = update.config
            && cfg != entry.ch.config
        {
            entry.external_ref = cfg.lifecycle_ref().map(str::to_owned);
            entry.ch.kind = cfg.kind();
            entry.ch.config = cfg;
            entry.ch.verified_at = None;
        }
        if let Some(tags) = update.auto_bind_tags {
            entry.ch.auto_bind_tags = tags;
        }
        if let Some(enabled) = update.enabled {
            let was_disabled = !entry.ch.enabled;
            entry.ch.enabled = enabled;
            if enabled {
                entry.ch.disabled_reason = None;
            }
            if enabled && was_disabled {
                entry.ch.consecutive_failures = 0;
                entry.ch.failing_since = None;
                entry.notified_at = None;
            }
        }
        let ch = &mut entry.ch;
        ch.write_source = source;
        ch.updated_at = Utc::now();
        Ok(Some(ch.clone()))
    }

    async fn delete(&self, org: OrgId, id: Uuid, _actor: Option<UserId>) -> Result<bool> {
        let mut g = self.inner.lock();
        let before = g.len();
        g.retain(|e| !(e.org == org && e.ch.id == id));
        Ok(g.len() < before)
    }

    async fn disable_by_external_ref(
        &self,
        kind: ChannelKind,
        external_ref: &str,
        reason: &str,
    ) -> Result<u64> {
        let mut flipped = 0;
        for e in self.inner.lock().iter_mut() {
            if e.ch.kind == kind && e.external_ref.as_deref() == Some(external_ref) && e.ch.enabled
            {
                e.ch.enabled = false;
                e.ch.disabled_reason = Some(reason.to_string());
                e.ch.verified_at = None;
                e.ch.updated_at = Utc::now();
                flipped += 1;
            }
        }
        Ok(flipped)
    }

    async fn count_by_external_ref(&self, kind: ChannelKind, external_ref: &str) -> Result<i64> {
        Ok(self
            .inner
            .lock()
            .iter()
            .filter(|e| e.ch.kind == kind && e.external_ref.as_deref() == Some(external_ref))
            .count() as i64)
    }

    async fn follow_chat_migration(
        &self,
        org: OrgId,
        id: Uuid,
        from: &str,
        to: &str,
    ) -> Result<bool> {
        let mut g = self.inner.lock();
        let Some(e) = g.iter_mut().find(|e| e.org == org && e.ch.id == id) else {
            return Ok(false);
        };
        Ok(Self::move_chat(e, from, to))
    }

    async fn follow_linked_chat_migration(&self, from: &str, to: &str) -> Result<u64> {
        let mut moved = 0;
        for e in self.inner.lock().iter_mut() {
            if e.ch.kind == ChannelKind::TelegramApp
                && e.external_ref.as_deref() == Some(from)
                && Self::move_chat(e, from, to)
            {
                moved += 1;
            }
        }
        Ok(moved)
    }

    async fn record_delivery_outcome(
        &self,
        org: OrgId,
        id: Uuid,
        delivered: bool,
    ) -> Result<ChannelHealth> {
        let mut g = self.inner.lock();
        let Some(entry) = g.iter_mut().find(|e| e.org == org && e.ch.id == id) else {
            return Ok(ChannelHealth::default());
        };
        if delivered {
            entry.ch.consecutive_failures = 0;
            entry.ch.failing_since = None;
            entry.ch.last_delivered_at = Some(Utc::now());
        } else {
            entry.ch.consecutive_failures += 1;
            entry.ch.failing_since.get_or_insert_with(Utc::now);
        }
        Ok(ChannelHealth {
            consecutive_failures: entry.ch.consecutive_failures,
            failing_since: entry.ch.failing_since,
        })
    }

    async fn claim_failure_alert(&self, org: OrgId, id: Uuid) -> Result<Option<DateTime<Utc>>> {
        let mut g = self.inner.lock();
        let Some(entry) = g.iter_mut().find(|e| e.org == org && e.ch.id == id) else {
            return Ok(None);
        };
        let now = Utc::now();
        if entry.ch.failing_since.is_none()
            || entry
                .notified_at
                .is_some_and(|at| at >= now - FAILURE_ALERT_COOLDOWN)
        {
            return Ok(None);
        }
        entry.notified_at = Some(now);
        Ok(Some(now))
    }

    async fn release_failure_alert(
        &self,
        org: OrgId,
        id: Uuid,
        claimed: DateTime<Utc>,
    ) -> Result<()> {
        let mut g = self.inner.lock();
        if let Some(entry) = g
            .iter_mut()
            .find(|e| e.org == org && e.ch.id == id && e.notified_at == Some(claimed))
        {
            entry.notified_at = None;
        }
        Ok(())
    }

    async fn existing_channel_ids(&self, org: OrgId, ids: &[Uuid]) -> Result<Vec<Uuid>> {
        let g = self.inner.lock();
        Ok(ids
            .iter()
            .copied()
            .filter(|id| g.iter().any(|e| e.org == org && e.ch.id == *id))
            .collect())
    }

    async fn set_verified(
        &self,
        org: OrgId,
        id: Uuid,
        expected_updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let mut g = self.inner.lock();
        let Some(e) = g.iter_mut().find(|e| {
            e.org == org
                && e.ch.id == id
                && e.ch.kind == ChannelKind::Email
                && e.ch.updated_at == expected_updated_at
        }) else {
            return Ok(false);
        };
        e.ch.verified_at = Some(Utc::now());
        e.ch.updated_at = Utc::now();
        Ok(true)
    }

    async fn disable_self_service(&self, channel_id: Uuid, reason: &str) -> Result<bool> {
        let mut g = self.inner.lock();
        let Some(e) = g.iter_mut().find(|e| e.ch.id == channel_id && e.ch.enabled) else {
            return Ok(false);
        };
        e.ch.enabled = false;
        e.ch.verified_at = None;
        e.ch.disabled_reason = Some(reason.to_string());
        e.ch.updated_at = Utc::now();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SlackConfig, WebhookConfig};
    use std::collections::BTreeMap;

    fn org() -> OrgId {
        OrgId(Uuid::from_u128(0xA1))
    }

    fn other_org() -> OrgId {
        OrgId(Uuid::from_u128(0xB2))
    }

    fn slack(name: &str) -> NewNotificationChannel {
        NewNotificationChannel {
            name: name.into(),
            config: ChannelConfig::Slack(SlackConfig {
                webhook_url: "https://hooks.slack.com/x".into(),
                mention: None,
            }),
            enabled: true,
            auto_bind_tags: Vec::new(),
        }
    }

    fn tagged_target(tags: Vec<String>) -> crate::domain::Target {
        crate::domain::Target {
            id: Uuid::from_u128(7),
            name: "api".into(),
            check: crate::domain::CheckSpec::Tcp(crate::domain::check::TcpCheck {
                host: "db.example.test".into(),
                port: 5432,
                timeout: std::time::Duration::from_secs(3),
            }),
            interval: std::time::Duration::from_secs(60),
            enabled: true,
            tags,
            alerts: crate::domain::TargetAlerts::default(),
            alert_confirmations: 1,
            notify_recovery: true,
            renotify_interval_secs: 0,
            region_policy: Default::default(),
            group_name: None,
            owner_user_id: None,
            write_source: WriteSource::Ui,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            plan_hold_at: None,
        }
    }

    #[tokio::test]
    async fn a_tag_rule_covers_a_monitor_nothing_is_bound_to() {
        use crate::domain::{AlertBinding, TargetAlerts};
        let store = InMemoryNotificationChannelStore::new();
        let mut rule = slack("db team");
        rule.auto_bind_tags = vec!["db".into()];
        let by_rule = store
            .create(org(), rule, WriteSource::Ui, 10, None)
            .await
            .unwrap();
        let bound = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        // Another org's identical rule must never leak in.
        let mut theirs = slack("their db team");
        theirs.auto_bind_tags = vec!["db".into()];
        store
            .create(other_org(), theirs, WriteSource::Ui, 10, None)
            .await
            .unwrap();

        let mut target = tagged_target(vec!["db".into(), "prod".into()]);
        target.alerts = TargetAlerts(vec![AlertBinding {
            channel_id: bound.id,
        }]);

        let ids = paging_channel_ids(&store, org(), &target).await.unwrap();
        assert_eq!(ids, vec![bound.id, by_rule.id]);

        // Retagging moves coverage with no write to the channel.
        target.tags = vec!["web".into()];
        assert_eq!(
            paging_channel_ids(&store, org(), &target).await.unwrap(),
            vec![bound.id]
        );
    }

    #[tokio::test]
    async fn a_rule_is_replaced_whole_and_an_empty_list_clears_it() {
        let store = InMemoryNotificationChannelStore::new();
        let mut rule = slack("db team");
        rule.auto_bind_tags = vec!["db".into()];
        let ch = store
            .create(org(), rule, WriteSource::Ui, 10, None)
            .await
            .unwrap();
        let patch = |tags: Option<Vec<String>>| NotificationChannelUpdate {
            auto_bind_tags: tags,
            ..Default::default()
        };
        // Omitted: the rule stands.
        let kept = store
            .update(org(), ch.id, patch(None), WriteSource::Ui, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.auto_bind_tags, vec!["db".to_string()]);
        let cleared = store
            .update(org(), ch.id, patch(Some(Vec::new())), WriteSource::Ui, None)
            .await
            .unwrap()
            .unwrap();
        assert!(cleared.auto_bind_tags.is_empty());
        assert!(
            store
                .auto_bound_ids(org(), &["db".to_string()])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn create_list_get_update_delete_roundtrip() {
        let store = InMemoryNotificationChannelStore::new();
        let ch = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        assert_eq!(store.list(org()).await.unwrap().len(), 1);
        assert_eq!(store.get(org(), ch.id).await.unwrap().unwrap().name, "ops");

        let patched = store
            .update(
                org(),
                ch.id,
                NotificationChannelUpdate {
                    enabled: Some(false),
                    ..Default::default()
                },
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(!patched.enabled);

        assert!(store.delete(org(), ch.id, None).await.unwrap());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn create_enforces_cap_and_unique_name() {
        let store = InMemoryNotificationChannelStore::new();
        store
            .create(org(), slack("a"), WriteSource::Ui, 1, None)
            .await
            .unwrap();
        let over = store
            .create(org(), slack("b"), WriteSource::Ui, 1, None)
            .await
            .unwrap_err();
        assert!(matches!(over, AppError::Unprocessable { .. }));
        let dup = store
            .create(org(), slack("a"), WriteSource::Ui, 10, None)
            .await
            .unwrap_err();
        assert!(matches!(dup, AppError::Unprocessable { .. }));
    }

    #[tokio::test]
    async fn channels_are_isolated_per_org() {
        let store = InMemoryNotificationChannelStore::new();
        let a = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        // Same name in a different org is allowed and invisible to org A.
        store
            .create(other_org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();

        assert_eq!(store.list(org()).await.unwrap().len(), 1);
        assert_eq!(store.list(other_org()).await.unwrap().len(), 1);
        // org B cannot read, mutate, or delete org A's channel by id.
        assert!(store.get(other_org(), a.id).await.unwrap().is_none());
        assert!(
            store
                .update(
                    other_org(),
                    a.id,
                    NotificationChannelUpdate::default(),
                    WriteSource::Ui,
                    None,
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(!store.delete(other_org(), a.id, None).await.unwrap());
        assert!(
            store
                .existing_channel_ids(other_org(), &[a.id])
                .await
                .unwrap()
                .is_empty()
        );
        // Still intact for the owning org.
        assert!(store.get(org(), a.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn lifecycle_disable_by_external_ref_spans_orgs_and_reenable_clears_note() {
        let store = InMemoryNotificationChannelStore::new();
        let linked = |name: &str, chat: &str| NewNotificationChannel {
            name: name.into(),
            config: ChannelConfig::TelegramApp(crate::domain::TelegramAppConfig {
                chat_id: chat.into(),
                chat_title: None,
            }),
            enabled: true,
            auto_bind_tags: Vec::new(),
        };
        // Two orgs linked the same chat; a third channel points elsewhere.
        let a = store
            .create(org(), linked("prod", "-100"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        store
            .create(
                other_org(),
                linked("ops", "-100"),
                WriteSource::Ui,
                10,
                None,
            )
            .await
            .unwrap();
        store
            .create(
                org(),
                linked("other-chat", "-200"),
                WriteSource::Ui,
                10,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .count_by_external_ref(ChannelKind::TelegramApp, "-100")
                .await
                .unwrap(),
            2
        );
        let flipped = store
            .disable_by_external_ref(ChannelKind::TelegramApp, "-100", "unlinked")
            .await
            .unwrap();
        assert_eq!(flipped, 2, "both orgs' channels for the kicked chat");
        // Idempotent: already-disabled rows don't flip again.
        assert_eq!(
            store
                .disable_by_external_ref(ChannelKind::TelegramApp, "-100", "unlinked")
                .await
                .unwrap(),
            0
        );
        let got = store.get(org(), a.id).await.unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(got.disabled_reason.as_deref(), Some("unlinked"));
        // The unrelated chat's channel is untouched.
        assert_eq!(
            store
                .list(org())
                .await
                .unwrap()
                .iter()
                .filter(|c| c.enabled)
                .count(),
            1
        );

        // Re-enabling clears the platform note; disabling again does not
        // resurrect it.
        let re = store
            .update(
                org(),
                a.id,
                NotificationChannelUpdate {
                    enabled: Some(true),
                    ..Default::default()
                },
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(re.enabled);
        assert_eq!(re.disabled_reason, None);
    }

    #[tokio::test]
    async fn a_moved_chat_is_followed_by_every_org_linked_to_it_and_by_id_once() {
        let store = InMemoryNotificationChannelStore::new();
        let linked = |name: &str, chat: &str| NewNotificationChannel {
            name: name.into(),
            config: ChannelConfig::TelegramApp(crate::domain::TelegramAppConfig {
                chat_id: chat.into(),
                chat_title: None,
            }),
            enabled: true,
            auto_bind_tags: Vec::new(),
        };
        let byo = NewNotificationChannel {
            name: "own-bot".into(),
            config: ChannelConfig::Telegram(crate::domain::TelegramConfig {
                bot_token: "123:abc".into(),
                chat_id: "-100".into(),
            }),
            enabled: true,
            auto_bind_tags: Vec::new(),
        };
        let a = store
            .create(org(), linked("prod", "-100"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        let b = store
            .create(
                other_org(),
                linked("ops", "-100"),
                WriteSource::Ui,
                10,
                None,
            )
            .await
            .unwrap();
        let own = store
            .create(org(), byo, WriteSource::Api, 10, None)
            .await
            .unwrap();

        // A BYO bot never reaches the webhook; its own send reports the move.
        let moved = store
            .follow_linked_chat_migration("-100", "-100100")
            .await
            .unwrap();
        assert_eq!(moved, 2);
        for (o, id) in [(org(), a.id), (other_org(), b.id)] {
            let got = store.get(o, id).await.unwrap().unwrap();
            assert_eq!(got.config.lifecycle_ref(), Some("-100100"));
        }
        assert_eq!(
            store
                .count_by_external_ref(ChannelKind::TelegramApp, "-100100")
                .await
                .unwrap(),
            2,
            "a later kick resolves by the new id"
        );
        assert_eq!(
            store
                .follow_linked_chat_migration("-100", "-100100")
                .await
                .unwrap(),
            0,
            "the twin service message finds nothing left to move"
        );

        assert!(
            store
                .follow_chat_migration(org(), own.id, "-100", "-100100")
                .await
                .unwrap()
        );
        assert!(
            !store
                .follow_chat_migration(org(), own.id, "-100", "-100100")
                .await
                .unwrap(),
            "already moved"
        );
        assert!(
            !store
                .follow_chat_migration(other_org(), own.id, "-100100", "-5")
                .await
                .unwrap(),
            "another org's id never reaches the row"
        );
        let got = store.get(org(), own.id).await.unwrap().unwrap();
        assert_eq!(
            got.config,
            ChannelConfig::Telegram(crate::domain::TelegramConfig {
                bot_token: "123:abc".into(),
                chat_id: "-100100".into(),
            })
        );
        assert_eq!(got.write_source, WriteSource::Api, "no operator touched it");
    }

    #[tokio::test]
    async fn a_delivery_that_lands_clears_the_failure_run() {
        let store = InMemoryNotificationChannelStore::new();
        let ch = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();

        for expected in 1..=3 {
            let health = store
                .record_delivery_outcome(org(), ch.id, false)
                .await
                .unwrap();
            assert_eq!(health.consecutive_failures, expected);
            assert!(health.failing_since.is_some());
        }
        let started = store
            .get(org(), ch.id)
            .await
            .unwrap()
            .unwrap()
            .failing_since
            .expect("run has a start");

        // A later failure extends the same run rather than restarting it.
        let health = store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        assert_eq!(health.failing_since, Some(started));

        assert!(store.get(org(), ch.id).await.unwrap().unwrap().enabled);

        let health = store
            .record_delivery_outcome(org(), ch.id, true)
            .await
            .unwrap();
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.failing_since, None);
    }

    /// A send that lands re-arms the claim, so a channel that dies twice is
    /// reported twice.
    #[tokio::test]
    async fn the_failure_alert_is_claimed_once_per_run() {
        let store = InMemoryNotificationChannelStore::new();
        let ch = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();

        macro_rules! claim {
            () => {
                store.claim_failure_alert(org(), ch.id).await.unwrap()
            };
        }

        assert!(claim!().is_none(), "nothing has failed yet");

        store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        assert!(claim!().is_some());
        assert!(claim!().is_none());

        store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        assert!(
            claim!().is_none(),
            "the same run must not be reported twice"
        );

        // Recovering and dying again inside the cooldown is a flapping
        // endpoint, not news: one report per channel per cooldown, whatever
        // the run boundaries do.
        store
            .record_delivery_outcome(org(), ch.id, true)
            .await
            .unwrap();
        store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        assert!(claim!().is_none());
    }

    /// A channel bound only to quiet monitors never fails, so a stamp that
    /// only moved on recovery would never move at all for the one case the
    /// console needs it for.
    #[tokio::test]
    async fn every_landed_delivery_stamps_the_channel() {
        let store = InMemoryNotificationChannelStore::new();
        let ch = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        assert_eq!(ch.last_delivered_at, None);

        store
            .record_delivery_outcome(org(), ch.id, true)
            .await
            .unwrap();
        let first = store.get(org(), ch.id).await.unwrap().unwrap();
        let stamped = first.last_delivered_at.expect("a landed delivery stamps");

        store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        let failed = store.get(org(), ch.id).await.unwrap().unwrap();
        assert_eq!(
            failed.last_delivered_at,
            Some(stamped),
            "a failure says nothing about when the channel last worked"
        );

        // Re-enabling resolves the failure run, not the delivery history.
        let save = |enabled| NotificationChannelUpdate {
            enabled: Some(enabled),
            ..Default::default()
        };
        store
            .update(org(), ch.id, save(false), WriteSource::Ui, None)
            .await
            .unwrap();
        store
            .update(org(), ch.id, save(true), WriteSource::Ui, None)
            .await
            .unwrap();
        let saved = store.get(org(), ch.id).await.unwrap().unwrap();
        assert_eq!(saved.consecutive_failures, 0);
        assert_eq!(saved.last_delivered_at, Some(stamped));
    }

    /// An unsent claim has to come back, or nobody is ever told.
    #[tokio::test]
    async fn a_released_claim_is_owed_again() {
        let store = InMemoryNotificationChannelStore::new();
        let ch = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        let claimed = store
            .claim_failure_alert(org(), ch.id)
            .await
            .unwrap()
            .expect("a fresh run is claimable");
        assert!(
            store
                .claim_failure_alert(org(), ch.id)
                .await
                .unwrap()
                .is_none()
        );

        // A release from some other run must not free this claim.
        store
            .release_failure_alert(org(), ch.id, claimed - Duration::seconds(1))
            .await
            .unwrap();
        assert!(
            store
                .claim_failure_alert(org(), ch.id)
                .await
                .unwrap()
                .is_none()
        );

        store
            .release_failure_alert(org(), ch.id, claimed)
            .await
            .unwrap();
        assert!(
            store
                .claim_failure_alert(org(), ch.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn only_the_enable_transition_clears_the_failure_run() {
        let store = InMemoryNotificationChannelStore::new();
        let ch = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        store.claim_failure_alert(org(), ch.id).await.unwrap();

        // Every ordinary save carries `enabled: true`.
        let save = |enabled| NotificationChannelUpdate {
            enabled: Some(enabled),
            ..Default::default()
        };
        store
            .update(
                org(),
                ch.id,
                NotificationChannelUpdate {
                    name: Some("ops renamed".into()),
                    ..save(true)
                },
                WriteSource::Ui,
                None,
            )
            .await
            .unwrap();
        let saved = store.get(org(), ch.id).await.unwrap().unwrap();
        assert_eq!(saved.consecutive_failures, 1);
        assert!(saved.failing_since.is_some());
        assert!(
            store
                .claim_failure_alert(org(), ch.id)
                .await
                .unwrap()
                .is_none(),
            "the run was already reported, so a rename owes no second mail"
        );

        // Off and back on is the operator saying it is dealt with.
        store
            .update(org(), ch.id, save(false), WriteSource::Ui, None)
            .await
            .unwrap();
        store
            .update(org(), ch.id, save(true), WriteSource::Ui, None)
            .await
            .unwrap();
        let fresh = store.get(org(), ch.id).await.unwrap().unwrap();
        assert_eq!(fresh.consecutive_failures, 0);
        assert_eq!(fresh.failing_since, None);
        // Re-enabling clears the report stamp too, so an operator who fixed the
        // endpoint hears about it again without waiting out the cooldown.
        store
            .record_delivery_outcome(org(), ch.id, false)
            .await
            .unwrap();
        assert!(
            store
                .claim_failure_alert(org(), ch.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn is_failing_needs_a_limit_and_a_run_that_reaches_it() {
        let mut ch = NotificationChannel {
            id: Uuid::now_v7(),
            name: "ops".into(),
            kind: ChannelKind::Slack,
            config: ChannelConfig::Slack(crate::domain::SlackConfig {
                webhook_url: "https://hooks.slack.com/services/x".into(),
                mention: None,
            }),
            enabled: true,
            disabled_reason: None,
            verified_at: None,
            consecutive_failures: 2,
            failing_since: Some(Utc::now()),
            last_delivered_at: None,
            write_source: WriteSource::Ui,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            auto_bind_tags: Vec::new(),
        };
        assert!(!ch.is_failing(3));
        ch.consecutive_failures = 3;
        assert!(ch.is_failing(3));
        assert!(!ch.is_failing(0));
    }

    #[tokio::test]
    async fn disable_self_service_by_id_is_idempotent() {
        let store = InMemoryNotificationChannelStore::new();
        let ch = store
            .create(org(), slack("ops"), WriteSource::Ui, 10, None)
            .await
            .unwrap();
        assert!(
            store
                .disable_self_service(ch.id, "recipient stopped delivery")
                .await
                .unwrap()
        );
        let got = store.get(org(), ch.id).await.unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(
            got.disabled_reason.as_deref(),
            Some("recipient stopped delivery")
        );
        // Already disabled, and unknown id, are both no-ops.
        assert!(!store.disable_self_service(ch.id, "again").await.unwrap());
        assert!(
            !store
                .disable_self_service(Uuid::now_v7(), "x")
                .await
                .unwrap()
        );
    }

    #[test]
    fn channel_stop_token_roundtrips_and_rejects_forgery() {
        let id = Uuid::from_u128(0xC1);
        let tok = channel_stop_token("secret", id);
        assert!(verify_channel_stop("secret", id, &tok));
        assert!(!verify_channel_stop("secret", id, "deadbeef"));
        assert!(!verify_channel_stop("other-secret", id, &tok));
        // A token for one channel cannot authorise another.
        assert!(!verify_channel_stop("secret", Uuid::from_u128(0xC2), &tok));
    }

    #[test]
    fn seal_open_round_trips_with_and_without_cipher() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        let cfg = ChannelConfig::Webhook(WebhookConfig {
            url: "https://x.test/h".into(),
            headers: BTreeMap::from([("X-Tok".into(), "s3cret".into())]),
            secret: None,
        });

        // No KEK: plaintext JSON, still opens.
        let v = seal(&cfg, None).unwrap();
        assert!(v.get(crate::security::ENC_KEY).is_none());
        assert_eq!(open(v, None).unwrap(), cfg);

        // KEK: sealed envelope, no plaintext secret on disk, opens back.
        let c = Cipher::from_base64(&STANDARD.encode([5u8; 32])).unwrap();
        let sealed = seal(&cfg, Some(&c)).unwrap();
        assert!(
            sealed
                .get(crate::security::ENC_KEY)
                .unwrap()
                .as_str()
                .unwrap()
                .starts_with("v1:")
        );
        assert!(!serde_json::to_string(&sealed).unwrap().contains("s3cret"));
        assert_eq!(open(sealed, Some(&c)).unwrap(), cfg);
    }

    #[test]
    fn sealed_value_without_kek_is_loud_error() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        let c = Cipher::from_base64(&STANDARD.encode([6u8; 32])).unwrap();
        let sealed = seal(
            &ChannelConfig::Slack(SlackConfig {
                webhook_url: "https://hooks.slack.com/x".into(),
                mention: None,
            }),
            Some(&c),
        )
        .unwrap();
        assert!(open(sealed, None).is_err());
    }

    #[test]
    fn plaintext_value_with_kek_is_loud_error() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;
        let c = Cipher::from_base64(&STANDARD.encode([7u8; 32])).unwrap();
        // Simulates a manual-INSERT path (test helper, ad-hoc SQL, demo seam)
        // that wrote a plaintext config into a KEK-mode deployment.
        let plaintext = seal(
            &ChannelConfig::Slack(SlackConfig {
                webhook_url: "https://hooks.slack.com/x".into(),
                mention: None,
            }),
            None,
        )
        .unwrap();
        assert!(open(plaintext, Some(&c)).is_err());
    }
}
