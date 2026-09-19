//! The read tools: side-effect-free views over one org's monitors, incidents,
//! status pages and usage.
//!
//! Every tool here is annotated `readOnlyHint`, takes the org from the
//! credential, and returns typed `structuredContent`. Customer free text
//! (monitor/group names, errors) is returned as labelled data — never as
//! instructions to the model.

use std::collections::HashMap;

use chrono::{Duration, Utc};
use futures::future::join_all;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, tool, tool_router};
use uuid::Uuid;

use crate::auth::scope::Scope;
use crate::domain::WriteSource;
use crate::domain::metrics::DashboardMetrics;
use crate::domain::target::Target;
use crate::domain::{confirmed_downtime_secs, uptime_pct_from_downtime};
use crate::storage::incidents::IncidentBriefFilter;
use crate::storage::{TargetFilter, TimeRange};
use crate::web::views::describe_check;

use crate::mcp::auth::McpAuth;
use crate::mcp::cursor;
use crate::mcp::error::McpToolError;
use crate::mcp::schema::{
    ChannelItem, ChannelList, Failure, FlowRunList, FlowStepTrendSummary, FlowWindowArgs,
    GetIncidentMetricsArgs, GetMonitorArgs, GetMonitorHistoryArgs, GetStatusPageArgs, HealthTotals,
    IncidentDetail, IncidentIdArg, IncidentList, IncidentMetricsResult, IncidentWindow,
    LatencyPoint, ListIncidentsArgs, ListMonitorsArgs, ListStatusPagesArgs, MetricCount,
    MonitorDetail, MonitorHistory, MonitorList, MonitorListItem, NoisyMonitor, OrgHealth, OrgUsage,
    Quota, RegionList, StatusPageComponent as McpComponent, StatusPageDetail, StatusPageList,
    StatusPageSummary, TagItem, TagList, VariableList, VariableSummary, WorstMonitor,
};

use super::McpServer;
use super::args::{
    incident_window, parse_incident_state_filter, parse_kind, parse_state, parse_uuid,
    parse_window, requested_region,
};
use super::text::{present_error, sanitize_data};
use super::view::{
    check_config, check_timing, current_state, flow_run_item, incident_detail, incident_summary,
    ms_to_rfc3339, region_cap, region_health, region_items, region_policy_view, step_trend_item,
    ts_to_rfc3339,
};

/// Health/list window. Matches the operator dashboard's default so the MCP
/// answer and the UI agree on "right now".
const HEALTH_WINDOW_HOURS: i64 = 24;
/// Cap on `get_org_health.worst` — triage wants the headline failures, not a
/// dump. The model can page the full set via `list_monitors(state=...)`.
const WORST_CAP: usize = 8;
/// Page size for `list_monitors`.
const PAGE_SIZE: usize = 50;
/// Upper bound on rows pulled for an in-memory paginated list (monitors,
/// incidents). Comfortably above any plan's cap; pagination then slices the
/// fetched set exactly.
const MAX_LIST_FETCH: usize = 1000;
/// Cap on incidents/failures returned by `get_monitor_history` — bound the
/// response regardless of how flappy the monitor is.
const HISTORY_INCIDENT_CAP: usize = 50;
/// Confirmed incidents read to derive uptime over a window; far above any
/// realistic confirmed-incident count, so it never truncates the downtime sum.
const UPTIME_INCIDENT_CAP: usize = 2_000;
/// Per branch of the run read, which merges newest-N with newest-N failures, so
/// the answer holds up to twice this. Each run carries its whole step trace, so
/// a deeper page costs the model more context than it buys.
const FLOW_RUN_CAP: usize = 25;
/// Tags returned by `list_tags`. Far above a usable tag vocabulary, and the
/// response flags the truncation rather than passing a partial list off as whole.
const TAG_CAP: usize = 200;

