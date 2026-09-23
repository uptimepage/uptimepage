pub mod account;
pub mod agents;
pub mod api_tokens;
pub mod auth;
pub mod billing;
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
mod sign_in;
pub mod status_page;
pub mod support;
pub mod tags;
pub mod targets;
pub mod usage;
pub mod validation;
pub mod variables;

use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{AccountId, OrgId, UserId};
use crate::error::codes;
use crate::error::{AppError, Result};

/// Owning the org is not owning the account it bills to: a non-owner gets
/// 403 rather than the pool or the subscription.
pub(crate) async fn owned_account(state: &AppState, org: OrgId, user: UserId) -> Result<AccountId> {
    let pool = state.require_db()?;
    let account = crate::storage::accounts::account_for_org(pool, org).await?;
    if crate::storage::accounts::account_for_user(pool, user).await? != Some(account) {
        return Err(AppError::forbidden_code(
            codes::ACCOUNT_OWNER_REQUIRED,
            "only the account owner can act on its plan",
        ));
    }
    Ok(account)
}

/// Bust the cached pages an incident shows on: those carrying its monitor,
/// or the ones it was published to when it has none.
pub(crate) async fn invalidate_incident_pages(
    state: &AppState,
    org: OrgId,
    incident: &crate::domain::OpsIncident,
) {
    if incident.target_id.is_some() {
        return invalidate_pages_for(state, org, incident.target_id.as_slice()).await;
    }
    match state
        .incident_ops_store
        .status_pages(org, incident.id)
        .await
    {
        Ok(pages) => invalidate_page_ids(state, &pages).await,
        Err(e) => tracing::warn!(error = %e, "could not resolve pages for cache invalidation"),
    }
}

async fn invalidate_page_ids(state: &AppState, pages: &[Uuid]) {
    for page in pages {
        state
            .public_source
            .invalidate(crate::domain::StatusPageId(*page))
            .await;
    }
}

/// Publish, then bust every page the incident shows on, and the pages a new
/// `status_page_ids` list just took it off.
pub(crate) async fn publish_and_invalidate(
    state: &AppState,
    org: OrgId,
    id: Uuid,
    public_title: Option<String>,
    public_description: Option<String>,
    status_page_ids: Option<Vec<Uuid>>,
    actor: crate::storage::Actor,
) -> Result<Option<crate::domain::OpsIncident>> {
    let before = match status_page_ids {
        Some(_) => state.incident_ops_store.status_pages(org, id).await?,
        None => Vec::new(),
    };
    let Some(incident) = state
        .incident_ops_store
        .publish(
            org,
            id,
            public_title,
            public_description,
            status_page_ids,
            actor,
        )
        .await?
    else {
        return Ok(None);
    };
    invalidate_incident_pages(state, org, &incident).await;
    invalidate_page_ids(state, &before).await;
    Ok(Some(incident))
}

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
