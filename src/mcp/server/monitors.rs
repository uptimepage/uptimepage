//! Monitor write bodies: run a check now, pause/resume, create, retune.
//!
//! No audit here — the wrapper in [`super::tools_write`] records the outcome.

use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::service::RequestContext;

use crate::auth::scope::Scope;
use crate::domain::notification_channel::NotificationChannel;
use crate::domain::target::{NewTarget, Target, TargetUpdate};
use crate::domain::{CheckSpec, TargetAlerts, WriteSource};
use crate::quotas::ratelimit::RateLimitCategory;
use crate::target_ops::vet_note;
use crate::targets::validate::{
    validate_alert_confirmations, validate_group_name, validate_recovery_period,
    validate_region_policy, validate_renotify_interval,
};
use crate::web::views::describe_check;

use crate::mcp::auth::McpAuth;
use crate::mcp::confirm::require_confirmation;
use crate::mcp::error::{McpToolError, codes, config_error, probe_dispatch_error};
use crate::mcp::schema::{
    CheckRunResult, CreateMonitorArgs, CreateMonitorsArgs, ManualStateSet, MonitorCreateOutcome,
    MonitorCreated, MonitorIdArg, MonitorStateResult, MonitorUpdateResult, MonitorsCreated,
    NewCheck, ProbeOutcome, SetMonitorStateArgs, UpdateMonitorArgs,
};

use super::McpServer;
use super::args::{
    build_monitor_patch, default_interval_secs, fits_i32, new_check_spec, parse_region_policy,
    parse_uuid, resolve_bindings,
};
use super::text::{
    change_lines, create_prompt_lines, present_error, sanitize_data, sanitize_prompt,
};
use super::view::{channel_names, check_diagnostic, check_timing};

/// Past this a batch costs more probes than the per-minute budget allows, so it
/// would spend them all and still fail whole.
const MAX_BATCH: usize = 20;

/// A validated, probed create waiting on the user's approval.
struct PreparedCreate {
    new: NewTarget,
    plan: std::sync::Arc<crate::domain::Plan>,
    regions: Vec<String>,
    address: String,
    probe: Option<(String, ProbeOutcome)>,
    channel_summary: Option<String>,
    /// The monitor carries tags, so a channel tag rule may still cover it.
    tagged: bool,
    reads_channels: bool,
}

impl PreparedCreate {
    fn prompt(&self) -> String {
        format!(
            "Create monitor \"{}\"?\n\n{}\n{}",
            sanitize_prompt(&self.new.name),
            sanitize_prompt(&self.address),
            create_prompt_lines(
                &self.new,
                &self.regions,
                self.probe.as_ref().map(|(r, p)| (r.as_str(), p)),
                self.channel_summary.as_deref(),
            )
            .join("\n"),
        )
    }

    /// One line per monitor: a full settings block each would run to hundreds.
    fn summary_line(&self) -> String {
        let outcome = match &self.probe {
            Some((_, p)) => match p.http_status {
                Some(code) => format!("{} ({code}, {}ms)", p.state, p.duration_ms),
                None => format!("{} ({}ms)", p.state, p.duration_ms),
            },
            None => "not probed".to_string(),
        };
        format!(
            "{} — {} — {outcome}",
            sanitize_prompt(&self.new.name),
            sanitize_prompt(&self.address),
        )
    }
}