#[tool_router(router = read_router, vis = "pub(super)")]
impl McpServer {
    /// Triage one-shot: "what's broken in my org right now?". Returns per-state
    /// totals plus the worst currently-failing monitors (newest failure first),
    /// in a single small call — cheaper for the model than stitching list calls.
    /// Use this first when asked about overall health or outages.
    #[tool(
        description = "Org health summary: per-state monitor totals and the worst currently-failing monitors. The one-shot answer to 'what is broken right now?'. Read-only.",
        title = "Org health",
        annotations(read_only_hint = true)
    )]
    async fn get_org_health(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<OrgHealth>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let org = auth.org;
        let pool = self
            .state
            .db
            .as_ref()
            .ok_or_else(|| McpToolError::internal("db unavailable"))?;

        let now = Utc::now();
        let range = TimeRange {
            from: now - Duration::try_hours(HEALTH_WINDOW_HOURS).unwrap_or_default(),
            to: now,
        };
        // `open` is best-effort (it only adds incident ids), so it rides a plain
        // `join!` alongside the load-bearing queries rather than failing health.
        let (targets, rollup, org_row, open) = tokio::join!(
            self.state.target_store.list(org, all_monitors_filter(None)),
            self.state.results_store.dashboard_rollup(org, range, None),
            crate::storage::orgs::get_org(pool, org),
            self.state.incident_narration_store.list_briefs(
                org,
                IncidentBriefFilter {
                    limit: MAX_LIST_FETCH,
                    ..Default::default()
                },
            ),
        );
        let to_err = |e| McpToolError::internal(format!("org health query: {e}"));
        let targets = targets.map_err(to_err)?;
        let rollup = rollup.map_err(to_err)?;
        let org_row = org_row.map_err(to_err)?;

        let metrics = index_by_target(rollup);
        let folded = self
            .state
            .folded_status(org, range, crate::app::folded_status_policies(&targets))
            .await;
        let org_slug = org_row.map(|o| o.slug).unwrap_or_else(|| org.0.to_string());

        let mut totals = HealthTotals {
            up: 0,
            down: 0,
            degraded: 0,
            error: 0,
            no_data: 0,
        };
        // (target, state) for every enabled, non-up monitor — the worst pool.
        let mut failing: Vec<(Target, &'static str, Option<i64>)> = Vec::new();
        for t in targets {
            if !t.enabled {
                continue;
            }
            let m = metrics.get(&t.id);
            let state = current_state(m, folded.get(&t.id).copied());
            match state {
                "up" => totals.up += 1,
                "down" => totals.down += 1,
                "degraded" => totals.degraded += 1,
                "error" => totals.error += 1,
                _ => totals.no_data += 1,
            }
            if matches!(state, "down" | "error" | "degraded") {
                let last_ts = m.and_then(|m| m.last_minute_ts);
                failing.push((t, state, last_ts));
            }
        }

        // Newest failure first; monitors with no recent sample (None) sort last.
        failing.sort_by_key(|f| std::cmp::Reverse(f.2));
        failing.truncate(WORST_CAP);

        // A stable, acknowledgeable `incident_id` exists only for monitors that
        // are components on a status page (the incident writer only materialises
        // those). Map target → its open public incident; a lookup failure yields
        // no ids rather than failing health.
        // Collecting by target is lossless because a unique partial index
        // allows one open incident per target; order here decides nothing.
        let open_by_target: HashMap<Uuid, (String, String)> = open
            .unwrap_or_default()
            .into_iter()
            .map(|i| (i.target_id, (i.id.to_string(), i.started_at.to_rfc3339())))
            .collect();

        // `since` must cover every failing monitor, public or not, so it falls
        // back to the coalesced check history when no public incident is open.
        // The ≤WORST_CAP fallbacks run concurrently.
        let worst = join_all(failing.into_iter().map(|(t, state, _)| {
            let open_by_target = &open_by_target;
            async move {
                let inc = open_by_target.get(&t.id);
                let since = match inc {
                    Some((_, s)) => Some(s.clone()),
                    None => self.ongoing_since(org, &t).await,
                };
                WorstMonitor {
                    id: t.id.to_string(),
                    name: sanitize_data(&t.name),
                    r#type: t.check.kind().to_string(),
                    state: state.to_string(),
                    since,
                    incident_id: inc.map(|(id, _)| id.clone()),
                }
            }
        }))
        .await;

        Ok(Json(OrgHealth {
            org: org_slug,
            totals,
            worst,
        }))
    }

    /// List monitors with optional filters and cursor pagination. For "show me
    /// all my DNS monitors", "everything that's degraded", etc. Prefer
    /// `get_org_health` for a quick "what's broken" overview.
    #[tool(
        description = "List monitors with optional state/type/tag filters and cursor pagination. Each item carries its current state and last-checked time. Read-only.",
        title = "List monitors",
        annotations(read_only_hint = true)
    )]
    async fn list_monitors(
        &self,
        Parameters(args): Parameters<ListMonitorsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let org = auth.org;

        let state_filter = args.state.as_deref().map(parse_state).transpose()?;
        let type_filter = args.r#type.as_deref().map(parse_kind).transpose()?;
        let offset = match args.cursor.as_deref() {
            Some(c) => cursor::decode_offset(c)
                .ok_or_else(|| McpToolError::invalid_argument("invalid cursor"))?,
            None => 0,
        };

        let now = Utc::now();
        let range = TimeRange {
            from: now - Duration::try_hours(HEALTH_WINDOW_HOURS).unwrap_or_default(),
            to: now,
        };
        let (targets, rollup) = tokio::try_join!(
            self.state
                .target_store
                .list(org, all_monitors_filter(args.tag.clone())),
            self.state.results_store.dashboard_rollup(org, range, None),
        )
        .map_err(|e| McpToolError::internal(format!("list monitors query: {e}")))?;

        let metrics = index_by_target(rollup);
        let folded = self
            .state
            .folded_status(org, range, crate::app::folded_status_policies(&targets))
            .await;

        // Build, filter (type + state) in memory, then sort for stable paging.
        let mut items: Vec<MonitorListItem> = targets
            .into_iter()
            .filter(|t| type_filter.is_none_or(|k| t.check.kind() == k))
            .map(|t| {
                let m = metrics.get(&t.id);
                MonitorListItem {
                    id: t.id.to_string(),
                    name: sanitize_data(&t.name),
                    r#type: t.check.kind().to_string(),
                    state: current_state(m, folded.get(&t.id).copied()).to_string(),
                    group_name: t.group_name.as_deref().map(sanitize_data),
                    interval_secs: t.interval.as_secs(),
                    enabled: t.enabled,
                    last_checked_at: m.and_then(|m| ts_to_rfc3339(m.last_minute_ts)),
                }
            })
            .filter(|i| state_filter.is_none_or(|s| i.state == s))
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));

        let (items, next_cursor) = cursor::paginate(&items, offset, PAGE_SIZE, |i| i.clone());

        Ok(Json(MonitorList { items, next_cursor }))
    }

    /// Full config + current state + recent uptime for one monitor. Use after
    /// `list_monitors`/`get_org_health` to investigate a specific monitor.
    #[tool(
        description = "One monitor's full configuration — everything the check asserts (expected status, body match, headers, timeout, redirect and TLS policy), the regions it probes from, and how it alerts (failing checks before it pages, whether recovery is announced, the reminder interval, the multi-region quorum, and the ids of the channels it notifies) — with its current state, last error, and 24h/30d uptime. Every field update_monitor can change is readable here, in the shape that tool takes. Read this before judging whether a response should have passed, or before changing a monitor. Credentials are withheld. Read-only.",
        title = "Monitor details",
        annotations(read_only_hint = true)
    )]
    async fn get_monitor(
        &self,
        Parameters(args): Parameters<GetMonitorArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorDetail>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let org = auth.org;
        let id = parse_uuid(&args.id, "monitor id")?;

        let target = self
            .state
            .target_store
            .get(org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("get monitor: {e}")))?
            .ok_or_else(|| McpToolError::not_found("monitor not found"))?;

        let now = Utc::now();
        let (r24, r30) = tokio::try_join!(
            self.clamped_raw_window(org, Duration::try_hours(24).unwrap_or_default()),
            self.clamped_raw_window(org, Duration::try_days(30).unwrap_or_default()),
        )?;
        let (latest, mut up24, mut up30, incidents, regions) = tokio::try_join!(
            self.state
                .results_store
                .list_results(org, id, r24, 1, 0, None),
            self.state.results_store.uptime(org, id, r24, None),
            self.state.results_store.uptime(org, id, r30, None),
            self.state.incident_narration_store.list_for_target(
                org,
                id,
                r30.inner(),
                UPTIME_INCIDENT_CAP,
                0,
                false
            ),
            self.state.target_store.regions_for_target(org, id),
        )
        .map_err(|e| McpToolError::internal(format!("monitor history: {e}")))?;

        // Report uptime as confirmed downtime over each window (30d incidents
        // cover the 24h window too).
        if up24.total > 0 {
            let down = confirmed_downtime_secs(&incidents, r24.from, r24.to, now);
            up24.uptime_pct = Some(uptime_pct_from_downtime(
                down,
                (r24.to - r24.from).num_seconds(),
            ));
        }
        if up30.total > 0 {
            let down = confirmed_downtime_secs(&incidents, r30.from, r30.to, now);
            up30.uptime_pct = Some(uptime_pct_from_downtime(
                down,
                (r30.to - r30.from).num_seconds(),
            ));
        }

        // Folded like `list_monitors`, so the drill-down cannot contradict it.
        let folded = self
            .state
            .folded_status(
                org,
                r24.inner(),
                crate::app::folded_status_policies(std::slice::from_ref(&target)),
            )
            .await
            .get(&id)
            .copied();

        let last = latest.first();
        let (_, address) = describe_check(&target.check);
        Ok(Json(MonitorDetail {
            id: target.id.to_string(),
            name: sanitize_data(&target.name),
            r#type: target.check.kind().to_string(),
            address: sanitize_data(&address),
            check: check_config(&target.check),
            // A passive check is seeded a region row it never runs from.
            regions: if target.check.is_passive() {
                Vec::new()
            } else {
                regions.unwrap_or_default()
            },
            enabled: target.enabled,
            interval_secs: target.interval.as_secs(),
            alert_channel_ids: target
                .alerts
                .iter()
                .map(|b| b.channel_id.to_string())
                .collect(),
            alert_confirmations: target.alert_confirmations,
            notify_recovery: target.notify_recovery,
            renotify_interval_secs: target.renotify_interval_secs,
            // Blanked for the same reason `regions` is: a quorum over no probe
            // regions describes nothing.
            region_policy: (!target.check.is_passive())
                .then(|| region_policy_view(target.region_policy)),
            managed_externally: target.write_source == WriteSource::Terraform,
            group_name: target.group_name.as_deref().map(sanitize_data),
            tags: target.tags.iter().map(|t| sanitize_data(t)).collect(),
            state: match (last, folded) {
                (Some(_), Some(f)) => f.as_str(),
                (Some(r), None) => r.status.as_str(),
                (None, _) => "no_data",
            }
            .to_string(),
            last_checked_at: last.map(|r| r.timestamp.to_rfc3339()),
            last_error: last.and_then(|r| r.error.as_deref()).map(present_error),
            last_diagnostic: last.and_then(super::view::check_diagnostic),
            last_http_status: last.and_then(|r| r.response_code),
            last_timing: last.map(check_timing).unwrap_or_default(),
            last_response_size: last.and_then(|r| r.response_size),
            uptime_24h: up24.uptime_pct,
            uptime_30d: up30.uptime_pct,
        }))
    }

    /// Bounded history for one monitor over a window: uptime, a latency series,
    /// failing observations (with error text), and incident windows.
    #[tool(
        description = "One monitor's history over a window (1h/24h/7d/30d): uptime, latency series, a per-region split of the same window, failures with error text, and incident windows. Pass `region` to narrow it to one probe region and tell a partial outage from a total one. Read-only.",
        title = "Monitor history",
        annotations(read_only_hint = true)
    )]
    async fn get_monitor_history(
        &self,
        Parameters(args): Parameters<GetMonitorHistoryArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<MonitorHistory>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let org = auth.org;
        let id = parse_uuid(&args.id, "monitor id")?;
        let (span, bucket_secs) = parse_window(&args.window)?;

        let (target, assigned) = tokio::try_join!(
            self.state.target_store.get(org, id),
            self.state.target_store.regions_for_target(org, id),
        )
        .map_err(|e| McpToolError::internal(format!("get monitor: {e}")))?;
        let target = target.ok_or_else(|| McpToolError::not_found("monitor not found"))?;
        // A passive check is seeded a region row it never runs from.
        let assigned = if target.check.is_passive() {
            Vec::new()
        } else {
            assigned.unwrap_or_default()
        };
        let region = requested_region(args.region.as_deref(), &assigned)?;
        let split_by_region = assigned.len() > 1;

        let now = Utc::now();
        let range = self.clamped_raw_window(org, span).await?;
        let (uptime, buckets, incidents, breakdown) = tokio::try_join!(
            self.state
                .results_store
                .uptime(org, id, range, region.as_deref()),
            self.state.results_store.latency_buckets(
                org,
                id,
                range,
                bucket_secs,
                region.as_deref()
            ),
            self.state.incident_narration_store.list_for_target(
                org,
                id,
                range.inner(),
                UPTIME_INCIDENT_CAP,
                0,
                false,
            ),
            async {
                if split_by_region {
                    self.state
                        .results_store
                        .region_breakdown(org, id, range.inner())
                        .await
                } else {
                    Ok(Vec::new())
                }
            },
        )
        .map_err(|e| McpToolError::internal(format!("monitor history: {e}")))?;

        let latency_series = buckets
            .into_iter()
            .filter_map(|b| {
                ms_to_rfc3339(b.t).map(|at| LatencyPoint {
                    at,
                    p50_ms: b.p50,
                    p95_ms: b.p95,
                    p99_ms: b.p99,
                })
            })
            .collect();

        // Not narrowed by `region`: the schema promises the full split here.
        let regions = breakdown.into_iter().map(region_health).collect();

        // Confirmed downtime measures the monitor, not a region.
        let uptime_pct = match (region.is_none(), uptime.total > 0) {
            (true, true) => {
                let down = confirmed_downtime_secs(&incidents, range.from, range.to, now);
                Some(uptime_pct_from_downtime(
                    down,
                    (range.to - range.from).num_seconds(),
                ))
            }
            (true, false) => None,
            (false, _) => uptime.uptime_pct,
        };

        // The failures/windows lists stay bounded regardless of how flappy the
        // monitor is; the uptime sum above already used the full set.
        let failures = incidents
            .iter()
            .take(HISTORY_INCIDENT_CAP)
            .map(|inc| Failure {
                at: inc.started_at.to_rfc3339(),
                state: inc.status.as_str().to_string(),
                error: inc.error_sample.as_deref().map(present_error),
            })
            .collect();

        let incident_windows = incidents
            .into_iter()
            .take(HISTORY_INCIDENT_CAP)
            .map(|inc| IncidentWindow {
                opened_at: inc.started_at.to_rfc3339(),
                resolved_at: inc.ended_at.map(|e| e.to_rfc3339()),
                counts_as_downtime: inc.counts_as_downtime,
            })
            .collect();

        Ok(Json(MonitorHistory {
            uptime: uptime_pct,
            region,
            latency_series,
            regions,
            failures,
            incidents: incident_windows,
        }))
    }

    /// The probe-region catalog, so the model can name a region and pass a
    /// valid one to `get_monitor_history`.
    #[tool(
        description = "The fleet's probe regions: id, display name, city, country, continent, and whether each is on by default for a new monitor. Reports `max_regions` only when the plan reaches fewer regions than the catalog lists, so a set too large to be accepted is visible before it is sent. Use it to pass a valid `region` to get_monitor_history, and to read where a check would run from. This is a catalog, not a menu to fill: leave `create_monitor.regions` unset unless the user named the places they want covered. Read-only.",
        title = "List probe regions",
        annotations(read_only_hint = true)
    )]
    async fn list_regions(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<RegionList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let regions = self
            .state
            .regions_detailed()
            .await
            .map_err(|e| McpToolError::internal(format!("list regions: {e}")))?;
        let plan = self
            .state
            .quotas
            .limit_for_org(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("plan: {e}")))?;
        let preferred = self
            .state
            .target_store
            .default_selected_regions()
            .await
            .map_err(|e| McpToolError::internal(format!("default regions: {e}")))?;
        // Must resolve exactly as an omitted `create_monitor.regions` does.
        let applied_default = crate::targets::default_region_set(
            preferred,
            plan.max_regions,
            self.state.cfg.scheduler.effective_default_region(),
        );
        let items = region_items(regions, &applied_default);
        Ok(Json(RegionList {
            max_regions: region_cap(plan.max_regions, items.len()),
            items,
        }))
    }

    /// The org's tag inventory — `list_monitors(tag=…)` filters by an exact tag,
    /// and this is the only way to learn which ones exist.
    #[tool(
        description = "Every tag in use across the org's monitors, most-used first, with how many monitors carry each. Pass one back as the `tag` filter to list_monitors. Read-only.",
        title = "List tags",
        annotations(read_only_hint = true)
    )]
    async fn list_tags(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<TagList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        // One past the cap, so `truncated` can be set.
        let mut tags = self
            .state
            .target_store
            .list_tags(auth.org, None, TAG_CAP + 1)
            .await
            .map_err(|e| McpToolError::internal(format!("list tags: {e}")))?;
        let truncated = tags.len() > TAG_CAP;
        tags.truncate(TAG_CAP);
        Ok(Json(TagList {
            items: tags
                .into_iter()
                .map(|t| TagItem {
                    name: sanitize_data(&t.name),
                    count: t.count,
                })
                .collect(),
            truncated,
        }))
    }

    /// The channel inventory. Channels are created in the app, where their
    /// tokens and addresses are entered; this only names them.
    #[tool(
        description = "The org's notification channels: id, operator-set name, kind (email, slack, telegram, webhook, and so on), and whether the channel is enabled. Two flags say a channel is not working even where it reads as ready: awaiting_verification for an email address nobody confirmed, and not_delivering for an enabled channel whose recent alerts all failed to arrive. auto_bind_tags is the channel's tag rule: it also pages any monitor carrying one of those tags, so a monitor with no binding can still be covered. Channel settings are withheld, since they hold webhook URLs and bot tokens. Channels are created in the Uptimepage app, not here. Read-only.",
        title = "List notification channels",
        annotations(read_only_hint = true)
    )]
    async fn list_notification_channels(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<ChannelList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::ChannelsRead)?;
        let failure_limit = self.state.cfg.escalation.channel_failure_limit;
        let channels = self
            .state
            .notification_channel_store
            .list(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("list channels: {e}")))?;
        Ok(Json(ChannelList {
            items: channels
                .into_iter()
                .map(|c| ChannelItem {
                    id: c.id.to_string(),
                    name: sanitize_data(&c.name),
                    kind: c.kind.as_db_str().to_string(),
                    // Both of these read as ready without saying so: an enabled
                    // channel can be unconfirmed, or landing nothing.
                    awaiting_verification: c.awaiting_verification(),
                    not_delivering: c.is_failing(failure_limit),
                    enabled: c.enabled,
                    auto_bind_tags: c.auto_bind_tags.iter().map(|t| sanitize_data(t)).collect(),
                })
                .collect(),
        }))
    }

    #[tool(
        description = "A browser flow monitor's recent runs over a window (1h/24h/7d/30d): every declared step with its outcome and duration, the step a failure stopped on, and the page the browser saw. Use this to answer why a login check failed. Read-only.",
        title = "Browser flow runs",
        annotations(read_only_hint = true)
    )]
    async fn get_flow_runs(
        &self,
        Parameters(args): Parameters<FlowWindowArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<FlowRunList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let org = auth.org;
        let id = parse_uuid(&args.id, "monitor id")?;
        let (span, _) = parse_window(&args.window)?;
        self.require_flow(org, id).await?;

        let range = self.clamped_raw_window(org, span).await?;
        let runs = self
            .state
            .results_store
            .flow_runs(org, id, range, None, FLOW_RUN_CAP)
            .await
            .map_err(|e| McpToolError::internal(format!("flow runs: {e}")))?;

        Ok(Json(FlowRunList {
            runs: runs.into_iter().map(flow_run_item).collect(),
        }))
    }

    #[tool(
        description = "How long each step of a browser flow monitor takes over a window (1h/24h/7d/30d), and how far it has moved: per step the earliest and latest mean duration, their ratio, and how many runs passed or failed it. Use this to spot a step drifting toward failure while the monitor still reports up. Read-only.",
        title = "Browser flow step trend",
        annotations(read_only_hint = true)
    )]
    async fn get_flow_step_trend(
        &self,
        Parameters(args): Parameters<FlowWindowArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<FlowStepTrendSummary>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let org = auth.org;
        let id = parse_uuid(&args.id, "monitor id")?;
        let (span, bucket_secs) = parse_window(&args.window)?;
        self.require_flow(org, id).await?;

        let range = self.clamped_raw_window(org, span).await?;
        let trends = self
            .state
            .results_store
            .flow_step_buckets(org, id, range, bucket_secs, None)
            .await
            .map_err(|e| McpToolError::internal(format!("flow step trend: {e}")))?;

        Ok(Json(FlowStepTrendSummary {
            steps: trends.into_iter().map(step_trend_item).collect(),
        }))
    }

    /// List the org's status pages with their public URLs. For "what status
    /// pages do I publish?".
    #[tool(
        description = "List the org's status pages: slug, name, public URL, enabled. Cursor-paginated. Read-only.",
        title = "List status pages",
        annotations(read_only_hint = true)
    )]
    async fn list_status_pages(
        &self,
        Parameters(args): Parameters<ListStatusPagesArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<StatusPageList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::StatusPageRead)?;
        let org = auth.org;
        let offset = match args.cursor.as_deref() {
            Some(c) => cursor::decode_offset(c)
                .ok_or_else(|| McpToolError::invalid_argument("invalid cursor"))?,
            None => 0,
        };

        let mut pages = self
            .state
            .status_page_store
            .list(org)
            .await
            .map_err(|e| McpToolError::internal(format!("list status pages: {e}")))?;
        pages.sort_by(|a, b| a.slug.cmp(&b.slug));

        let (items, next_cursor) =
            cursor::paginate(&pages, offset, PAGE_SIZE, |p| StatusPageSummary {
                slug: p.slug.clone(),
                name: sanitize_data(&p.name),
                public_url: self.page_public_url(&p.slug),
                enabled: p.enabled,
            });

        Ok(Json(StatusPageList { items, next_cursor }))
    }

    /// One status page with its components and each component's current state —
    /// the "what do customers see" view.
    #[tool(
        description = "One status page: name, public URL, enabled, and its components with each linked monitor's current state. Read-only.",
        title = "Status page details",
        annotations(read_only_hint = true)
    )]
    async fn get_status_page(
        &self,
        Parameters(args): Parameters<GetStatusPageArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<StatusPageDetail>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::StatusPageRead)?;
        let org = auth.org;

        let page = self
            .state
            .status_page_store
            .list(org)
            .await
            .map_err(|e| McpToolError::internal(format!("get status page: {e}")))?
            .into_iter()
            .find(|p| p.slug == args.slug)
            .ok_or_else(|| McpToolError::not_found("status page not found"))?;

        let now = Utc::now();
        let range = TimeRange {
            from: now - Duration::try_hours(HEALTH_WINDOW_HOURS).unwrap_or_default(),
            to: now,
        };
        // Components name a monitor by id, so the target list rides along
        // purely for `region_policy`.
        let (components, rollup, targets) = tokio::try_join!(
            self.state.status_page_store.list_components(org, page.id),
            self.state.results_store.dashboard_rollup(org, range, None),
            self.state.target_store.list(org, all_monitors_filter(None)),
        )
        .map_err(|e| McpToolError::internal(format!("status page components: {e}")))?;
        let metrics = index_by_target(rollup);
        let folded = self
            .state
            .folded_status(org, range, crate::app::folded_status_policies(&targets))
            .await;

        let components = components
            .into_iter()
            .map(|c| McpComponent {
                public_name: sanitize_data(&c.public_name.unwrap_or(c.monitor_name)),
                group: c.public_group.as_deref().map(sanitize_data),
                linked_monitor: c.target_id.to_string(),
                state: current_state(metrics.get(&c.target_id), folded.get(&c.target_id).copied())
                    .to_string(),
            })
            .collect();

        Ok(Json(StatusPageDetail {
            slug: page.slug.clone(),
            name: sanitize_data(&page.name),
            public_url: self.page_public_url(&page.slug),
            enabled: page.enabled,
            components,
        }))
    }

    /// Incidents across the org: open ones by default, or the full history in a
    /// window with `state: "all"`. The entry point for "what incidents are
    /// open?", "what broke last week", and for obtaining an incident id to read
    /// or acknowledge.
    #[tool(
        description = "List the org's incidents: incident id, affected monitor, severity, open/resolved times, and latest update phase. Defaults to currently-open ones; pass state=\"all\" with an optional from/to window (default: last 30 days) for resolved history, and monitor_id to narrow to one monitor. Read-only.",
        title = "List incidents",
        annotations(read_only_hint = true)
    )]
    async fn list_incidents(
        &self,
        Parameters(args): Parameters<ListIncidentsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::IncidentsRead)?;
        let org = auth.org;
        let page = match args.cursor.as_deref() {
            Some(c) => cursor::decode_query::<IncidentPage>(c)
                .ok_or_else(|| McpToolError::invalid_argument("invalid cursor"))?,
            None => IncidentPage {
                offset: 0,
                open_only: parse_incident_state_filter(args.state.as_deref())?,
                range: incident_window(args.from.as_deref(), args.to.as_deref(), Utc::now())?,
                target_id: args
                    .monitor_id
                    .as_deref()
                    .map(|id| parse_uuid(id, "monitor id"))
                    .transpose()?,
            },
        };
        let IncidentPage {
            offset,
            open_only,
            range,
            target_id,
        } = page;

        // Peek one row past the page so `next_cursor` needs no second query.
        let mut incidents = self
            .state
            .incident_narration_store
            .list_briefs(
                org,
                IncidentBriefFilter {
                    range: Some(range),
                    target_id,
                    open_only,
                    oldest_first: false,
                    limit: PAGE_SIZE + 1,
                    offset,
                },
            )
            .await
            .map_err(|e| McpToolError::internal(format!("list incidents: {e}")))?;

        let next_cursor = (incidents.len() > PAGE_SIZE)
            .then(|| {
                cursor::encode_query(&IncidentPage {
                    offset: offset.saturating_add(PAGE_SIZE),
                    open_only,
                    range,
                    target_id,
                })
            })
            .flatten();
        incidents.truncate(PAGE_SIZE);

        Ok(Json(IncidentList {
            items: incidents.iter().map(incident_summary).collect(),
            from: range.from.to_rfc3339(),
            to: range.to.to_rfc3339(),
            next_cursor,
        }))
    }

    /// One incident with its full operator-update timeline. Use after
    /// `list_incidents`/`get_org_health` to read what's been posted before
    /// acknowledging.
    #[tool(
        description = "One incident: affected monitor, severity, open/resolved times, error sample, and the full operator-update timeline. Read-only.",
        title = "Incident details",
        annotations(read_only_hint = true)
    )]
    async fn get_incident(
        &self,
        Parameters(args): Parameters<IncidentIdArg>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentDetail>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::IncidentsRead)?;
        let org = auth.org;
        let id = parse_uuid(&args.id, "incident id")?;

        let incident = self
            .state
            .incident_narration_store
            .get(org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("get incident: {e}")))?
            .ok_or_else(|| McpToolError::not_found("incident not found"))?;

        let monitor_name = match incident.target_id {
            Some(t) => self
                .state
                .target_store
                .get(org, t)
                .await
                .ok()
                .flatten()
                .map(|x| x.name),
            None => None,
        };

        Ok(Json(incident_detail(&incident, monitor_name)))
    }

    /// Incident reporting over a trailing window: MTTA/MTTR, counts by
    /// severity/state, auto-vs-human resolution, and the noisiest monitors.
    #[tool(
        description = "Incident metrics over a trailing window (default 30 days): MTTA/MTTR in seconds, total incidents, counts by severity and state, auto- vs human-resolved, and the noisiest monitors. Read-only.",
        title = "Incident metrics",
        annotations(read_only_hint = true)
    )]
    async fn get_incident_metrics(
        &self,
        Parameters(args): Parameters<GetIncidentMetricsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<IncidentMetricsResult>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::IncidentsRead)?;
        let org = auth.org;
        let window = args.window_days.unwrap_or(30).clamp(1, 365);
        let m = self
            .state
            .incident_ops_store
            .metrics(org, window)
            .await
            .map_err(|e| McpToolError::internal(format!("incident metrics: {e}")))?;
        let buckets = |v: Vec<crate::domain::MetricBucket>| {
            v.into_iter()
                .map(|b| MetricCount {
                    key: b.key,
                    count: b.count,
                })
                .collect()
        };
        Ok(Json(IncidentMetricsResult {
            window_days: m.window_days,
            total: m.total,
            mtta_secs: m.mtta_secs,
            mttr_secs: m.mttr_secs,
            by_severity: buckets(m.by_severity),
            by_state: buckets(m.by_state),
            auto_resolved: m.auto_resolved,
            human_resolved: m.human_resolved,
            top_monitors: m
                .top_monitors
                .into_iter()
                .map(|t| NoisyMonitor {
                    monitor_id: t.target_id.to_string(),
                    count: t.count,
                })
                .collect(),
        }))
    }

    /// The org's variable keys, so a check can reference one rather than carry
    /// its value.
    #[tool(
        description = "The org's reusable variables: key, and whether it is a secret. Values are never returned, and a secret's value is never even read. Write a variable into a monitor's header or body as `{{ key }}`, which is resolved when the check runs: this is how an authenticated check is built here, since pasting the credential itself is refused. Variables are created and edited in the app, not here. Read-only.",
        title = "List variables",
        annotations(read_only_hint = true)
    )]
    async fn list_variables(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<VariableList>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::VariablesRead)?;
        let vars = self
            .state
            .variable_store
            .list(auth.org)
            .await
            .map_err(|e| McpToolError::internal(format!("list variables: {e}")))?;
        Ok(Json(VariableList {
            items: vars
                .into_iter()
                .map(|v| VariableSummary {
                    key: v.key,
                    is_secret: v.is_secret,
                })
                .collect(),
        }))
    }

    /// Usage against plan limits, and the cheapest answer to "which org is this
    /// connector bound to?", which the client cannot see for itself.
    #[tool(
        description = "Which org this connector is bound to, and the account's resource usage against plan limits: monitors, status pages, members, components, and key policy values. Caps are pooled across every org the account owns, so the counts can exceed what this one org holds. Read-only.",
        title = "Usage against plan",
        annotations(read_only_hint = true)
    )]
    async fn get_org_usage(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<OrgUsage>, McpToolError> {
        let auth = McpAuth::from_ctx(&ctx)?;
        auth.require(Scope::TargetsRead)?;
        let pool = self
            .state
            .db
            .as_ref()
            .ok_or_else(|| McpToolError::internal("db unavailable"))?;
        let (usage, org_row) = tokio::join!(
            self.state.quotas.account_usage(auth.org),
            crate::storage::orgs::get_org(pool, auth.org),
        );
        let u = usage.map_err(|e| McpToolError::internal(format!("org usage: {e}")))?;
        let org_row = org_row.map_err(|e| McpToolError::internal(format!("org usage: {e}")))?;
        let (org_slug, org_name) = match org_row {
            Some(o) => (o.slug, sanitize_data(&o.name)),
            None => (auth.org.0.to_string(), String::new()),
        };
        let p = &u.plan;
        Ok(Json(OrgUsage {
            org: org_slug,
            org_name,
            plan: p.id.clone(),
            targets: Quota {
                used: u.targets,
                cap: p.max_targets.into(),
            },
            status_pages: Quota {
                used: u.status_pages,
                cap: p.max_status_pages.into(),
            },
            members: Quota {
                used: u.members,
                cap: p.max_members.into(),
            },
            public_components: Quota {
                used: u.public_components,
                cap: p.max_public_components.into(),
            },
            maintenance_windows: Quota {
                used: u.maintenance_windows,
                cap: p.max_maintenance_windows.into(),
            },
            notification_channels: Quota {
                used: u.notification_channels,
                cap: p.max_notification_channels.into(),
            },
            min_check_interval_secs: p.min_check_interval_secs.into(),
            retention_days: p.retention_days.into(),
        }))
    }
}

/// Filter that returns every monitor in the org (optionally tag-scoped),
/// bounded by [`MAX_LIST_FETCH`].
fn all_monitors_filter(tag: Option<String>) -> TargetFilter {
    TargetFilter {
        limit: Some(MAX_LIST_FETCH),
        offset: 0,
        tag,
        ..Default::default()
    }
}

fn index_by_target(rollup: Vec<DashboardMetrics>) -> HashMap<Uuid, DashboardMetrics> {
    rollup.into_iter().map(|m| (m.target_id, m)).collect()
}

/// The query behind one `list_incidents` page, round-tripped through the
/// cursor so later pages answer the same question over the same window.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct IncidentPage {
    pub(super) offset: usize,
    pub(super) open_only: bool,
    pub(super) range: TimeRange,
    pub(super) target_id: Option<Uuid>,
}
