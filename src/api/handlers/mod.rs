pub mod account;
pub mod agents;
pub mod api_tokens;
pub mod auth;
pub mod dashboard;
pub mod escalation_policies;
pub mod health;
pub mod holds;
pub mod identities;
pub mod incidents;
pub mod invitations;
pub mod magic_link;
pub mod maintenance;
pub mod me;
pub mod notification_channels;
pub mod on_call;
pub mod operator;
pub mod operator_accounts;
pub mod orgs;
pub mod passkeys;
pub mod public;
pub mod results;
pub mod shares;
pub mod status_page;
pub mod support;
pub mod tags;
pub mod targets;
pub mod usage;
pub mod validation;
pub mod variables;

use uuid::Uuid;

use crate::app::AppState;
use crate::domain::OrgId;

/// Bust cached public status pages that surface any of `ids`.
pub(crate) async fn invalidate_pages_for(state: &AppState, org: OrgId, ids: &[Uuid]) {
    if ids.is_empty() {
        return;
    }
    match state.status_page_store.pages_for_targets(org, ids).await {
        Ok(pages) => {
            for page in pages {
                state.public_source.invalidate(page).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not resolve pages for cache invalidation"),
    }
}
