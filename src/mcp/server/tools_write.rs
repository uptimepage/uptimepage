//! The write tools: scope-gated, elicitation-confirmed and audited.
//!
//! Each is a thin wrapper: build the audit args, run the inner body, then
//! [`McpServer::finish`] writes exactly one audit row for the outcome — so
//! EVERY path (insufficient scope, declined, bad input, not-found, error,
//! success) is recorded, not just the happy path. The bodies themselves live
//! in [`super::monitors`] and [`super::incidents`].

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, tool, tool_router};
use serde_json::json;

use crate::mcp::auth::McpAuth;
use crate::mcp::error::McpToolError;
use crate::mcp::schema::{
    AddComponentsArgs, CheckRunResult, ComponentUpdated, ComponentsAdded, CreateMonitorArgs,
    CreateMonitorsArgs, CreateStatusPageArgs, IncidentActionArgs, IncidentActionResult,
    IncidentIdArg, IncidentUpdatePosted, IncidentVisibilityResult, MonitorCreated, MonitorIdArg,
    MonitorStateResult, MonitorUpdateResult, MonitorsCreated, PostIncidentUpdateArgs,
    PublishIncidentArgs, StatusPageWritten, UpdateComponentArgs, UpdateMonitorArgs,
    UpdateStatusPageArgs,
};

use super::McpServer;
use super::args::requested_fields;

