use super::validate::reject_passive_probe;

use uuid::Uuid;

use crate::ad_hoc_dispatch::DeliveredResult;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::agent_wire::{DispatchKind, DispatchedCheck};
use crate::domain::{CheckResult, CheckSpec, OrgId, Target};
use crate::error::{AppError, Result};

/// Run an immediate check on `target` via an agent in its region and return the
/// result. Shared by the REST check-now handler and the MCP tool so both go
/// through the same region-aware dispatch (the agent persists the result).
///
/// The preconditions live here rather than in either caller precisely because
/// there are two of them: a guard on one front door is not a guard.
pub(crate) async fn check_now_via_dispatch(
    state: &AppState,
    org: OrgId,
    target: &Target,
) -> Result<CheckResult> {
    reject_passive_probe(&target.check)?;
    // Every scheduled hand-out already skips a held monitor, so the interactive
    // run has to agree; otherwise this is the one path left that probes what
    // the plan stopped covering and writes a result for it.
    if target.plan_hold_at.is_some() {
        return Err(AppError::forbidden_code(
            codes::PLAN_HOLD,
            "this monitor is paused because your plan no longer covers it",
        ));
    }
    let region = if matches!(&target.check, CheckSpec::Flow(_)) {
        let assigned = state
            .target_store
            .regions_for_target(org, target.id)
            .await?
            .unwrap_or_default();
        pick_flow_region(state, &assigned).await?
    } else {
        resolve_check_now_region(state, org, target.id).await?
    };
    let view = run_ad_hoc(
        state,
        org,
        &region,
        DispatchKind::CheckNow,
        Some(target.id),
        target.check.clone(),
    )
    .await?;
    Ok(view.result)
}

/// Dispatch a target's first check in every assigned region so its status
/// appears within a second of creation instead of waiting up to a full
/// config-pull cycle. Fire-and-forget per region, and only where an agent is
/// already holding the long-poll — regions without a live agent are covered by
/// their next scheduled pull. The agent's result POST persists each result.
pub(crate) async fn dispatch_first_check(
    state: &AppState,
    org: OrgId,
    target: &Target,
    regions: &[String],
) {
    if !target.enabled || target.check.is_passive() {
        return;
    }
    // Restrict a flow first-check to capable regions: a false Error elsewhere
    // would never be overwritten (routing later withholds flow from that region).
    let is_flow = matches!(&target.check, CheckSpec::Flow(_));
    let flow_regions: Vec<String> = if is_flow {
        let mut c = state
            .target_store
            .flow_capable_regions()
            .await
            .unwrap_or_default();
        if state.cfg.flow.enabled {
            c.push(state.cfg.scheduler.effective_default_region().to_string());
        }
        c
    } else {
        Vec::new()
    };
    for region in regions {
        if is_flow && !flow_regions.contains(region) {
            continue;
        }
        if !state.ad_hoc.region_live(region) {
            continue;
        }
        // Fire-and-forget: the dropped receiver is fine — the agent's result
        // POST still persists the check-now result; we just don't wait for it.
        let _rx = state.ad_hoc.dispatch(
            region,
            DispatchedCheck {
                id: Uuid::now_v7(),
                kind: DispatchKind::CheckNow,
                org_id: org.0,
                target_id: Some(target.id),
                spec: target.check.clone(),
            },
        );
    }
}

/// Dispatch an interactive check to an agent holding `region` and wait for the
/// result. 503 when no agent is holding the region (fast) or none answers in
/// time.
pub(crate) async fn run_ad_hoc(
    state: &AppState,
    org: OrgId,
    region: &str,
    kind: DispatchKind,
    target_id: Option<Uuid>,
    check: CheckSpec,
) -> Result<DeliveredResult> {
    // Chokepoint: every interactive dispatch funnels through here, so a future
    // caller can't hand a passive check to an agent by omission.
    reject_passive_probe(&check)?;
    if !state.ad_hoc.region_live(region) {
        return Err(AppError::service_unavailable(
            codes::PROBE_UNAVAILABLE,
            format!("no probe available in region '{region}'; probing runs on agents"),
        ));
    }
    let (check, secrets) = resolve_spec_variables(state, org, check).await?;
    let check_id = Uuid::now_v7();
    let rx = state.ad_hoc.dispatch(
        region,
        DispatchedCheck {
            id: check_id,
            kind,
            org_id: org.0,
            target_id,
            spec: check,
        },
    );
    match tokio::time::timeout(crate::ad_hoc_dispatch::RESULT_WAIT, rx).await {
        Ok(Ok(mut delivered)) => {
            scrub_secrets(&mut delivered, &secrets);
            store_check_now_flow_run(state, org, target_id, region, kind, &delivered).await;
            Ok(delivered)
        }
        _ => {
            state.ad_hoc.abandon(check_id);
            Err(AppError::service_unavailable(
                codes::PROBE_UNAVAILABLE,
                "no probe completed this check in time; the region's agent may be offline",
            ))
        }
    }
}

/// Record a check-now flow run so the manual button and the schedule write the
/// same history. Done here rather than beside the result persist: this is where
/// both the agent and in-process paths converge already scrubbed. A `test` run
/// belongs to no monitor and is never stored.
pub(crate) async fn store_check_now_flow_run(
    state: &AppState,
    org: OrgId,
    target_id: Option<Uuid>,
    region: &str,
    kind: DispatchKind,
    delivered: &crate::ad_hoc_dispatch::DeliveredResult,
) {
    if kind != DispatchKind::CheckNow || delivered.flow_steps.is_empty() {
        return;
    }
    let (Some(sink), Some(target_id)) = (state.flow_run_sink.as_ref(), target_id) else {
        return;
    };
    let r = &delivered.result;
    sink.write_runs_tagged(
        &[crate::domain::agent_wire::FlowRunRecord {
            org_id: org.0,
            target_id,
            timestamp: r.timestamp,
            status: r.status,
            duration_ms: r.duration_ms,
            error: r.error.clone(),
            steps: delivered.flow_steps.clone(),
            evidence: delivered.flow_evidence.clone(),
        }],
        region,
    )
    .await;
}

