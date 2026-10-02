//! Schedule, edit and cancel maintenance windows under the same rules for the
//! REST handlers and the MCP tools.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::domain::{
    MaintenanceWindow, MaintenanceWindowUpdate, NewMaintenanceWindow, OrgId, UserId, WindowPhase,
    WriteSource,
};
use crate::error::codes;
use crate::error::validation;
use crate::error::{AppError, Result};
use crate::quotas::QuotaService;
use crate::storage::MaintenanceStore;

const MAX_WINDOW_DAYS: i64 = 30;

/// Borrowed view over the stores a maintenance write touches, taken from the
/// app state per call.
pub struct MaintenanceOps<'a> {
    pub(crate) store: &'a dyn MaintenanceStore,
    pub(crate) quotas: &'a QuotaService,
}

impl MaintenanceOps<'_> {
    pub async fn create(
        &self,
        org: OrgId,
        new: NewMaintenanceWindow,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MaintenanceWindow> {
        self.vet_new(org, &new).await?;
        self.store.create(org, new, source, actor).await
    }

    pub async fn vet_new(&self, org: OrgId, new: &NewMaintenanceWindow) -> Result<()> {
        validation::validate_title(&new.title, "title")?;
        validation::validate_description(new.description.as_deref(), "description")?;
        validate_time_range(new.starts_at, new.ends_at)?;
        require_future_end(new.ends_at)?;
        self.validate_component_ids(org, &new.component_ids).await?;
        // Not atomic with the insert; windows are created one at a time.
        self.quotas
            .check_can_create_maintenance_window(org, None)
            .await
    }

    /// A body that sets nothing returns `existing` unrecorded.
    pub async fn update(
        &self,
        org: OrgId,
        existing: &MaintenanceWindow,
        mut update: MaintenanceWindowUpdate,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<MaintenanceWindow> {
        self.vet_update(org, existing, &mut update, Utc::now())
            .await?;
        if update.changed_fields().is_empty() {
            return Ok(existing.clone());
        }
        // The store re-checks that the window has not ended, so losing that
        // race is a 404 instead of a revived window.
        self.store
            .update(org, existing.id, update, source, actor)
            .await?
            .ok_or_else(|| {
                AppError::not_found(
                    codes::MAINTENANCE_NOT_FOUND,
                    "maintenance window ended or was cancelled meanwhile",
                )
            })
    }

    /// An end-now edit is pinned to `now` in `update`.
    pub async fn vet_update(
        &self,
        org: OrgId,
        existing: &MaintenanceWindow,
        update: &mut MaintenanceWindowUpdate,
        now: DateTime<Utc>,
    ) -> Result<()> {
        reject_closed(existing.phase(now))?;
        if update.changed_fields().is_empty() {
            return Ok(());
        }
        if let Some(t) = update.title.as_deref() {
            validation::validate_title(t, "title")?;
        }
        validation::validate_description(update.description.as_deref(), "description")?;
        let ends_now = pin_end_now(existing, update, now);
        let starts = update.starts_at.unwrap_or(existing.starts_at);
        let ends = update.ends_at.unwrap_or(existing.ends_at);
        validate_time_range(starts, ends)?;
        if update.ends_at.is_some() && !ends_now {
            require_future_end(ends)?;
        }
        if let Some(ids) = update.component_ids.as_deref() {
            self.validate_component_ids(org, ids).await?;
        }
        Ok(())
    }

    pub async fn cancel(
        &self,
        org: OrgId,
        id: Uuid,
        source: WriteSource,
        actor: Option<UserId>,
    ) -> Result<()> {
        if self.store.delete(org, id, source, actor).await? {
            return Ok(());
        }
        if let Some(window) = self.store.get(org, id).await? {
            self.vet_cancel(&window, Utc::now())?;
        }
        Err(AppError::not_found(
            codes::MAINTENANCE_NOT_FOUND,
            "maintenance window not found",
        ))
    }

    pub fn vet_cancel(&self, window: &MaintenanceWindow, now: DateTime<Utc>) -> Result<()> {
        match window.phase(now) {
            WindowPhase::Cancelled => Err(AppError::not_found(
                codes::MAINTENANCE_NOT_FOUND,
                "maintenance window is already cancelled",
            )),
            WindowPhase::Completed => Err(AppError::unprocessable(
                codes::MAINTENANCE_COMPLETED,
                "cannot cancel a completed maintenance window",
            )),
            WindowPhase::Upcoming | WindowPhase::Active => Ok(()),
        }
    }

    async fn validate_component_ids(&self, org: OrgId, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        // The store would hit the component primary key on a repeat.
        if let Some(i) = ids
            .iter()
            .enumerate()
            .position(|(i, id)| ids[..i].contains(id))
        {
            return Err(AppError::bad_request_field(
                codes::INVALID_COMPONENT_ID,
                format!("component id {} is listed twice", ids[i]),
                "component_ids",
            ));
        }
        let known = self.store.existing_target_ids(org, ids).await?;
        let unknown = ids.iter().filter(|id| !known.contains(id)).count();
        if unknown > 0 {
            return Err(AppError::bad_request_field(
                codes::INVALID_COMPONENT_ID,
                format!("{unknown} component id(s) do not exist"),
                "component_ids",
            ));
        }
        Ok(())
    }
}

/// A completed or cancelled window is history, so it cannot be edited.
fn reject_closed(phase: WindowPhase) -> Result<()> {
    match phase {
        WindowPhase::Cancelled => Err(AppError::unprocessable(
            codes::MAINTENANCE_CANCELLED,
            "cannot edit a cancelled maintenance window",
        )),
        WindowPhase::Completed => Err(AppError::unprocessable(
            codes::MAINTENANCE_COMPLETED,
            "cannot edit a completed maintenance window",
        )),
        WindowPhase::Upcoming | WindowPhase::Active => Ok(()),
    }
}

/// An `ends_at` at or before `now` on a running window whose start is left
/// alone means "stop now": it is replaced by the server's clock instead of the
/// client's. Returns whether it did.
fn pin_end_now(
    existing: &MaintenanceWindow,
    update: &mut MaintenanceWindowUpdate,
    now: DateTime<Utc>,
) -> bool {
    let ends_now = existing.phase(now) == WindowPhase::Active
        && update.ends_at.is_some_and(|ends| ends <= now)
        && update
            .starts_at
            .is_none_or(|starts| starts == existing.starts_at);
    if ends_now {
        update.ends_at = Some(now);
    }
    ends_now
}

fn validate_time_range(starts: DateTime<Utc>, ends: DateTime<Utc>) -> Result<()> {
    if ends <= starts {
        return Err(AppError::bad_request_field(
            codes::INVALID_TIME_RANGE,
            "ends_at must be strictly after starts_at",
            "ends_at",
        ));
    }
    if ends - starts > Duration::days(MAX_WINDOW_DAYS) {
        return Err(AppError::bad_request_field(
            codes::INVALID_DURATION,
            format!("maintenance window cannot exceed {MAX_WINDOW_DAYS} days"),
            "ends_at",
        ));
    }
    Ok(())
}

fn require_future_end(ends: DateTime<Utc>) -> Result<()> {
    if ends <= Utc::now() {
        return Err(AppError::bad_request_field(
            codes::INVALID_TIME_RANGE,
            "ends_at must be in the future",
            "ends_at",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_time_range_rejects_zero_duration() {
        let t = Utc::now();
        assert!(matches!(
            validate_time_range(t, t),
            Err(AppError::BadRequest { code, .. }) if code == codes::INVALID_TIME_RANGE
        ));
    }

    #[test]
    fn validate_time_range_rejects_too_long() {
        let s = Utc::now();
        let e = s + Duration::days(MAX_WINDOW_DAYS + 1);
        assert!(matches!(
            validate_time_range(s, e),
            Err(AppError::BadRequest { code, .. }) if code == codes::INVALID_DURATION
        ));
    }

    #[test]
    fn validate_time_range_accepts_normal_window() {
        let s = Utc::now();
        let e = s + Duration::hours(2);
        assert!(validate_time_range(s, e).is_ok());
    }
}
