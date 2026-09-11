//! Owner endpoints for escalation policies — the paging ladders + the bindings
//! that point a monitor (or the org default) at one.
//!
//! Reads require `oncall:read`; mutations are owner-only (`OwnerAuthorized<
//! OnCallWrite>`) because a policy is org configuration, not incident data. The
//! engine consumes these at page time; see `escalation::engine`.

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::api::ApiError;
use crate::api::error::codes;
use crate::app::AppState;
use crate::domain::{
    EscalationPolicy, EscalationPolicySummary, EscalationTargetType, NewEscalationPolicy,
};
use crate::error::{AppError, Result};
use crate::web::{Authorized, OnCallRead, OnCallWrite, OwnerAuthorized};

/// Pointing something at a policy is new paging coverage and needs the plan to
/// sell it; clearing a binding is always allowed, so an org that has lost the
/// feature can still unwire what it has.
async fn gate_binding(
    state: &AppState,
    org: crate::domain::OrgId,
    b: &PolicyBinding,
) -> Result<()> {
    if b.policy_id.is_none() {
        return Ok(());
    }
    let plan = state.quotas.limit_for_org(org).await?;
    crate::api::handlers::on_call::gate_on_call(state, &plan)
}

const MAX_NAME: usize = 100;
const MAX_DESCRIPTION: usize = 2000;
const MAX_STEPS: usize = 20;
const MAX_TARGETS_PER_STEP: usize = 20;

/// Bind a monitor or the org default to a policy (or clear it with `null`).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PolicyBinding {
    #[serde(default)]
    #[schema(nullable = true)]
    pub policy_id: Option<Uuid>,
}

fn invalid(msg: impl Into<String>) -> AppError {
    AppError::unprocessable(codes::ESCALATION_POLICY_INVALID, msg)
}

/// Validate a policy payload: levels unique and positive, and each target sets
/// exactly the one id matching its type. Membership of `user` targets and
/// ownership of `schedule`/`channel` targets are enforced in the store on write
/// (so the IDOR check holds regardless of caller).
fn validate(new: &NewEscalationPolicy) -> Result<()> {
    let name = new.name.trim();
    if name.is_empty() {
        return Err(invalid("policy name is required"));
    }
    if name.len() > MAX_NAME {
        return Err(invalid("policy name is too long"));
    }
    if new
        .description
        .as_ref()
        .is_some_and(|d| d.len() > MAX_DESCRIPTION)
    {
        return Err(invalid("policy description is too long"));
    }
    if new.repeat_count < 0 {
        return Err(invalid("repeat_count must be >= 0"));
    }
    if new.steps.len() > MAX_STEPS {
        return Err(invalid("too many escalation steps"));
    }
    let mut levels = Vec::with_capacity(new.steps.len());
    for step in &new.steps {
        if step.level < 1 {
            return Err(invalid("escalation level must be >= 1"));
        }
        if levels.contains(&step.level) {
            return Err(invalid("duplicate escalation level"));
        }
        levels.push(step.level);
        if step.delay_secs < 0 {
            return Err(invalid("step delay must be >= 0"));
        }
        if step.targets.is_empty() {
            return Err(invalid("each step needs at least one target"));
        }
        if step.targets.len() > MAX_TARGETS_PER_STEP {
            return Err(invalid("too many targets in one step"));
        }
        for t in &step.targets {
            let set = [
                t.user_id.is_some(),
                t.schedule_id.is_some(),
                t.channel_id.is_some(),
            ];
            if set.iter().filter(|x| **x).count() != 1 {
                return Err(invalid(
                    "a target must set exactly one of user/schedule/channel",
                ));
            }
            let ok = match t.target_type {
                EscalationTargetType::Channel => t.channel_id.is_some(),
                EscalationTargetType::User => t.user_id.is_some(),
                EscalationTargetType::Schedule => t.schedule_id.is_some(),
            };
            if !ok {
                return Err(invalid("target id must match its target_type"));
            }
        }
    }
    Ok(())
}

fn not_found() -> AppError {
    AppError::not_found(
        codes::ESCALATION_POLICY_NOT_FOUND,
        "escalation policy not found",
    )
}

#[utoipa::path(
    get, path = "/api/v1/escalation-policies", tag = "escalation",
    summary = "List escalation policies",
    responses((status = 200, body = [EscalationPolicySummary])),
)]
pub async fn list(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
) -> Result<Json<Vec<EscalationPolicySummary>>> {
    Ok(Json(state.escalation_policy_store.list(org).await?))
}

