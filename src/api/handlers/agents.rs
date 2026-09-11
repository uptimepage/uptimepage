//! Region-agent surface: config pull + result ingest. Authenticated by the
//! agent's own bearer token, which resolves to a region + agent id — never org
//! membership and never the request body.

use crate::api::json::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use std::collections::HashMap;

use uuid::Uuid;

use crate::ad_hoc_dispatch::DeliveredResult;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::OrgId;
use crate::domain::agent_wire::{
    AgentTargetDto, AgentTargetsResponse, DispatchBatch, DispatchKind, DispatchReport,
    FlowRunRecord, IngestRequest, IngestResponse,
};
use crate::error::{AppError, Result};
use crate::storage::admin::AdminRepo;
use crate::storage::operator::OperatorRepo;
use crate::web::AgentIdentity;

/// Max interactive checks handed to one agent per long-poll return.
const DISPATCH_CLAIM_LIMIT: usize = 32;

const INGEST_MAX_BATCH: usize = 10_000;
/// Far tighter than the result cap: a monitor runs one every 300s at most, so a
/// batch of thousands is a bug or an attempt to make us write rows on demand.
const INGEST_MAX_FLOW_RUNS: usize = 256;
/// Reject results timestamped further ahead than this. Past timestamps are
/// allowed — an agent buffering through a control-plane outage legitimately
/// drains older results — but a future timestamp would land in the wrong CH
/// partition and skew the TTL window.
const MAX_FUTURE_SKEW_SECS: i64 = 300;

pub async fn pull_targets(
    State(state): State<AppState>,
    headers: HeaderMap,
    agent: AgentIdentity,
) -> Result<Response> {
    let region = agent.region;
    // Self-reported: only a flow-capable agent is handed flow checks. A change
    // ships with an agent restart, which clears its etag cache and forces a full
    // re-pull, so the region etag need not encode capability.
    let flow_capable = headers
        .get("x-flow-capable")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let db = state.require_db()?.clone();
    let repo = AdminRepo::new(db.clone(), state.cipher.clone(), "agent_config_pull");

    // Resolved before the etag and reused for the clamp below, so what the agent
    // is told changed and what it is handed come from one reading of the plan.
    let plans =
        crate::quotas::effective::resolve_plans(&state.quotas, repo.region_org_ids(&region).await?)
            .await;

    // Validate the cheap etag first so an unchanged poll returns 304 without
    // decrypting any credentials.
    let etag = repo
        .region_pull_etag(
            &region,
            &crate::quotas::effective::RegionCaps::from(&plans),
            &crate::quotas::effective::plan_digest(&plans),
        )
        .await?;
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag)
    {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    // Persist it for create-time region validation before returning, so a create
    // right after an agent first reports capability can't miss it. Only full
    // pulls reach here (a 304 returned above), so the extra write is rare.
    if let Ok(id) = agent.agent_id.parse::<uuid::Uuid>()
        && let Err(err) = OperatorRepo::new(db)
            .set_agent_flow_capable(id, flow_capable)
            .await
    {
        tracing::warn!(error = %err, "persisting agent flow_capable failed");
    }

    let targets =
        crate::quotas::effective::region_targets(&repo, &region, flow_capable, &plans).await?;
    let body = AgentTargetsResponse {
        region,
        targets: targets
            .into_iter()
            .map(|(org, target)| AgentTargetDto {
                org_id: org.0,
                target,
            })
            .collect(),
    };
    Ok((StatusCode::OK, [(header::ETAG, etag)], Json(body)).into_response())
}

pub async fn ingest_results(
    State(state): State<AppState>,
    agent: AgentIdentity,
    Json(req): Json<IngestRequest>,
) -> Result<Response> {
    let region = &agent.region;
    if req.results.len() > INGEST_MAX_BATCH {
        return Err(AppError::unprocessable(
            codes::AGENT_BATCH_TOO_LARGE,
            format!(
                "batch of {} results exceeds the {INGEST_MAX_BATCH} cap",
                req.results.len()
            ),
        ));
    }
    if req.flow_runs.len() > INGEST_MAX_FLOW_RUNS {
        return Err(AppError::unprocessable(
            codes::AGENT_BATCH_TOO_LARGE,
            format!(
                "batch of {} flow runs exceeds the {INGEST_MAX_FLOW_RUNS} cap",
                req.flow_runs.len()
            ),
        ));
    }

    // Idempotent retry: a batch_id we already committed is a no-op.
    if state.agent_ingest_dedup.get(&req.batch_id).is_some() {
        return Ok(accepted(0, 0, true));
    }
    if req.results.is_empty() {
        return Ok(accepted(0, 0, false));
    }

    let pool = state.require_db()?.clone();
    let repo = AdminRepo::new(pool, None, "agent_ingest");
    let assigned = repo.assigned_targets_for_region(region).await?;

    // Drop — never reject the whole batch on — a single bad row: a future-skewed
    // timestamp (one host with a wrong clock would otherwise poison every other
    // result) or a target not assigned to this region (anti-spoof; also covers a
    // benign reassignment race). The authoritative org_id is stamped from the
    // assignment, never the agent-supplied value.
    let total = req.results.len();
    let cutoff = chrono::Utc::now() + chrono::Duration::seconds(MAX_FUTURE_SKEW_SECS);
    let mut skewed = 0usize;
    let mut foreign = 0usize;
    let mut results = req.results;
    results.retain_mut(|r| {
        if r.timestamp > cutoff {
            skewed += 1;
            return false;
        }
        match assigned.get(&r.target_id) {
            Some(org) => {
                r.org_id = org.0;
                true
            }
            None => {
                foreign += 1;
                false
            }
        }
    });
    let dropped = skewed + foreign;
    if dropped > 0 {
        tracing::warn!(
            region = %region,
            agent_id = %agent.agent_id,
            total,
            dropped_skew = skewed,
            dropped_foreign = foreign,
            "agent ingest dropped rows"
        );
    }

    // Before the early return below: a batch whose results were all dropped can
    // still carry runs for targets this region serves, and the agent has already
    // let go of them.
    store_flow_runs(&state, req.flow_runs, &assigned, region).await;

    if results.is_empty() {
        // Nothing written → don't mark the batch consumed, so a resend after the
        // assignment settles is reprocessed rather than swallowed as a duplicate.
        return Ok(accepted(0, dropped, false));
    }

    state
        .result_sink
        .write_batch_tagged(&results, region, &agent.agent_id)
        .await?;
    // Mark consumed only after a successful write so a failed write stays
    // retriable.
    state.agent_ingest_dedup.insert(req.batch_id, ());
    Ok(accepted(results.len(), dropped, false))
}

