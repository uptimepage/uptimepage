//! Incident write bodies: acknowledge, resolve, post a public update, publish
//! and unpublish.
//!
//! No audit here — the wrapper in [`super::tools_write`] records the outcome.

use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::service::RequestContext;
use uuid::Uuid;

use crate::api::handlers::validation::{MAX_DESCRIPTION, MAX_TITLE};
use crate::auth::scope::Scope;
use crate::domain::incident::{NewIncidentUpdate, OpsIncident};
use crate::domain::public::IncidentStatusPhase;
use crate::domain::{IncidentVisibility, OrgId};
use crate::quotas::ratelimit::RateLimitCategory;
use crate::storage::Actor;
use crate::storage::incident_ops::opening_update_message;

use crate::mcp::auth::McpAuth;
use crate::mcp::confirm::require_confirmation;
use crate::mcp::error::{McpToolError, config_error};
use crate::mcp::schema::{
    IncidentActionArgs, IncidentActionResult, IncidentIdArg, IncidentUpdatePosted,
    IncidentVisibilityResult, PostIncidentUpdateArgs, PublishIncidentArgs,
};

use super::args::{parse_phase, parse_uuid};
use super::text::{
    clean_incident_note, clean_public_text, incident_action_result, sanitize_prompt,
};
use super::view::visibility_result;
use super::{MAX_INCIDENT_MESSAGE_LEN, McpServer};