#[utoipa::path(
    post, path = "/api/v1/escalation-policies", tag = "escalation",
    summary = "Create an escalation policy",
    request_body = NewEscalationPolicy,
    responses((status = 201, body = EscalationPolicy), (status = 422, body = ApiError)),
)]
pub async fn create(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Json(new): Json<NewEscalationPolicy>,
) -> Result<(StatusCode, Json<EscalationPolicy>)> {
    validate(&new)?;
    let plan = state.quotas.limit_for_org(org).await?;
    crate::api::handlers::on_call::gate_on_call(&state, &plan)?;
    // Friendly pre-check; the store INSERT enforces the same cap atomically.
    state
        .quotas
        .check_can_create_escalation_policy(org, None)
        .await?;
    let limit = i64::from(plan.max_escalation_policies);
    let policy = state
        .escalation_policy_store
        .create(org, new, limit)
        .await?;
    Ok((StatusCode::CREATED, Json(policy)))
}

#[utoipa::path(
    get, path = "/api/v1/escalation-policies/{id}", tag = "escalation",
    summary = "Get an escalation policy with its steps",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = EscalationPolicy), (status = 404, body = ApiError)),
)]
pub async fn get(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<EscalationPolicy>> {
    state
        .escalation_policy_store
        .get(org, id)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

#[utoipa::path(
    patch, path = "/api/v1/escalation-policies/{id}", tag = "escalation",
    summary = "Replace an escalation policy's metadata and steps",
    params(("id" = Uuid, Path)), request_body = NewEscalationPolicy,
    responses((status = 200, body = EscalationPolicy), (status = 404, body = ApiError), (status = 422, body = ApiError)),
)]
pub async fn replace(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Path(id): Path<Uuid>,
    Json(new): Json<NewEscalationPolicy>,
) -> Result<Json<EscalationPolicy>> {
    validate(&new)?;
    state
        .escalation_policy_store
        .replace(org, id, new)
        .await?
        .map(Json)
        .ok_or_else(not_found)
}

#[utoipa::path(
    delete, path = "/api/v1/escalation-policies/{id}", tag = "escalation",
    summary = "Delete an escalation policy",
    params(("id" = Uuid, Path)),
    responses((status = 204), (status = 404, body = ApiError)),
)]
pub async fn delete(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if state.escalation_policy_store.delete(org, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

/// Resolve a binding target to a live policy id, rejecting an unknown one.
async fn validate_binding(
    state: &AppState,
    org: crate::domain::OrgId,
    b: &PolicyBinding,
) -> Result<()> {
    if let Some(pid) = b.policy_id
        && state.escalation_policy_store.get(org, pid).await?.is_none()
    {
        return Err(not_found());
    }
    Ok(())
}

#[utoipa::path(
    get, path = "/api/v1/escalation-policies/default", tag = "escalation",
    summary = "Get the org default escalation policy",
    responses((status = 200, body = PolicyBinding)),
)]
pub async fn get_org_default(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
) -> Result<Json<PolicyBinding>> {
    let policy_id = state.escalation_policy_store.org_default(org).await?;
    Ok(Json(PolicyBinding { policy_id }))
}

#[utoipa::path(
    put, path = "/api/v1/escalation-policies/default", tag = "escalation",
    summary = "Set or clear the org default escalation policy",
    request_body = PolicyBinding,
    responses((status = 200, body = PolicyBinding), (status = 404, body = ApiError)),
)]
pub async fn set_org_default(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Json(b): Json<PolicyBinding>,
) -> Result<Json<PolicyBinding>> {
    gate_binding(&state, org, &b).await?;
    validate_binding(&state, org, &b).await?;
    state
        .escalation_policy_store
        .set_org_default(org, b.policy_id)
        .await?;
    Ok(Json(b))
}

#[utoipa::path(
    get, path = "/api/v1/targets/{id}/escalation-policy", tag = "escalation",
    summary = "Get a monitor's escalation policy binding",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = PolicyBinding), (status = 404, body = ApiError)),
)]
pub async fn get_target_policy(
    State(state): State<AppState>,
    Authorized(org, _): Authorized<OnCallRead>,
    Path(id): Path<Uuid>,
) -> Result<Json<PolicyBinding>> {
    if state.target_store.get(org, id).await?.is_none() {
        return Err(AppError::not_found(
            codes::TARGET_NOT_FOUND,
            "monitor not found",
        ));
    }
    // The monitor's own binding only (not the org-default fallback), so the
    // form can show "inherit" vs "override".
    let policy_id = state.escalation_policy_store.target_policy(org, id).await?;
    Ok(Json(PolicyBinding { policy_id }))
}