/// Drop runs for targets this region is not assigned, and restamp the survivors'
/// org from the assignment rather than trusting the body — the same anti-spoof
/// rule the results go through.
fn retain_assigned(runs: &mut Vec<FlowRunRecord>, assigned: &HashMap<Uuid, OrgId>) {
    runs.retain_mut(|r| match assigned.get(&r.target_id) {
        Some(org) => {
            r.org_id = org.0;
            true
        }
        None => false,
    });
}

/// Persist the flow runs riding along with a result batch. Secrets are stripped
/// by the sink itself, so every path that stores a run gets the same treatment.
async fn store_flow_runs(
    state: &AppState,
    mut runs: Vec<FlowRunRecord>,
    assigned: &HashMap<Uuid, OrgId>,
    region: &str,
) {
    let Some(sink) = state.flow_run_sink.as_ref() else {
        return;
    };
    let sent = runs.len();
    retain_assigned(&mut runs, assigned);
    if runs.len() < sent {
        tracing::warn!(
            region = %region,
            sent,
            dropped = sent - runs.len(),
            "agent ingest dropped flow runs for unassigned targets"
        );
    }
    if runs.is_empty() {
        return;
    }
    sink.write_runs_tagged(&runs, region).await;
}

fn accepted(n: usize, dropped: usize, duplicate: bool) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(IngestResponse {
            accepted: n,
            dropped,
            duplicate,
        }),
    )
        .into_response()
}

/// Long-poll for interactive checks in this agent's region: held open until a
/// check is dispatched or the hold window elapses (then returns empty and the
/// agent reconnects). Region + identity come from the bearer token.
pub async fn claim_dispatch(
    State(state): State<AppState>,
    agent: AgentIdentity,
) -> Result<Json<DispatchBatch>> {
    let claim = state.ad_hoc.claim(&agent.region, DISPATCH_CLAIM_LIMIT);
    // Return empty immediately on shutdown so a held long-poll doesn't stall
    // graceful drain for the full hold window; the agent reconnects on restart.
    let checks = match &state.shutdown {
        Some(token) => tokio::select! {
            c = claim => c,
            _ = token.cancelled() => Vec::new(),
        },
        None => claim.await,
    };
    Ok(Json(DispatchBatch { checks }))
}

/// Report the result of one check. Routes it to the waiting request; for
/// `check_now` also persists to ClickHouse under the check's authoritative org +
/// target (never the agent-supplied fields). No-op if no waiter is registered.
pub async fn submit_dispatch_result(
    State(state): State<AppState>,
    agent: AgentIdentity,
    Json(req): Json<DispatchReport>,
) -> Result<Response> {
    let result_for_ch = req.result.clone();
    let delivered = DeliveredResult {
        result: req.result,
        response_headers_preview: req.response_headers_preview,
        response_body_snippet: req.response_body_snippet,
        flow_evidence: req.flow_evidence,
        flow_steps: req.flow_steps,
    };
    if let Some(meta) = state.ad_hoc.complete(req.check_id, delivered)
        && meta.kind == DispatchKind::CheckNow
        && let Some(target_id) = meta.target_id
    {
        let mut result = result_for_ch;
        result.target_id = target_id;
        result.org_id = meta.org_id;
        state
            .result_sink
            .write_batch_tagged(
                std::slice::from_ref(&result),
                &agent.region,
                &agent.agent_id,
            )
            .await?;
    }
    Ok(StatusCode::ACCEPTED.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CheckStatus;

    fn run(target_id: Uuid, org_id: Uuid) -> FlowRunRecord {
        FlowRunRecord {
            org_id,
            target_id,
            timestamp: chrono::Utc::now(),
            status: CheckStatus::Down,
            duration_ms: 10,
            error: None,
            steps: Vec::new(),
            evidence: None,
        }
    }

    #[test]
    fn runs_for_an_unassigned_target_are_dropped() {
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let assigned = HashMap::from([(mine, OrgId(owner))]);

        let mut runs = vec![run(theirs, owner), run(mine, owner)];
        retain_assigned(&mut runs, &assigned);

        assert_eq!(
            runs.len(),
            1,
            "a target this region does not serve is spoof"
        );
        assert_eq!(runs[0].target_id, mine);
    }

    #[test]
    fn the_org_comes_from_the_assignment_not_the_body() {
        let target = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let claimed = Uuid::new_v4();
        let assigned = HashMap::from([(target, OrgId(owner))]);

        let mut runs = vec![run(target, claimed)];
        retain_assigned(&mut runs, &assigned);

        assert_eq!(runs[0].org_id, owner, "a claimed org would cross tenants");
    }
}
