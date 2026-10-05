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
use crate::web::views::notification_channels::create_capped_channel;

/// Quota-capped create + link attach, shared by the manual form and the
/// OAuth callbacks' delegate branch. A failed create restores the link.
pub(crate) async fn finish_create(
    state: &AppState,
    link: &ConsumedLink,
    base_name: &str,
    config: ChannelConfig,
    via: &'static str,
) -> Result<NotificationChannel> {
    let created = create_capped_channel(state, link.org_id, base_name, config, None, via).await;
    let channel = match created {
        Ok(ch) => ch,
        Err(err) => {
            restore_link(state, link, via).await;
            return Err(err);
        }
    };
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

/// Un-spends a link after a failed create. Best-effort: the caller's own
/// error is the one worth returning, and a failed restore only leaves the
/// link spent.
pub(crate) async fn restore_link(state: &AppState, link: &ConsumedLink, via: &'static str) {
    if let Err(err) = state.channel_link_code_store.restore(link.id).await {
        tracing::warn!(
            ?err,
            org_id = %link.org_id.0,
            link_id = %link.id,
            via,
            "delegate link restore failed"
        );
    }
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