impl McpServer {
    /// `acknowledge_incident` body (no audit — the wrapper's `finish` records it).
    pub(super) async fn acknowledge_incident_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &IncidentActionArgs,
    ) -> Result<Json<IncidentActionResult>, McpToolError> {
        auth.require(Scope::IncidentsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "incident id")?;
        let note = clean_incident_note(args.note.as_deref())?;
        require_confirmation(
            ctx,
            auth,
            "Acknowledge this incident (take ownership, stop escalation)?".to_string(),
        )
        .await?;
        let outcome = self
            .state
            .incident_ops_store
            .acknowledge(auth.org, id, Actor::Mcp(auth.user_id), note, None)
            .await
            .map_err(|e| McpToolError::internal(format!("acknowledge_incident: {e}")))?;
        incident_action_result(id, outcome)
    }

    /// `resolve_incident` body.
    pub(super) async fn resolve_incident_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &IncidentActionArgs,
    ) -> Result<Json<IncidentActionResult>, McpToolError> {
        auth.require(Scope::IncidentsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "incident id")?;
        let note = clean_incident_note(args.note.as_deref())?;
        require_confirmation(ctx, auth, "Resolve this incident?".to_string()).await?;
        let outcome = self
            .state
            .incident_ops_store
            .resolve(auth.org, id, Actor::Mcp(auth.user_id), note)
            .await
            .map_err(|e| McpToolError::internal(format!("resolve_incident: {e}")))?;
        if let crate::storage::LifecycleOutcome::Updated(inc) = &outcome {
            self.state.signal_incident(
                auth.org,
                inc.id,
                crate::domain::NotificationReason::Resolved,
            );
        }
        incident_action_result(id, outcome)
    }

    /// `post_incident_update` body — the public-facing status-page update.
    pub(super) async fn post_incident_update_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &PostIncidentUpdateArgs,
    ) -> Result<Json<IncidentUpdatePosted>, McpToolError> {
        auth.require(Scope::IncidentsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "incident id")?;
        let phase = match args.phase.as_deref() {
            Some(p) => parse_phase(p)?,
            None => IncidentStatusPhase::Investigating,
        };
        let message = args.message.trim().to_string();
        if message.is_empty() {
            return Err(McpToolError::invalid_argument("message must not be empty"));
        }
        if message.chars().count() > MAX_INCIDENT_MESSAGE_LEN {
            return Err(McpToolError::invalid_argument(format!(
                "message must be at most {MAX_INCIDENT_MESSAGE_LEN} characters"
            )));
        }
        // A public update only reaches customers on a published incident.
        // Posting to an internal one would silently vanish (and resurface if it
        // were later published), so reject it with a clear, actionable error.
        let incident = self
            .state
            .incident_ops_store
            .get(auth.org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("post_incident_update: {e}")))?
            .ok_or_else(|| McpToolError::not_found("incident not found"))?;
        if incident.visibility != IncidentVisibility::Public {
            return Err(McpToolError::invalid_argument(
                "incident is not published; call publish_incident first, then post the update",
            ));
        }
        let label = self.label_for(auth.org, &incident).await?;
        require_confirmation(
            ctx,
            auth,
            format!(
                "Publish this update on your public status page, on {label}?\n\n\"{}\"",
                sanitize_prompt(&message)
            ),
        )
        .await?;
        let posted = self
            .state
            .incident_narration_store
            .append_update(
                auth.org,
                id,
                NewIncidentUpdate { phase, message },
                Some("mcp".to_string()),
            )
            .await
            .map_err(|e| McpToolError::internal(format!("post_incident_update: {e}")))?
            .ok_or_else(|| McpToolError::not_found("incident not found"))?;
        Ok(Json(IncidentUpdatePosted {
            incident_id: id.to_string(),
            posted_at: posted.posted_at.to_rfc3339(),
        }))
    }

    pub(super) async fn publish_incident_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &PublishIncidentArgs,
    ) -> Result<Json<IncidentVisibilityResult>, McpToolError> {
        auth.require(Scope::IncidentsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "incident id")?;
        let title = clean_public_text(args.public_title.as_deref(), "public_title", MAX_TITLE)?;
        let description = clean_public_text(
            args.public_description.as_deref(),
            "public_description",
            MAX_DESCRIPTION,
        )?;
        let pages = args
            .status_page_ids
            .as_deref()
            .map(|ids| {
                ids.iter()
                    .map(|p| parse_uuid(p, "status page id"))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let incident = self
            .state
            .incident_ops_store
            .get(auth.org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("get incident: {e}")))?
            .ok_or_else(|| McpToolError::not_found("incident not found"))?;
        let label = self.label_for(auth.org, &incident).await?;
        let where_ = self
            .publish_destination(auth.org, &incident, pages.as_deref())
            .await?;
        // Publishing posts an opening update, and that update is what reaches
        // subscribers, so the prompt has to show the words they will receive.
        let opening = opening_update_message(title.as_deref(), description.as_deref());
        require_confirmation(
            ctx,
            auth,
            format!(
                "Publish {label} on {where_}?{}\n\nSubscribers receive:\n\n\"{}\"",
                match &title {
                    Some(t) => format!(" Headline: \"{}\".", sanitize_prompt(t)),
                    None => String::new(),
                },
                sanitize_prompt(&opening)
            ),
        )
        .await?;
        let incident = crate::api::handlers::publish_and_invalidate(
            &self.state,
            auth.org,
            id,
            title,
            description,
            pages,
            Actor::Mcp(auth.user_id),
        )
        .await
        .map_err(config_error)?
        .ok_or_else(|| McpToolError::not_found("incident not found"))?;
        Ok(Json(visibility_result(id, incident.visibility)))
    }

    pub(super) async fn unpublish_incident_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &IncidentIdArg,
    ) -> Result<Json<IncidentVisibilityResult>, McpToolError> {
        auth.require(Scope::IncidentsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "incident id")?;
        let label = self.incident_label(auth.org, id).await?;
        require_confirmation(
            ctx,
            auth,
            format!("Hide {label} from your public status pages?"),
        )
        .await?;
        let incident = self
            .state
            .incident_ops_store
            .unpublish(auth.org, id, Actor::Mcp(auth.user_id))
            .await
            .map_err(|e| McpToolError::internal(format!("unpublish_incident: {e}")))?
            .ok_or_else(|| McpToolError::not_found("incident not found"))?;
        self.invalidate_status_pages(auth.org, &incident).await;
        Ok(Json(visibility_result(id, incident.visibility)))
    }

    /// How a confirmation prompt names the incident it is about, so approving
    /// one is never approving an unnamed thing: its monitor, else its operator
    /// title. Loading it here also rejects an unknown id before prompting.
    async fn incident_label(
        &self,
        org: crate::domain::OrgId,
        id: Uuid,
    ) -> Result<String, McpToolError> {
        let incident = self
            .state
            .incident_ops_store
            .get(org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("get incident: {e}")))?
            .ok_or_else(|| McpToolError::not_found("incident not found"))?;
        self.label_for(org, &incident).await
    }

    async fn label_for(
        &self,
        org: crate::domain::OrgId,
        incident: &OpsIncident,
    ) -> Result<String, McpToolError> {
        // A failed name lookup fails the whole call: degrading to an unnamed
        // prompt would ask the user to approve they-know-not-what, which is
        // the one thing this confirmation exists to prevent.
        let monitor = match incident.target_id {
            Some(target_id) => self
                .state
                .target_store
                .get(org, target_id)
                .await
                .map_err(|e| McpToolError::internal(format!("get monitor: {e}")))?
                .map(|t| t.name),
            None => None,
        };
        Ok(match (monitor, incident.title.as_deref()) {
            (Some(name), _) => format!("the incident on \"{}\"", sanitize_prompt(&name)),
            (None, Some(title)) => format!("the incident \"{}\"", sanitize_prompt(title)),
            // A declared incident can carry neither monitor nor title; the id is
            // then the only handle, and it beats approving an unnamed thing.
            (None, None) => format!("incident {}", incident.id),
        })
    }

    /// Where a publish will show the incident, refused up front when the store
    /// would refuse it, so the user never confirms a publish that cannot happen.
    async fn publish_destination(
        &self,
        org: OrgId,
        incident: &OpsIncident,
        pages: Option<&[Uuid]>,
    ) -> Result<String, McpToolError> {
        if incident.target_id.is_some() {
            if pages.is_some_and(|p| !p.is_empty()) {
                return Err(config_error(
                    crate::storage::incident_ops::pages_with_monitor(),
                ));
            }
            return Ok("the status pages carrying its monitor".to_string());
        }
        let pages = match pages {
            Some(ids) => ids.to_vec(),
            None => self
                .state
                .incident_ops_store
                .status_pages(org, incident.id)
                .await
                .map_err(|e| McpToolError::internal(format!("incident pages: {e}")))?,
        };
        if pages.is_empty() {
            return Err(config_error(
                crate::storage::incident_ops::status_page_required(),
            ));
        }
        self.page_names(org, &pages).await
    }

    async fn page_names(&self, org: OrgId, ids: &[Uuid]) -> Result<String, McpToolError> {
        let pages = self
            .state
            .status_page_store
            .list(org)
            .await
            .map_err(|e| McpToolError::internal(format!("list status pages: {e}")))?;
        let names: Vec<String> = pages
            .iter()
            .filter(|p| ids.contains(&p.id.0))
            .map(|p| format!("\"{}\"", sanitize_prompt(&p.name)))
            .collect();
        let mut wanted = ids.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        if names.len() < wanted.len() {
            return Err(config_error(
                crate::storage::incident_ops::unknown_status_pages(wanted.len() - names.len()),
            ));
        }
        Ok(match names.len() {
            1 => format!("the status page {}", names[0]),
            _ => format!("the status pages {}", names.join(", ")),
        })
    }
}
