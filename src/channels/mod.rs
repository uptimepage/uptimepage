//! What every surface that creates or repairs a notification channel must do
//! the same way: validate the name and config, refuse operator-managed kinds
//! and abusive destinations, and start the verification mail.

use chrono::Utc;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::url::token_link;
use crate::config::TransactionalEmailConfig;
use crate::domain::{ChannelConfig, NotificationChannel, OrgId, UserId, validate_channel_name};
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::quotas::QuotaService;
use crate::storage::channel_verification;
use crate::storage::{LinkCodeStatus, NotificationChannelStore};

/// Sender + From identity for alert email; built per call from app state.
pub fn email_delivery(state: &AppState) -> crate::notifier::EmailDelivery {
    crate::notifier::EmailDelivery {
        sender: state.email_sender.clone(),
        from_address: state.cfg.email.from_address.clone(),
        from_name: state.cfg.email.from_name.clone(),
    }
}

pub fn stop_link(state: &AppState, channel_id: Uuid) -> Option<String> {
    crate::storage::notification_channels::channel_stop_url(
        &state.cfg.auth.public_base_url,
        &state.alert_channel_stop_secret,
        channel_id,
    )
}

/// One verification mail, composed identically for the create/update hook
/// and the resend endpoint: mint a token (all daily caps enforced in the
/// mint) and send the link off the response path.
pub async fn mint_and_send_verification(
    state: &AppState,
    org: OrgId,
    channel_id: Uuid,
    channel_name: String,
    to: String,
) -> Result<channel_verification::MintOutcome> {
    let pool = state.require_db()?;
    let outcome = channel_verification::mint(pool, org, channel_id, &to).await?;
    if let channel_verification::MintOutcome::Created { token } = &outcome {
        let delivery = email_delivery(state);
        let org_name = crate::storage::orgs::get_org(pool, org)
            .await
            .ok()
            .flatten()
            .map(|o| o.name);
        let decline_url = stop_link(state, channel_id);
        let outgoing = TransactionalEmail {
            from: EmailAddress::new(delivery.from_address, delivery.from_name),
            to: EmailAddress::new(to.clone(), to),
            template: EmailTemplate::ChannelVerification {
                channel_name,
                verify_url: token_link(&state.cfg.auth.public_base_url, "/verify-channel", token),
                expires_hours: channel_verification::VERIFICATION_TTL_HOURS,
                org_name,
                decline_url,
            },
        };
        let sender = delivery.sender;
        tokio::spawn(async move {
            if let Err(err) = sender.send(outgoing).await {
                tracing::warn!(%channel_id, error = %err, "channel verification mail failed");
            }
        });
    }
    Ok(outcome)
}

/// Skip the verify round-trip when the destination is the confirmed login email
/// of a member of this org: joining the org already proved control of that
/// mailbox, so no separate opt-in click is needed. A third-party address still
/// falls through to the mailed confirmation. Best-effort — a lookup failure
/// leaves the channel unverified.
pub async fn fast_verify_member_email(
    state: &AppState,
    org: OrgId,
    ch: NotificationChannel,
) -> Result<NotificationChannel> {
    let ChannelConfig::Email(cfg) = &ch.config else {
        return Ok(ch);
    };
    if !ch.awaiting_verification() {
        return Ok(ch);
    }
    let Some(pool) = state.db.as_ref() else {
        return Ok(ch);
    };
    let (is_member,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (
            SELECT 1 FROM memberships m
            JOIN users u ON u.id = m.user_id
            WHERE m.org_id = $1
              AND u.deleted_at IS NULL
              AND u.email_verified_at IS NOT NULL
              AND u.email = $2
        )",
    )
    .bind(org.0)
    .bind(&cfg.to)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("fast-verify lookup: {e}")))?;
    if !is_member {
        return Ok(ch);
    }
    let mut ch = ch;
    if state
        .notification_channel_store
        .set_verified(org, ch.id, ch.updated_at)
        .await?
    {
        ch.verified_at = Some(Utc::now());
    }
    Ok(ch)
}

/// Best-effort hook on create / config replace. Cap breaches and failures
/// only log — the channel stays unverified and the operator has the resend
/// action.
pub fn spawn_send_verification(state: &AppState, org: OrgId, ch: &NotificationChannel) {
    if !ch.awaiting_verification() {
        return;
    }
    let ChannelConfig::Email(cfg) = &ch.config else {
        return;
    };
    if state.db.is_none() {
        return;
    }
    let state = state.clone();
    let (channel_id, channel_name, to) = (ch.id, ch.name.clone(), cfg.to.clone());
    tokio::spawn(async move {
        match mint_and_send_verification(&state, org, channel_id, channel_name, to).await {
            Ok(channel_verification::MintOutcome::Created { .. }) => {}
            Ok(channel_verification::MintOutcome::LimitReached) => {
                tracing::warn!(%channel_id, "channel verification mint rate-limited");
            }
            Err(err) => {
                tracing::warn!(%channel_id, error = %err, "channel verification mint failed");
            }
        }
    });
}

