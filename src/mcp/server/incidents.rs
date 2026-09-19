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
use crate::domain::IncidentVisibility;
use crate::domain::incident::{NewIncidentUpdate, OpsIncident};
use crate::domain::public::IncidentStatusPhase;
use crate::quotas::ratelimit::RateLimitCategory;
use crate::storage::Actor;
use crate::storage::incident_ops::opening_update_message;

use crate::mcp::auth::McpAuth;
use crate::mcp::confirm::require_confirmation;
use crate::mcp::error::McpToolError;
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
        let label = self.incident_label(auth.org, id).await?;
        // Publishing posts an opening update, and that update is what reaches
        // subscribers, so the prompt has to show the words they will receive.
        let opening = opening_update_message(title.as_deref(), description.as_deref());
        require_confirmation(
            ctx,
            auth,
            format!(
                "Publish {label} on your public status pages?{}\n\nSubscribers receive:\n\n\"{}\"",
                match &title {
                    Some(t) => format!(" Headline: \"{}\".", sanitize_prompt(t)),
                    None => String::new(),
                },
                sanitize_prompt(&opening)
            ),
        )
        .await?;
        let incident = self
            .state
            .incident_ops_store
            .publish(auth.org, id, title, description, Actor::Mcp(auth.user_id))
            .await
            .map_err(|e| McpToolError::internal(format!("publish_incident: {e}")))?
            .ok_or_else(|| McpToolError::not_found("incident not found"))?;
        self.invalidate_status_pages(auth.org, incident.target_id)
            .await;
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
        self.invalidate_status_pages(auth.org, incident.target_id)
            .await;
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
}
