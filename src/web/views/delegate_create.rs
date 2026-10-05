//! The tail of a channel made through a delegation link: the quota-capped
//! create and link attach the manual form and the OAuth callbacks share, and
//! the audit entry the Telegram bot records too.

use serde_json::json;

use crate::app::AppState;
use crate::channels::spawn_send_verification;
use crate::domain::{ChannelConfig, NotificationChannel, OrgId};
use crate::error::Result;
use crate::storage::channel_link_codes::ConsumedLink;
use crate::storage::orgs::record_audit_tx;
use crate::web::views::notification_channels::{QuotaBlockLog, create_channel_deduped};

/// Quota-capped create + link attach, shared by the manual form and the
/// OAuth callbacks' delegate branch.
pub(crate) async fn finish_create(
    state: &AppState,
    link: &ConsumedLink,
    base_name: &str,
    config: ChannelConfig,
    via: &'static str,
) -> Result<NotificationChannel> {
    let limit = i64::from(
        state
            .quotas
            .limit_for_org(link.org_id)
            .await?
            .max_notification_channels,
    );
    let channel = create_channel_deduped(
        state.notification_channel_store.as_ref(),
        link.org_id,
        base_name,
        config,
        limit,
        QuotaBlockLog {
            db: state.db.clone(),
            user: None,
            flow: via,
        },
    )
    .await?;
    // Best-effort: the channel exists and works either way, and never
    // failing after the create means a restore can never resurrect a link
    // whose channel was already made.
    if let Err(err) = state
        .channel_link_code_store
        .attach_channel(link.id, channel.id)
        .await
    {
        tracing::warn!(?err, channel_id = %channel.id, "delegate link attach failed");
    }
    if channel.awaiting_verification() {
        spawn_send_verification(state, link.org_id, &channel);
    }
    tracing::info!(
        org_id = %link.org_id.0,
        channel_id = %channel.id,
        via,
        "channel created via delegation link"
    );
    Ok(channel)
}

/// Best-effort compliance trail; the channel exists either way and the
/// tracing line above already records the event operationally.
pub(crate) async fn audit_delegated_create(
    state: &AppState,
    org: OrgId,
    channel: &NotificationChannel,
    client_ip: &str,
) {
    let Some(pool) = state.db.as_ref() else {
        return;
    };
    let ip_hash = crate::auth::hash_fingerprint(&state.cfg.auth.fingerprint_salt, client_ip);
    let meta = json!({
        "channel_id": channel.id,
        "kind": channel.kind.as_db_str(),
        "ip_hash": ip_hash,
    });
    let res = async {
        let mut tx = pool.begin().await?;
        record_audit_tx(&mut tx, org, None, "channel.created_via_delegation", meta).await?;
        tx.commit().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(err) = res {
        tracing::warn!(org_id = %org.0, error = %err, "delegation audit write failed");
    }
}