pub fn delegate_status_parts(status: LinkCodeStatus) -> (&'static str, Option<Uuid>) {
    match status {
        LinkCodeStatus::Pending => ("pending", None),
        LinkCodeStatus::Consumed { channel_id } => ("consumed", Some(channel_id)),
        LinkCodeStatus::Expired => ("expired", None),
    }
}

/// A caller-supplied operator-managed config (`telegram_app` chat id) would
/// let anyone alert-spam an arbitrary destination with our credentials —
/// only the transport's own flow may mint one.
pub fn reject_managed_kind(cfg: &ChannelConfig) -> Result<()> {
    if cfg.operator_managed() {
        return Err(AppError::unprocessable(
            codes::CHANNEL_KIND_MANAGED,
            "telegram channels are created by linking a chat through the bot; \
             mint a link code instead of supplying config",
        ));
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    validate_channel_name(name)
        .map_err(|m| AppError::bad_request_field(codes::CHANNEL_NAME_INVALID, m, "name"))
}

/// Redaction-sentinel guard first (so a `GET → PATCH` round-trip or a
/// copy-pasted redacted create reports `REDACTION_SENTINEL`, not a generic
/// invalid-URL — `***` does not parse as a URL), then the structural
/// transport check.
pub fn validate_config(cfg: &ChannelConfig) -> Result<()> {
    if cfg.has_redaction_sentinel() {
        return Err(AppError::bad_request_field(
            codes::REDACTION_SENTINEL,
            "config still contains the redaction sentinel; send the real secret \
             or omit config to keep the stored value",
            "config",
        ));
    }
    cfg.validate()
        .map_err(|m| AppError::bad_request_field(codes::INVALID_CHANNEL_CONFIG, m, "config"))?;
    Ok(())
}

/// Deny-list gate for the config's outbound URL, mirroring the targets
/// test path: a hit is recorded as an `abuse_blocked` quota event and
/// rejected. Transports with a fixed vendor endpoint expose no URL and
/// pass through.
/// `established` skips the deliverability gate only; the deny-list is a
/// security control and always applies.
pub async fn check_channel_abuse(
    state: &AppState,
    org: OrgId,
    config: &ChannelConfig,
    established: bool,
) -> Result<()> {
    if let ChannelConfig::Email(cfg) = config {
        let ops = crate::security::abuse::operator_domains(
            &state.cfg.email.from_address,
            &state.cfg.auth.public_base_url,
        );
        if let Some(detail) = crate::security::abuse::blocked_email_destination(&cfg.to, &ops) {
            crate::quotas::service::record_quota_event(
                state.db.clone(),
                Some(org),
                None,
                "abuse_blocked",
                Some("email_destination"),
                serde_json::json!({ "detail": detail }),
                None,
            );
            return Err(AppError::bad_request_field(
                codes::EMAIL_DESTINATION_BLOCKED,
                detail,
                "config.to",
            ));
        }
        // Refused whatever `signup_policy` says: an alert nobody can read is
        // worse than no channel, because it looks configured. Not for an
        // already-verified address — a list changing its mind would lock the
        // owner out of a channel that is still delivering, and the pinned floor
        // is compile-time, so nothing short of a release could clear it.
        if !established
            && let Some(risk) = state
                .undeliverable_email(&cfg.to, "notification_channel")
                .await
        {
            crate::quotas::service::record_quota_event(
                state.db.clone(),
                Some(org),
                None,
                "abuse_blocked",
                Some("email_destination"),
                serde_json::json!({ "detail": risk.as_db_str() }),
                None,
            );
            return Err(risk.into_app_error("config.to"));
        }
        return Ok(());
    }
    let Some(url) = config.abuse_url() else {
        return Ok(());
    };
    let Some(hit) = state.abuse.inspect_url(url) else {
        return Ok(());
    };
    crate::quotas::service::record_quota_event(
        state.db.clone(),
        Some(org),
        None,
        "abuse_blocked",
        Some(hit.quota_name()),
        serde_json::json!({ "detail": hit.detail }),
        None,
    );
    Err(hit.into_app_error())
}

/// The owner's address as the org's first alert channel, pre-verified: the
/// sign-in or bootstrap that reached here already proved control of the
/// inbox. Never fails the caller: an org with no channel is recoverable, a
/// sign-in or bootstrap that aborts is not.
pub async fn seed_owner_email(
    store: &dyn NotificationChannelStore,
    quotas: &QuotaService,
    email_cfg: &TransactionalEmailConfig,
    org: OrgId,
    user: UserId,
    email: &str,
) {
    // A channel seeded against the log-only sender reads as configured while
    // dropping every alert.
    if !email_cfg.delivers() {
        return;
    }
    let seeded = async {
        let limit = i64::from(quotas.limit_for_org(org).await?.max_notification_channels);
        store.seed_owner_email(org, email, user, limit).await
    }
    .await;
    match seeded {
        Ok(Some(ch)) => {
            tracing::info!(org_id = %org.0, channel_id = %ch.id, "seeded the owner's email alert channel")
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, org_id = %org.0, "seeding the owner's email alert channel failed")
        }
    }
}
