//! Reading and setting a manual monitor's state, the same way from every front
//! door: the API, the console and MCP.

use std::sync::Arc;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::domain::manual::normalize_note;
use crate::domain::{CheckSpec, ManualState, ManualStatus, OrgId, Target, UserId};
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::storage::{ManualChange, ManualStore, ResultSink, TargetStore};
use crate::worker::manual::{ManualRuntime, manual_result};

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetManualState {
    pub status: ManualStatus,
    /// Why, in a line. Omit for none; a set replaces the previous note.
    #[serde(default)]
    #[schema(max_length = 200, example = "carrier reports a trunk outage")]
    pub note: Option<String>,
}

/// Borrowed view over what a set touches, taken from the app state per call.
pub struct ManualOps<'a> {
    pub(crate) targets: &'a dyn TargetStore,
    pub(crate) store: &'a dyn ManualStore,
    pub(crate) runtime: &'a ManualRuntime,
    pub(crate) results: &'a Arc<dyn ResultSink>,
}

impl ManualOps<'_> {
    pub async fn get(&self, org: OrgId, id: Uuid) -> Result<ManualState> {
        let target = self.manual_target(org, id).await?;
        self.current(org, &target).await
    }

    pub async fn current(&self, org: OrgId, target: &Target) -> Result<ManualState> {
        Ok(self
            .store
            .get(org, target.id)
            .await?
            .unwrap_or_else(|| ManualState::initial(target.created_at)))
    }

    /// A set that changes something lands in history at once, so the incident
    /// writer acts on it at its next tick rather than the scheduler's.
    pub async fn set(
        &self,
        org: OrgId,
        id: Uuid,
        req: SetManualState,
        actor: Option<UserId>,
    ) -> Result<ManualChange> {
        let target = self.manual_target(org, id).await?;
        let note = vet_note(req.note.as_deref())?;
        self.set_on(org, &target, req.status, note, actor).await
    }

    /// [`Self::set`] for a caller that already holds the target and a note
    /// from [`vet_note`].
    pub async fn set_on(
        &self,
        org: OrgId,
        target: &Target,
        status: ManualStatus,
        note: Option<String>,
        actor: Option<UserId>,
    ) -> Result<ManualChange> {
        let change = self
            .store
            .set(org, target.id, status, note, actor)
            .await?
            .ok_or_else(not_found)?;
        if change.changed() {
            self.runtime.record(target.id, change.state.clone());
            // Stamped after the record: a restatement that read the old state
            // took its time before this one. Results are stored to the second,
            // so a tie in that second is settled by the next restatement.
            if target.enabled && target.plan_hold_at.is_none() {
                let result = manual_result(target.id, org.0, Utc::now(), &change.state);
                crate::worker::spawn_passive_results(Arc::clone(self.results), vec![result]);
            }
        }
        Ok(change)
    }

    pub async fn manual_target(&self, org: OrgId, id: Uuid) -> Result<Target> {
        let target = self.targets.get(org, id).await?.ok_or_else(not_found)?;
        if !matches!(target.check, CheckSpec::Manual(_)) {
            return Err(AppError::not_found(
                codes::MANUAL_NOT_CONFIGURED,
                "this monitor is not a manual monitor",
            ));
        }
        Ok(target)
    }
}

pub fn vet_note(note: Option<&str>) -> Result<Option<String>> {
    normalize_note(note)
        .map_err(|msg| AppError::bad_request_field(codes::INVALID_MANUAL_STATE, msg, "note"))
}

fn not_found() -> AppError {
    AppError::not_found(codes::TARGET_NOT_FOUND, "target not found")
}