#[tool_router(router = write_router, vis = "pub(super)")]
impl McpServer {
    #[tool(
        description = "Run a check on a monitor immediately and record the result. Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. A down result may fire the org's normal alerts. Heartbeat monitors cannot be probed (they wait for your systems to ping them). Not read-only.",
        title = "Run check now",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn run_check_now(
        &self,
        Parameters(args): Parameters<MonitorIdArg>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<CheckRunResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id });
        let result = self.run_check_now_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "run_check_now", args_json, result)
            .await
    }

    #[tool(
        description = "Pause a monitor (stop its checks until resumed). Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. Not read-only; idempotent.",
        title = "Pause monitor",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn pause_monitor(
        &self,
        Parameters(args): Parameters<MonitorIdArg>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorStateResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id });
        let result = self.set_enabled_inner(&ctx, &auth, &args, false).await;
        self.finish(pool, &auth, "pause_monitor", args_json, result)
            .await
    }

    /// Create a monitor. The check runs once first and its result is shown in
    /// the confirmation, so a misconfigured check is visible before it exists.
    #[tool(
        description = "Create a monitor for an http, tcp, ping, dns, tls_cert, domain_expiry or heartbeat check. The check is run once before anything is saved and the result is shown to the user along with every setting it would apply; where the client can show a prompt, nothing is created unless they approve; otherwise the monitor is created on the token's scope and the trial result comes back with it. Bind it to alerts as you create it: pass channel_ids from list_notification_channels (this needs the channels:read scope), and if the org has no channel yet, say so rather than leaving a monitor that pages nobody. Leave regions unset unless the user named where they want the check to run from — omitted, it probes from the operator's default set, which is already the intended coverage; naming more regions than the plan allows is refused outright. Request headers and a request body can be set, but a credential must be referenced rather than pasted: write `Bearer {{ my_key }}` and call list_variables for the keys this org has. A URL carrying a username or password is refused, and browser flows cannot be created here — add those in the app. Not read-only.",
        title = "Create monitor",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_monitor(
        &self,
        Parameters(args): Parameters<CreateMonitorArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorCreated>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let result = self.create_monitor_inner(&ctx, &auth, &args).await;
        let args_json = match &result {
            Ok(Json(created)) => json!({
                "id": created.id,
                "name": created.name,
                "address": created.address,
                "interval_secs": created.interval_secs,
                // A caller's choice now, and this row is its only record.
                "regions": created.regions,
                // Who this pages is the part an incident review asks about.
                "alerts": created.alerts,
            }),
            Err(_) => json!({ "name": args.name, "regions": args.regions }),
        };
        self.finish(pool, &auth, "create_monitor", args_json, result)
            .await
    }

    /// Retune an existing monitor's alerting and cadence. Read it first with
    /// `get_monitor`: tags and channel bindings are replaced whole, not merged.
    #[tool(
        description = "Change how loudly a monitor is watched: check interval, alert confirmations, recovery notices, reminder interval, tags, group, the multi-region detection quorum, and which notification channels it alerts (channel_ids replaces the whole set, and needs the channels:read scope). It cannot change what the check watches — name, address, assertions, expected status, headers, body, probe regions and owner are refused. A monitor managed by Terraform is refused outright. Shows the old and new value of every field before it runs, where the client can show a prompt; otherwise runs on the token's scope. Not read-only.",
        title = "Retune monitor",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn update_monitor(
        &self,
        Parameters(args): Parameters<UpdateMonitorArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorUpdateResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let result = self.update_monitor_inner(&ctx, &auth, &args).await;
        // This path leaves `write_source` alone, so the audit row is the only
        // record of what an MCP client changed.
        let args_json = match &result {
            Ok(Json(applied)) => json!({ "id": args.id, "changes": applied.changes }),
            Err(_) => json!({ "id": args.id, "requested": requested_fields(&args) }),
        };
        self.finish(pool, &auth, "update_monitor", args_json, result)
            .await
    }

    #[tool(
        description = "Resume a paused monitor (restart its checks). Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. Not read-only; idempotent.",
        title = "Resume monitor",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn resume_monitor(
        &self,
        Parameters(args): Parameters<MonitorIdArg>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorStateResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id });
        let result = self.set_enabled_inner(&ctx, &auth, &args, true).await;
        self.finish(pool, &auth, "resume_monitor", args_json, result)
            .await
    }

    #[tool(
        description = "Acknowledge an incident: take ownership and halt escalation. Internal/operational only — does NOT post anything to the public status page. Use post_incident_update for customer-facing updates. Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. Not read-only.",
        title = "Acknowledge incident",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn acknowledge_incident(
        &self,
        Parameters(args): Parameters<IncidentActionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentActionResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id });
        let result = self.acknowledge_incident_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "acknowledge_incident", args_json, result)
            .await
    }

    #[tool(
        description = "Resolve an incident (mark the operational state resolved). Internal only — does not post to the public status page. Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. Not read-only.",
        title = "Resolve incident",
        // Closing a live incident ends the escalation that would page someone.
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true)
    )]
    async fn resolve_incident(
        &self,
        Parameters(args): Parameters<IncidentActionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentActionResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id });
        let result = self.resolve_incident_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "resolve_incident", args_json, result)
            .await
    }

    #[tool(
        description = "Post a public, customer-facing update to an incident's status-page timeline (phase + message). This is what your subscribers and status-page visitors see. Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. Not read-only.",
        title = "Post incident update",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn post_incident_update(
        &self,
        Parameters(args): Parameters<PostIncidentUpdateArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentUpdatePosted>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id, "phase": args.phase });
        let result = self.post_incident_update_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "post_incident_update", args_json, result)
            .await
    }

    #[tool(
        description = "Publish an incident so it appears on every status page carrying the affected monitor, optionally seeding the public title and description. An incident with no monitor needs `status_page_ids` naming the pages to show it on; the list replaces its pages, so start from `get_incident`'s `status_page_ids`. Status-page subscribers may be notified. Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. Not read-only; idempotent.",
        title = "Publish incident",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn publish_incident(
        &self,
        Parameters(args): Parameters<PublishIncidentArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentVisibilityResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id });
        let result = self.publish_incident_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "publish_incident", args_json, result)
            .await
    }

    #[tool(
        description = "Hide a published incident from the public status pages again. Its operator timeline is untouched. Asks for confirmation where the client can show a prompt; otherwise runs on the token's scope. Not read-only; idempotent.",
        title = "Unpublish incident",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn unpublish_incident(
        &self,
        Parameters(args): Parameters<IncidentIdArg>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentVisibilityResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "id": args.id });
        let result = self.unpublish_incident_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "unpublish_incident", args_json, result)
            .await
    }

    #[tool(
        description = "Create several monitors at once, with one confirmation covering the batch where the client can show a prompt. Every check is run once first and all the results are shown together, so a misconfigured endpoint is visible before anything is saved. An item that fails validation or its trial run is reported in the results and the rest are still created. Prefer this over repeated create_monitor calls whenever the user names more than one thing to watch: it costs them one prompt instead of many. Same per-monitor fields and same limits as create_monitor. Not read-only.",
        title = "Create monitors",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_monitors(
        &self,
        Parameters(args): Parameters<CreateMonitorsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorsCreated>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let result = self.create_monitors_inner(&ctx, &auth, &args).await;
        // Target creation writes no org_audit_log row, so this is the only
        // record of what a batch brought into existence.
        let args_json = match &result {
            Ok(Json(batch)) => json!({
                "requested": args.monitors.len(),
                "created": batch.created,
                "monitors": batch.results,
            }),
            Err(_) => json!({ "requested": args.monitors.len() }),
        };
        self.finish(pool, &auth, "create_monitors", args_json, result)
            .await
    }

    #[tool(
        description = "Create a status page. It is created unpublished unless you pass enabled, so its components can be curated before anyone can read it. The slug is the page's public address: it is first-come across the platform and moving it later breaks every existing link, so confirm it with the user rather than inventing one. Add monitors to it with add_status_page_components. Not read-only.",
        title = "Create status page",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn create_status_page(
        &self,
        Parameters(args): Parameters<CreateStatusPageArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<StatusPageWritten>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "slug": args.slug, "name": args.name });
        let result = self.create_status_page_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "create_status_page", args_json, result)
            .await
    }

    #[tool(
        description = "Rename a status page, move it to a new slug, or publish and unpublish it. An omitted field is left alone. Changing the slug moves the public URL and breaks existing links. Not read-only; idempotent.",
        title = "Update status page",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
    )]
    async fn update_status_page(
        &self,
        Parameters(args): Parameters<UpdateStatusPageArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<StatusPageWritten>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "slug": args.slug });
        let result = self.update_status_page_inner(&ctx, &auth, &args).await;
        self.finish(pool, &auth, "update_status_page", args_json, result)
            .await
    }

    #[tool(
        description = "Add monitors to a status page as public components, in one confirmation where the client can show a prompt. Give each a public_name the page's readers will understand, since the monitor's own name is operator-facing, and a public_group to file related components together. Monitors already on the page are reported as such rather than duplicated. detail_link_enabled publishes a per-monitor detail view that shows the monitor's real name and address, not public_name. Not read-only.",
        title = "Add status page components",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn add_status_page_components(
        &self,
        Parameters(args): Parameters<AddComponentsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<ComponentsAdded>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "slug": args.slug, "count": args.components.len() });
        let result = self
            .add_status_page_components_inner(&ctx, &auth, &args)
            .await;
        self.finish(pool, &auth, "add_status_page_components", args_json, result)
            .await
    }

    #[tool(
        description = "Change how one monitor is presented on a status page: its public name, description, group or position. An omitted field is left alone. Not read-only; idempotent.",
        title = "Update status page component",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn update_status_page_component(
        &self,
        Parameters(args): Parameters<UpdateComponentArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<ComponentUpdated>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        let pool = self.require_pool()?;
        let args_json = json!({ "slug": args.slug, "monitor_id": args.monitor_id });
        let result = self
            .update_status_page_component_inner(&ctx, &auth, &args)
            .await;
        self.finish(
            pool,
            &auth,
            "update_status_page_component",
            args_json,
            result,
        )
        .await
    }
}