impl McpServer {
    /// `run_check_now` body (no audit — the wrapper's `finish` records it).
    pub(super) async fn run_check_now_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &MonitorIdArg,
    ) -> Result<Json<CheckRunResult>, McpToolError> {
        auth.require(Scope::TargetsExecute)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::CheckNow)
            .await?;
        let id = parse_uuid(&args.id, "monitor id")?;
        let target = self.load_target(auth.org, id).await?;
        require_confirmation(
            ctx,
            auth,
            format!(
                "Run a check now on monitor \"{}\"? It probes the target immediately and \
                 records the result; a failure may trigger your alerts.",
                sanitize_prompt(&target.name)
            ),
        )
        .await?;

        // Same region-aware agent dispatch as REST check-now; the agent runs
        // the probe and persists the result.
        let result = self
            .state
            .target_ops()
            .check_now_via_dispatch(auth.org, &target)
            .await
            .map_err(probe_dispatch_error)?;

        Ok(Json(CheckRunResult {
            id: target.id.to_string(),
            state: result.status.as_str().to_string(),
            checked_at: result.timestamp.to_rfc3339(),
            duration_ms: result.duration_ms,
            http_status: result.response_code,
            timing: check_timing(&result),
            response_size: result.response_size,
            error: result.error.as_deref().map(present_error),
            diagnostic: check_diagnostic(&result),
        }))
    }

    /// Shared pause/resume body. The wrapper's `finish` records the tool call;
    /// the store writes the org's own `target.paused`/`target.resumed` row, so
    /// the trail reads the same whichever surface stopped the monitor.
    pub(super) async fn set_enabled_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &MonitorIdArg,
        enabled: bool,
    ) -> Result<Json<MonitorStateResult>, McpToolError> {
        auth.require(Scope::TargetsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "monitor id")?;
        let target = self.load_writable_target(auth.org, id).await?;

        let (verb, effect) = if enabled {
            ("Resume", "Its checks will restart.")
        } else {
            ("Pause", "Its checks will stop until you resume it.")
        };
        require_confirmation(
            ctx,
            auth,
            format!(
                "{verb} monitor \"{}\"? {effect}",
                sanitize_prompt(&target.name)
            ),
        )
        .await?;

        let updated = self
            .state
            .target_store
            .update(
                auth.org,
                id,
                TargetUpdate {
                    enabled: Some(enabled),
                    ..Default::default()
                },
                None,
                Some(auth.user_id),
            )
            .await
            .map_err(|e| McpToolError::internal(format!("set enabled: {e}")))?
            .ok_or_else(|| McpToolError::not_found("monitor not found"))?;

        Ok(Json(MonitorStateResult {
            id: id.to_string(),
            enabled: updated.enabled,
        }))
    }

    /// `set_monitor_state` body (no audit — the wrapper's `finish` records it).
    /// Allowed on a Terraform-managed monitor: the state is not config.
    pub(super) async fn set_manual_state_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &SetMonitorStateArgs,
    ) -> Result<Json<ManualStateSet>, McpToolError> {
        auth.require(Scope::TargetsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "monitor id")?;
        let target = self.load_target(auth.org, id).await?;
        if !matches!(target.check, CheckSpec::Manual(_)) {
            return Err(McpToolError::invalid_argument(
                "only a manual monitor's state can be set; this one is checked automatically",
            ));
        }
        let status = crate::domain::ManualStatus::from(args.state);
        let note = vet_note(args.note.as_deref()).map_err(config_error)?;
        let ops = self.state.manual_ops();
        let current = ops
            .current(auth.org, &target)
            .await
            .map_err(|e| McpToolError::internal(format!("manual state: {e}")))?;
        // Matches what is there: nothing to write, so nothing to ask.
        if current.matches(status, &note) {
            return Ok(Json(manual_state_set(id, current, false)));
        }
        let effect = manual_set_effect(&target, current.status, status);
        let shown_note = note
            .as_deref()
            .map(|n| format!(" Note: \"{}\".", sanitize_prompt(n)))
            .unwrap_or_default();
        require_confirmation(
            ctx,
            auth,
            format!(
                "Mark monitor \"{}\" {} (now {})?{shown_note} {effect}",
                sanitize_prompt(&target.name),
                status.as_str(),
                current.status.as_str()
            ),
        )
        .await?;

        // The prompt promised an effect read from this state; if someone moved
        // it meanwhile, that promise no longer holds.
        let target_now = self.load_target(auth.org, id).await?;
        let state_now = ops
            .current(auth.org, &target_now)
            .await
            .map_err(|e| McpToolError::internal(format!("manual state: {e}")))?;
        // Every set moves `set_at`; who made the last one is not the state.
        if state_now.set_at != current.set_at
            || manual_set_effect(&target_now, current.status, status) != effect
        {
            return Err(McpToolError::new(
                codes::CONFLICT,
                "monitor changed while the change was being confirmed; read it again and retry",
                true,
            ));
        }
        let change = ops
            .set_on(auth.org, &target_now, status, note, Some(auth.user_id))
            .await
            .map_err(config_error)?;
        let changed = change.changed();
        Ok(Json(manual_state_set(id, change.state, changed)))
    }

    /// `create_monitor` body (no audit — the wrapper's `finish` records it).
    pub(super) async fn create_monitor_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &CreateMonitorArgs,
    ) -> Result<Json<MonitorCreated>, McpToolError> {
        auth.require(Scope::TargetsWrite)?;
        // The trial run is a real probe against a caller-supplied address, so
        // this needs the scope that dispatching a probe needs, and it is metered
        // against the same probe budget the REST dry run spends.
        if probes(&args.check) {
            auth.require(Scope::TargetsExecute)?;
        }

        let prepared = self.prepare_create(auth, args).await?;
        require_confirmation(ctx, auth, prepared.prompt()).await?;
        self.persist_create(auth, prepared).await
    }

    /// Everything up to asking the user, so a batch can spend one confirmation
    /// on many monitors instead of one each.
    async fn prepare_create(
        &self,
        auth: &McpAuth,
        args: &CreateMonitorArgs,
    ) -> Result<PreparedCreate, McpToolError> {
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        if probes(&args.check) {
            self.enforce_rate_limit(auth.org, RateLimitCategory::TestNow)
                .await?;
        }

        let name = args.name.trim();
        if name.is_empty() {
            return Err(McpToolError::invalid_argument("name must not be blank"));
        }
        let check = new_check_spec(&args.check)?;
        let plan = self
            .state
            .quotas
            .limit_for_org(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("plan: {e}")))?;

        let plan_floor = u64::try_from(plan.min_check_interval_secs).unwrap_or(60);
        // No interval can satisfy both bounds, so say which two disagree rather
        // than refuse an interval the caller never chose.
        if let Some(hb) = check.as_heartbeat() {
            let window = hb.period.as_secs().saturating_add(hb.grace.as_secs());
            if window < plan_floor {
                return Err(McpToolError::invalid_argument(format!(
                    "this plan checks no more often than every {plan_floor}s, so it cannot judge a \
                     heartbeat whose period and grace add up to {window}s; raise period_secs or \
                     grace_secs"
                )));
            }
        }
        let interval_secs = args
            .interval_secs
            .unwrap_or_else(|| default_interval_secs(&check, plan_floor));
        fits_i32(interval_secs, "interval_secs")?;
        if let Some(n) = args.alert_confirmations {
            fits_i32(u64::from(n), "alert_confirmations")?;
        }
        if let Some(n) = args.renotify_interval_secs {
            fits_i32(u64::from(n), "renotify_interval_secs")?;
        }
        if let Some(n) = args.recovery_period_secs {
            fits_i32(u64::from(n), "recovery_period_secs")?;
        }

        // An empty list binds nothing, exactly like omitting the field, so it
        // does not demand the scope or spend the query.
        let wants_channels = args
            .channel_ids
            .as_deref()
            .is_some_and(|ids| !ids.is_empty());
        let tags = args.tags.as_deref().unwrap_or_default();
        // A tag rule covers a monitor nothing is bound to. Best-effort for
        // tags: the caller did not name those channels, so reporting them
        // must not demand a scope.
        let reads_channels = auth.scopes.allows(Scope::ChannelsRead);
        let channels = self
            .channels_for_binding(auth, wants_channels || (!tags.is_empty() && reads_channels))
            .await?;
        let alerts = match args.channel_ids.as_deref() {
            Some(ids) if wants_channels => resolve_bindings(ids, &channels)?,
            _ => TargetAlerts::default(),
        };
        let channel_summary = channel_names(
            &alerts,
            tags,
            &channels,
            self.state.cfg.escalation.channel_failure_limit,
        );

        let mut new = NewTarget {
            name: name.to_string(),
            check,
            interval: std::time::Duration::from_secs(interval_secs),
            enabled: true,
            tags: args.tags.clone().unwrap_or_default(),
            alerts,
            region_policy: args
                .region_policy
                .as_ref()
                .map(parse_region_policy)
                .transpose()?,
            alert_confirmations: args.alert_confirmations.unwrap_or(2),
            notify_recovery: args.notify_recovery.unwrap_or(true),
            renotify_interval_secs: args.renotify_interval_secs.unwrap_or(3600),
            recovery_period_secs: args.recovery_period_secs.unwrap_or(0),
            group_name: args.group_name.as_deref().map(str::trim).and_then(|g| {
                if g.is_empty() {
                    None
                } else {
                    Some(g.to_string())
                }
            }),
            owner_user_id: None,
            regions: args.regions.clone(),
        };
        new.default_owner(auth.user_id);
        let ops = self.state.target_ops();
        ops.vet_new_target(auth.org, &mut new, &plan)
            .await
            .map_err(config_error)?;

        // With the other argument checks: a set the fleet cannot serve is a
        // mistake worth answering before a probe is spent on it.
        let snapshot = ops.region_snapshot().await.map_err(config_error)?;
        let regions = ops
            .resolve_create_regions(auth.org, &new, &plan, &snapshot)
            .await
            .map_err(config_error)?;

        let address = describe_check(&new.check).1;
        let probe = if new.check.is_passive() {
            None
        } else {
            Some(self.trial_run(auth.org, &new.check, &regions).await?)
        };

        Ok(PreparedCreate {
            new,
            plan,
            regions,
            address,
            probe,
            channel_summary,
            tagged: !tags.is_empty(),
            reads_channels,
        })
    }

    /// Runs only once the user has approved the prepared create.
    async fn persist_create(
        &self,
        auth: &McpAuth,
        prepared: PreparedCreate,
    ) -> Result<Json<MonitorCreated>, McpToolError> {
        let PreparedCreate {
            new,
            plan,
            regions,
            address,
            probe,
            channel_summary,
            tagged,
            reads_channels,
        } = prepared;

        let created = self
            .state
            .target_ops()
            .create_target(auth.org, new, WriteSource::Api, &plan, regions.clone())
            .await
            .map_err(config_error)?;

        Ok(Json(MonitorCreated {
            id: created.id.to_string(),
            name: sanitize_data(&created.name),
            address: sanitize_data(&address),
            interval_secs: created.interval.as_secs(),
            regions,
            probe: probe.map(|(_, p)| p),
            alerts: match &channel_summary {
                Some(s) => s.clone(),
                // Unread inventory would call a tag-covered monitor unmonitored.
                None if tagged && !reads_channels => {
                    "unknown: a channel tag rule may cover it".to_string()
                }
                None => "nobody".to_string(),
            },
        }))
    }

    /// `create_monitors` body: many monitors, one confirmation. Every item is
    /// probed first, and one that cannot be prepared is reported without
    /// discarding the rest.
    pub(super) async fn create_monitors_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &CreateMonitorsArgs,
    ) -> Result<Json<MonitorsCreated>, McpToolError> {
        auth.require(Scope::TargetsWrite)?;
        if args.monitors.iter().any(|m| probes(&m.check)) {
            auth.require(Scope::TargetsExecute)?;
        }

        if args.monitors.is_empty() {
            return Err(McpToolError::invalid_argument(
                "pass at least one monitor to create",
            ));
        }
        // Each item spends a real probe before anything is confirmed, so an
        // oversized batch would burn the probe budget and then fail whole.
        if args.monitors.len() > MAX_BATCH {
            return Err(McpToolError::invalid_argument(format!(
                "at most {MAX_BATCH} monitors per call; split the rest into another call"
            )));
        }

        let mut slots: Vec<Result<PreparedCreate, MonitorCreateOutcome>> =
            Vec::with_capacity(args.monitors.len());
        for item in &args.monitors {
            match self.prepare_create(auth, item).await {
                Ok(p) => slots.push(Ok(p)),
                Err(e) if e.is_fatal_to_batch() => return Err(e),
                Err(e) => slots.push(Err(MonitorCreateOutcome {
                    name: sanitize_data(item.name.trim()),
                    id: None,
                    address: None,
                    probe: None,
                    error: Some(e.message.clone()),
                })),
            }
        }
        let prepared: Vec<&PreparedCreate> = slots.iter().filter_map(|s| s.as_ref().ok()).collect();
        if prepared.is_empty() {
            return Err(McpToolError::invalid_argument(format!(
                "none of the {} monitors could be prepared: {}",
                slots.len(),
                slots
                    .iter()
                    .filter_map(|s| s.as_ref().err())
                    .filter_map(|o| o.error.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }

        let lines: Vec<String> = prepared
            .iter()
            .map(|p| PreparedCreate::summary_line(p))
            .collect();
        let count = prepared.len();
        require_confirmation(
            ctx,
            auth,
            format!("Create {count} monitors?\n\n{}", lines.join("\n")),
        )
        .await?;

        let mut results = Vec::with_capacity(slots.len());
        let mut created = 0usize;
        for slot in slots {
            let p = match slot {
                Ok(p) => p,
                Err(outcome) => {
                    results.push(outcome);
                    continue;
                }
            };
            let name = sanitize_data(&p.new.name);
            let address = sanitize_data(&p.address);
            match self.persist_create(auth, p).await {
                Ok(Json(m)) => {
                    created += 1;
                    results.push(MonitorCreateOutcome {
                        name: m.name,
                        id: Some(m.id),
                        address: Some(m.address),
                        probe: m.probe,
                        error: None,
                    });
                }
                Err(e) => results.push(MonitorCreateOutcome {
                    name,
                    id: None,
                    address: Some(address),
                    probe: None,
                    error: Some(e.message.clone()),
                }),
            }
        }

        if created == 0 {
            // Nothing exists that did not before, so the audit row must not say
            // success; the per-item reasons ride along in the message.
            return Err(McpToolError::invalid_argument(format!(
                "none of the {} monitors could be created: {}",
                results.len(),
                results
                    .iter()
                    .filter_map(|o| o.error.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }

        Ok(Json(MonitorsCreated { created, results }))
    }

    /// The org's channel inventory, read once so every diff and prompt names
    /// the same rows. Empty when the caller is not touching bindings.
    async fn channels_for_binding(
        &self,
        auth: &McpAuth,
        binding: bool,
    ) -> Result<Vec<NotificationChannel>, McpToolError> {
        if !binding {
            return Ok(Vec::new());
        }
        // Naming and validating a channel is reading the inventory, so the
        // caller needs the scope that reading it needs. Checked before any
        // budget is spent on a call that cannot succeed without it.
        auth.require(Scope::ChannelsRead)?;
        self.state
            .notification_channel_store
            .list(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("list channels: {e}")))
    }

    /// Run the check once, unsaved, so the confirmation can show what it does.
    /// A trial answered by a region the monitor was never assigned describes a
    /// different network path than the one being approved.
    async fn trial_run(
        &self,
        org: crate::domain::OrgId,
        check: &CheckSpec,
        regions: &[String],
    ) -> Result<(String, ProbeOutcome), McpToolError> {
        let default = self.state.cfg.scheduler.effective_default_region();
        // A region with no agent 503s the dispatch and takes the create with it,
        // so availability outranks the preference for home.
        let region = regions
            .iter()
            .min_by_key(|r| (self.state.ad_hoc.region_state(r), r.as_str() != default))
            .map_or_else(|| default.to_string(), String::clone);
        let delivered = self
            .state
            .target_ops()
            .run_ad_hoc(
                org,
                &region,
                crate::domain::agent_wire::DispatchKind::Test,
                None,
                check.clone(),
            )
            .await
            .map_err(probe_dispatch_error)?;
        let r = delivered.result;
        Ok((
            region,
            ProbeOutcome {
                state: r.status.as_str().to_string(),
                duration_ms: r.duration_ms,
                http_status: r.response_code,
                error: r.error.as_deref().map(present_error),
                diagnostic: check_diagnostic(&r),
            },
        ))
    }

    /// `update_monitor` body (no audit — the wrapper's `finish` records it).
    pub(super) async fn update_monitor_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &UpdateMonitorArgs,
    ) -> Result<Json<MonitorUpdateResult>, McpToolError> {
        auth.require(Scope::TargetsWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;
        let id = parse_uuid(&args.id, "monitor id")?;
        let target = self.load_writable_target(auth.org, id).await?;

        // Read once and handed to every diff: the patch is rebuilt after the
        // confirmation and the two must agree field for field, so channels
        // cannot be diffed outside it.
        // Tags alone can move who pages this monitor, so they need the
        // inventory as much as a binding change does. Best-effort there: a
        // retag must not start demanding a scope it never needed.
        let retags = args.tags.is_some() && auth.scopes.allows(Scope::ChannelsRead);
        let channels = self
            .channels_for_binding(auth, args.channel_ids.is_some() || retags)
            .await?;

        let (mut update, changes) = build_monitor_patch(
            args,
            &target,
            &channels,
            self.state.cfg.escalation.channel_failure_limit,
        )?;
        if changes.is_empty() {
            return Ok(Json(MonitorUpdateResult {
                id: id.to_string(),
                changes,
            }));
        }

        validate_alert_confirmations(update.alert_confirmations).map_err(config_error)?;
        validate_renotify_interval(update.renotify_interval_secs).map_err(config_error)?;
        validate_recovery_period(update.recovery_period_secs).map_err(config_error)?;
        if let Some(Some(group)) = update.group_name.as_ref() {
            validate_group_name(Some(group.as_str())).map_err(config_error)?;
        }
        if update.region_policy.is_some() {
            let available = self
                .state
                .target_store
                .available_regions()
                .await
                .map_err(|e| McpToolError::internal(format!("region catalog: {e}")))?;
            validate_region_policy(update.region_policy, available.len()).map_err(config_error)?;
        }
        self.state
            .target_ops()
            .validate_patch_schedule(auth.org, id, &mut update, Some(&target))
            .await
            .map_err(config_error)?;

        require_confirmation(
            ctx,
            auth,
            format!(
                "Change monitor \"{}\"?\n\n{}",
                sanitize_prompt(&target.name),
                change_lines(&changes)
            ),
        )
        .await?;

        // The monitor can move while a human reads the prompt, and the approval
        // describes the diff as it stood then.
        let current = self.load_writable_target(auth.org, id).await?;
        let (update, still) = build_monitor_patch(
            args,
            &current,
            &channels,
            self.state.cfg.escalation.channel_failure_limit,
        )?;
        if still != changes {
            return Err(McpToolError::new(
                codes::CONFLICT,
                "monitor changed while the change was being confirmed; read it again and retry",
                true,
            ));
        }

        // `None`: not restamping `write_source` is what keeps a terraform marker.
        self.state
            .target_store
            .update(auth.org, id, update, None, Some(auth.user_id))
            .await
            .map_err(|e| McpToolError::internal(format!("update monitor: {e}")))?
            .ok_or_else(|| McpToolError::not_found("monitor not found"))?;

        Ok(Json(MonitorUpdateResult {
            id: id.to_string(),
            changes,
        }))
    }
}

fn manual_state_set(
    id: uuid::Uuid,
    state: crate::domain::ManualState,
    changed: bool,
) -> ManualStateSet {
    ManualStateSet {
        id: id.to_string(),
        state: state.status.as_str().to_string(),
        note: state.note,
        set_at: state.set_at.to_rfc3339(),
        changed,
    }
}

/// What a set will do, said before it is approved. Incidents only ever get
/// worse while open, so a softer bad state changes the cause, not the outage.
pub(super) fn manual_set_effect(
    target: &Target,
    from: crate::domain::ManualStatus,
    to: crate::domain::ManualStatus,
) -> &'static str {
    use crate::domain::ManualStatus::{Degraded, Down, Up};
    let held = target.plan_hold_at.is_some();
    if held && !target.enabled {
        return "The plan no longer covers this monitor and it is paused: the state is kept and takes effect once the plan covers it and it is enabled.";
    }
    if held {
        return "The plan no longer covers this monitor: the state is kept and takes effect once it does.";
    }
    if !target.enabled {
        return "The monitor is paused: the state is kept and takes effect once it is enabled.";
    }
    match (from, to) {
        (_, Up) => "Its open incident, if any, closes.",
        (Up, _) => "It opens an incident, which may alert its channels.",
        (Degraded, Down) => "Its open incident becomes an outage, with the note as its cause.",
        (Down, Degraded) => {
            "Its open incident stays an outage until it is set up; the note becomes its cause."
        }
        _ => "The note becomes the cause of its open incident.",
    }
}

/// Whether creating this check spends a trial probe. A passive kind has
/// nothing to reach, so it needs neither the scope nor the budget.
fn probes(check: &NewCheck) -> bool {
    !matches!(check, NewCheck::Heartbeat { .. } | NewCheck::Manual {})
}