#[utoipa::path(
    put, path = "/api/v1/targets/{id}/escalation-policy", tag = "escalation",
    summary = "Bind or clear a monitor's escalation policy",
    params(("id" = Uuid, Path)), request_body = PolicyBinding,
    responses((status = 200, body = PolicyBinding), (status = 404, body = ApiError)),
)]
pub async fn set_target_policy(
    State(state): State<AppState>,
    OwnerAuthorized(org, _): OwnerAuthorized<OnCallWrite>,
    Path(id): Path<Uuid>,
    Json(b): Json<PolicyBinding>,
) -> Result<Json<PolicyBinding>> {
    gate_binding(&state, org, &b).await?;
    validate_binding(&state, org, &b).await?;
    if !state
        .escalation_policy_store
        .set_target_policy(org, id, b.policy_id)
        .await?
    {
        return Err(AppError::not_found(
            codes::TARGET_NOT_FOUND,
            "monitor not found",
        ));
    }
    Ok(Json(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NewEscalationStep, NewEscalationTarget};

    fn policy(steps: Vec<NewEscalationStep>) -> NewEscalationPolicy {
        NewEscalationPolicy {
            name: "primary".into(),
            description: None,
            repeat_count: 0,
            steps,
        }
    }

    fn channel_target() -> NewEscalationTarget {
        NewEscalationTarget {
            target_type: EscalationTargetType::Channel,
            user_id: None,
            schedule_id: None,
            channel_id: Some(Uuid::now_v7()),
        }
    }

    #[test]
    fn accepts_a_channel_ladder() {
        let p = policy(vec![
            NewEscalationStep {
                level: 1,
                delay_secs: 300,
                targets: vec![channel_target()],
            },
            NewEscalationStep {
                level: 2,
                delay_secs: 600,
                targets: vec![channel_target()],
            },
        ]);
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn rejects_blank_name() {
        let mut p = policy(vec![]);
        p.name = "  ".into();
        assert!(validate(&p).is_err());
    }

    #[test]
    fn rejects_duplicate_levels() {
        let p = policy(vec![
            NewEscalationStep {
                level: 1,
                delay_secs: 0,
                targets: vec![channel_target()],
            },
            NewEscalationStep {
                level: 1,
                delay_secs: 0,
                targets: vec![channel_target()],
            },
        ]);
        assert!(validate(&p).is_err());
    }

    #[test]
    fn accepts_user_and_schedule_targets() {
        let user = NewEscalationStep {
            level: 1,
            delay_secs: 0,
            targets: vec![NewEscalationTarget {
                target_type: EscalationTargetType::User,
                user_id: Some(Uuid::now_v7()),
                schedule_id: None,
                channel_id: None,
            }],
        };
        assert!(validate(&policy(vec![user])).is_ok());
        let schedule = NewEscalationStep {
            level: 1,
            delay_secs: 0,
            targets: vec![NewEscalationTarget {
                target_type: EscalationTargetType::Schedule,
                user_id: None,
                schedule_id: Some(Uuid::now_v7()),
                channel_id: None,
            }],
        };
        assert!(validate(&policy(vec![schedule])).is_ok());
    }

    #[test]
    fn rejects_target_id_not_matching_type() {
        // A user target carrying only a channel id is rejected.
        let mismatched = NewEscalationStep {
            level: 1,
            delay_secs: 0,
            targets: vec![NewEscalationTarget {
                target_type: EscalationTargetType::User,
                user_id: None,
                schedule_id: None,
                channel_id: Some(Uuid::now_v7()),
            }],
        };
        assert!(validate(&policy(vec![mismatched])).is_err());
    }

    #[test]
    fn rejects_target_with_no_id_or_multiple_ids() {
        let none = NewEscalationStep {
            level: 1,
            delay_secs: 0,
            targets: vec![NewEscalationTarget {
                target_type: EscalationTargetType::Channel,
                user_id: None,
                schedule_id: None,
                channel_id: None,
            }],
        };
        assert!(validate(&policy(vec![none])).is_err());
        let two = NewEscalationStep {
            level: 1,
            delay_secs: 0,
            targets: vec![NewEscalationTarget {
                target_type: EscalationTargetType::Channel,
                user_id: Some(Uuid::now_v7()),
                schedule_id: None,
                channel_id: Some(Uuid::now_v7()),
            }],
        };
        assert!(validate(&policy(vec![two])).is_err());
    }

    #[test]
    fn rejects_empty_step() {
        let empty = NewEscalationStep {
            level: 1,
            delay_secs: 0,
            targets: vec![],
        };
        assert!(validate(&policy(vec![empty])).is_err());
    }
}