/// Region to run a check-now in: the target's assigned region with a live
/// agent, else its first region (run_ad_hoc then 503s), else the default
/// region for an unassigned target.
pub(crate) async fn resolve_check_now_region(
    state: &AppState,
    org: OrgId,
    id: Uuid,
) -> Result<String> {
    let regions = state
        .target_store
        .regions_for_target(org, id)
        .await?
        .unwrap_or_default();
    if regions.is_empty() {
        return Ok(state.cfg.scheduler.effective_default_region().to_string());
    }
    if let Some(r) = regions.iter().find(|r| state.ad_hoc.region_live(r)) {
        return Ok(r.clone());
    }
    Ok(regions[0].clone())
}

/// Pick a flow-capable region for an interactive flow check. `prefer` = the
/// caller's candidates (a target's regions or an explicit test region), empty for
/// any. Prefers a live region so the dispatch doesn't 503; errors if none qualify.
/// Regions that can actually run a flow: agents self-reporting the capability,
/// plus the control-plane region when it runs the engine in-process.
pub(crate) async fn flow_capable_set(
    state: &AppState,
) -> Result<std::collections::HashSet<String>> {
    let mut capable: std::collections::HashSet<String> = state
        .target_store
        .flow_capable_regions()
        .await?
        .into_iter()
        .collect();
    if state.cfg.flow.enabled {
        capable.insert(state.cfg.scheduler.effective_default_region().to_string());
    }
    Ok(capable)
}

pub(crate) async fn pick_flow_region(state: &AppState, prefer: &[String]) -> Result<String> {
    let capable: Vec<String> = flow_capable_set(state).await?.into_iter().collect();
    if capable.is_empty() {
        return Err(AppError::service_unavailable(
            codes::PROBE_UNAVAILABLE,
            "no flow-capable agent is available",
        ));
    }
    let candidates: Vec<String> = if prefer.is_empty() {
        capable
    } else {
        let filtered: Vec<String> = prefer
            .iter()
            .filter(|r| capable.contains(*r))
            .cloned()
            .collect();
        if filtered.is_empty() {
            return Err(AppError::unprocessable(
                codes::NO_FLOW_CAPABLE_AGENT,
                "no flow-capable agent runs in the requested region",
            ));
        }
        filtered
    };
    Ok(candidates
        .iter()
        .find(|r| state.ad_hoc.region_live(r))
        .cloned()
        .unwrap_or_else(|| candidates[0].clone()))
}

/// Substitute `{{var}}` references in an interactive check (test / check-now)
/// before it is dispatched, so the operator probes against the real resolved
/// values. Returns the secret plaintexts substituted so the caller can scrub
/// them from a captured response. A missing variable or a policy violation
/// surfaces as a 422 here. Non-HTTP or no-variable specs pass through.
pub(crate) async fn resolve_spec_variables(
    state: &AppState,
    org: OrgId,
    spec: CheckSpec,
) -> Result<(CheckSpec, Vec<String>)> {
    use crate::worker::interpolate::{
        flow_uses_vars, resolve_flow_spec, resolve_http_spec, uses_vars,
    };

    let needs_vars = match &spec {
        CheckSpec::Http(http) => uses_vars(http),
        CheckSpec::Flow(flow) => flow_uses_vars(flow),
        _ => false,
    };
    if !needs_vars {
        return Ok((spec, Vec::new()));
    }
    let vars = state.variable_store.resolve_map(org).await?;
    let unresolved = |e: crate::worker::interpolate::ResolveError| {
        AppError::unprocessable(codes::UNRESOLVED_VARIABLE, e.to_string())
    };
    let resolved = match &spec {
        CheckSpec::Http(http) => {
            CheckSpec::Http(resolve_http_spec(http, &vars).map_err(unresolved)?)
        }
        CheckSpec::Flow(flow) => {
            CheckSpec::Flow(resolve_flow_spec(flow, &vars).map_err(unresolved)?)
        }
        _ => spec,
    };
    Ok((resolved, crate::api::redaction::secret_values(&vars)))
}

/// Replace any resolved secret echoed back in an interactive probe's captured
/// response (body snippet + header values) with `***`, so a value a secret
/// variable supplied is never shown back through the test surface.
pub(crate) fn scrub_secrets(delivered: &mut DeliveredResult, secrets: &[String]) {
    use crate::api::redaction::{redact_secrets, scrub_flow_evidence};
    if secrets.is_empty() {
        return;
    }
    if let Some(snippet) = delivered.response_body_snippet.as_mut() {
        redact_secrets(snippet, secrets);
    }
    for h in &mut delivered.response_headers_preview {
        redact_secrets(&mut h.value, secrets);
    }
    // A flow's error string is its main output and can echo a resolved secret
    // (e.g. a page that reflects a submitted value); scrub it like the rest.
    if let Some(err) = delivered.result.error.as_mut() {
        redact_secrets(err, secrets);
    }
    if let Some(ev) = delivered.flow_evidence.as_mut() {
        scrub_flow_evidence(ev, secrets);
    }
}
