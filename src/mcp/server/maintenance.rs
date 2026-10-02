//! Maintenance window bodies: list and read, schedule, edit, cancel.
//!
//! No audit here: the wrappers in [`super::tools_write`] record the outcome.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Timelike, Utc};
use rmcp::RoleServer;
use rmcp::handler::server::wrapper::Json;
use rmcp::service::RequestContext;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::scope::Scope;
use crate::domain::{
    MaintenanceFilter, MaintenanceWindow, MaintenanceWindowUpdate, NewMaintenanceWindow, OrgId,
    WindowPhase, WriteSource,
};
use crate::error::AppError;
use crate::quotas::ratelimit::RateLimitCategory;
use crate::storage::MaintenanceListQuery;
use crate::templates::format::humanize_duration;

use crate::mcp::auth::McpAuth;
use crate::mcp::confirm::require_confirmation;
use crate::mcp::cursor;
use crate::mcp::error::{McpToolError, codes, config_error};
use crate::mcp::schema::{
    CreateMaintenanceArgs, FieldChange, ListMaintenanceArgs, MaintenanceIdArg, MaintenanceList,
    MaintenanceMonitor, MaintenanceStatus, MaintenanceUpdateResult, MaintenanceWindowView,
    UpdateMaintenanceArgs,
};

use super::McpServer;
use super::args::{parse_rfc3339, parse_uuid};
use super::text::{change_lines, sanitize_data, sanitize_prompt};
use super::tools_read::PAGE_SIZE;

pub(super) type MonitorNames = HashMap<Uuid, String>;

/// The query behind one `list_maintenance` page, carried whole in the cursor.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct MaintenancePage {
    pub(super) status: MaintenanceStatus,
    pub(super) offset: u32,
}

impl From<MaintenanceStatus> for MaintenanceFilter {
    fn from(status: MaintenanceStatus) -> Self {
        match status {
            MaintenanceStatus::Active => Self::Active,
            MaintenanceStatus::Upcoming => Self::Upcoming,
            MaintenanceStatus::Past => Self::Past,
            MaintenanceStatus::All => Self::All,
        }
    }
}

impl McpServer {
    pub(super) async fn list_maintenance_inner(
        &self,
        auth: &McpAuth,
        args: &ListMaintenanceArgs,
    ) -> Result<Json<MaintenanceList>, McpToolError> {
        auth.require(Scope::MaintenanceRead)?;
        let page = match args.cursor.as_deref() {
            Some(c) => cursor::decode_query::<MaintenancePage>(c)
                .ok_or_else(|| McpToolError::invalid_argument("invalid cursor"))?,
            None => MaintenancePage {
                status: args.status.unwrap_or_default(),
                offset: 0,
            },
        };
        let page_size = PAGE_SIZE as u32;
        let mut windows = self
            .state
            .maintenance_store
            .list(
                auth.org,
                MaintenanceListQuery {
                    filter: page.status.into(),
                    limit: page_size + 1,
                    offset: page.offset,
                },
            )
            .await
            .map_err(|e| McpToolError::internal(format!("list maintenance: {e}")))?;
        let next_cursor = (windows.len() > PAGE_SIZE)
            .then(|| {
                cursor::encode_query(&MaintenancePage {
                    status: page.status,
                    offset: page.offset.saturating_add(page_size),
                })
            })
            .flatten();
        windows.truncate(PAGE_SIZE);

        let names = self.monitor_names(auth.org).await?;
        let now = Utc::now();
        Ok(Json(MaintenanceList {
            items: windows
                .iter()
                .map(|w| window_view(w, &names, now))
                .collect(),
            next_cursor,
        }))
    }

    pub(super) async fn get_maintenance_inner(
        &self,
        auth: &McpAuth,
        args: &MaintenanceIdArg,
    ) -> Result<Json<MaintenanceWindowView>, McpToolError> {
        auth.require(Scope::MaintenanceRead)?;
        let window = self.load_window(auth.org, &args.id).await?;
        let names = self.monitor_names(auth.org).await?;
        Ok(Json(window_view(&window, &names, Utc::now())))
    }

    pub(super) async fn create_maintenance_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &CreateMaintenanceArgs,
    ) -> Result<Json<MaintenanceWindowView>, McpToolError> {
        auth.require(Scope::MaintenanceWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;

        let names = self.monitor_names(auth.org).await?;
        let new = NewMaintenanceWindow {
            title: args.title.clone(),
            description: args.description.clone(),
            starts_at: parse_rfc3339(&args.starts_at, "starts_at")?,
            ends_at: parse_rfc3339(&args.ends_at, "ends_at")?,
            component_ids: monitor_set(&args.monitor_ids, &names)?,
            suppress_alerts: args.suppress_alerts.unwrap_or(true),
        };
        self.state
            .maintenance_ops()
            .vet_new(auth.org, &new)
            .await
            .map_err(window_error)?;
        let published = self.published_monitors(auth.org).await?;

        require_confirmation(
            ctx,
            auth,
            schedule_prompt(&new, &names, &published, Utc::now()),
        )
        .await?;

        // Vets again: the prompt can stay open past the window's end or the plan's last slot.
        let window = self
            .state
            .maintenance_ops()
            .create(auth.org, new, WriteSource::Api, Some(auth.user_id))
            .await
            .map_err(window_error)?;
        Ok(Json(window_view(&window, &names, Utc::now())))
    }

    pub(super) async fn update_maintenance_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &UpdateMaintenanceArgs,
    ) -> Result<Json<MaintenanceUpdateResult>, McpToolError> {
        auth.require(Scope::MaintenanceWrite)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;

        let existing = self.load_window(auth.org, &args.id).await?;
        let names = self.monitor_names(auth.org).await?;
        let (proposed, changes) = self.vetted_patch(auth.org, args, &existing, &names).await?;
        if changes.is_empty() {
            return Ok(Json(MaintenanceUpdateResult {
                window: window_view(&existing, &names, Utc::now()),
                changes,
            }));
        }
        let published = match proposed.component_ids {
            Some(_) => self.published_monitors(auth.org).await?,
            None => HashSet::new(),
        };

        require_confirmation(
            ctx,
            auth,
            edit_prompt(&existing, &proposed, &changes, &names, &published),
        )
        .await?;

        // The window can move, start or end while a human reads the prompt.
        let current = self.load_window(auth.org, &args.id).await?;
        if current.updated_at != existing.updated_at {
            return Err(moved_meanwhile());
        }
        let (update, still) = build_window_patch(args, &current, &names, Utc::now())?;
        if still != changes {
            return Err(moved_meanwhile());
        }

        let window = self
            .state
            .maintenance_ops()
            .update(
                auth.org,
                &current,
                update,
                WriteSource::Api,
                Some(auth.user_id),
            )
            .await
            .map_err(window_error)?;
        Ok(Json(MaintenanceUpdateResult {
            window: window_view(&window, &names, Utc::now()),
            changes,
        }))
    }

    pub(super) async fn cancel_maintenance_inner(
        &self,
        ctx: &RequestContext<RoleServer>,
        auth: &McpAuth,
        args: &MaintenanceIdArg,
    ) -> Result<Json<MaintenanceWindowView>, McpToolError> {
        auth.require(Scope::MaintenanceDelete)?;
        self.enforce_rate_limit(auth.org, RateLimitCategory::ApiWrites)
            .await?;

        let window = self.load_window(auth.org, &args.id).await?;
        let now = Utc::now();
        self.state
            .maintenance_ops()
            .vet_cancel(&window, now)
            .map_err(window_error)?;

        let names = self.monitor_names(auth.org).await?;
        let prompt = cancel_prompt(&window, &names, now);
        require_confirmation(ctx, auth, prompt.clone()).await?;

        let mut current = self.load_window(auth.org, &args.id).await?;
        if current.updated_at != window.updated_at
            || cancel_prompt(&current, &names, Utc::now()) != prompt
        {
            return Err(moved_meanwhile());
        }
        self.state
            .maintenance_ops()
            .cancel(auth.org, current.id, WriteSource::Api, Some(auth.user_id))
            .await
            .map_err(window_error)?;
        current.deleted_at = Some(Utc::now());
        Ok(Json(window_view(&current, &names, Utc::now())))
    }

    async fn vetted_patch(
        &self,
        org: OrgId,
        args: &UpdateMaintenanceArgs,
        existing: &MaintenanceWindow,
        names: &MonitorNames,
    ) -> Result<(MaintenanceWindowUpdate, Vec<FieldChange>), McpToolError> {
        let now = Utc::now();
        let (mut update, changes) = build_window_patch(args, existing, names, now)?;
        self.state
            .maintenance_ops()
            .vet_update(org, existing, &mut update, now)
            .await
            .map_err(window_error)?;
        Ok((update, changes))
    }

    async fn load_window(&self, org: OrgId, id: &str) -> Result<MaintenanceWindow, McpToolError> {
        let id = parse_uuid(id, "maintenance window id")?;
        self.state
            .maintenance_store
            .get(org, id)
            .await
            .map_err(|e| McpToolError::internal(format!("get maintenance: {e}")))?
            .ok_or_else(|| McpToolError::not_found("maintenance window not found"))
    }

    async fn published_monitors(&self, org: OrgId) -> Result<HashSet<Uuid>, McpToolError> {
        self.state
            .status_page_store
            .published_target_ids(org)
            .await
            .map_err(|e| McpToolError::internal(format!("published monitors: {e}")))
    }

    async fn monitor_names(&self, org: OrgId) -> Result<MonitorNames, McpToolError> {
        self.state
            .target_store
            .names(org)
            .await
            .map_err(|e| McpToolError::internal(format!("monitor names: {e}")))
    }
}

fn moved_meanwhile() -> McpToolError {
    McpToolError::new(
        codes::CONFLICT,
        "maintenance window changed while this was being confirmed; read it again and retry",
        true,
    )
}

fn window_error(e: AppError) -> McpToolError {
    match e {
        AppError::NotFound { message, .. } => McpToolError::not_found(message),
        other => config_error(other),
    }
}

pub(super) fn window_view(
    w: &MaintenanceWindow,
    names: &MonitorNames,
    now: DateTime<Utc>,
) -> MaintenanceWindowView {
    let mut monitors: Vec<MaintenanceMonitor> = w
        .component_ids
        .iter()
        .map(|id| MaintenanceMonitor {
            id: id.to_string(),
            name: names.get(id).map(|n| sanitize_data(n)).unwrap_or_default(),
        })
        .collect();
    monitors.sort_by_key(|m| m.name.to_lowercase());
    MaintenanceWindowView {
        id: w.id.to_string(),
        title: sanitize_data(&w.title),
        description: w
            .description
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map(sanitize_data),
        phase: w.phase(now).as_str().to_string(),
        starts_at: w.starts_at.to_rfc3339(),
        ends_at: w.ends_at.to_rfc3339(),
        suppress_alerts: w.suppress_alerts,
        monitors,
        cancelled_at: w.deleted_at.map(|t| t.to_rfc3339()),
    }
}

/// Monitor ids to the org's own monitors, refusing a foreign or repeated one.
pub(super) fn monitor_set(ids: &[String], names: &MonitorNames) -> Result<Vec<Uuid>, McpToolError> {
    if ids.is_empty() {
        return Err(McpToolError::invalid_argument(
            "pass at least one monitor id (from list_monitors)",
        ));
    }
    let mut set: Vec<Uuid> = Vec::with_capacity(ids.len());
    for raw in ids {
        let id = parse_uuid(raw, "monitor id")?;
        if !names.contains_key(&id) {
            return Err(McpToolError::invalid_argument(format!(
                "no monitor {id} in this organization"
            )));
        }
        if set.contains(&id) {
            return Err(McpToolError::invalid_argument(format!(
                "monitor {id} is listed twice"
            )));
        }
        set.push(id);
    }
    Ok(set)
}

/// A value equal to the stored one is not a change.
pub(super) fn build_window_patch(
    args: &UpdateMaintenanceArgs,
    existing: &MaintenanceWindow,
    names: &MonitorNames,
    now: DateTime<Utc>,
) -> Result<(MaintenanceWindowUpdate, Vec<FieldChange>), McpToolError> {
    let mut update = MaintenanceWindowUpdate::default();
    let mut changes = Vec::new();
    let mut moved = |field: &str, from: String, to: String| {
        changes.push(FieldChange {
            field: field.to_string(),
            from,
            to,
        });
    };

    if let Some(title) = args.title.as_ref().filter(|t| **t != existing.title) {
        moved(
            "title",
            sanitize_data(&existing.title),
            sanitize_data(title),
        );
        update.title = Some(title.clone());
    }
    let stored_description = existing.description.as_deref().unwrap_or_default();
    if let Some(description) = args
        .description
        .as_ref()
        .filter(|d| d.as_str() != stored_description)
    {
        moved(
            "description",
            sanitize_data(stored_description),
            sanitize_data(description),
        );
        update.description = Some(description.clone());
    }
    if let Some(raw) = &args.starts_at {
        let starts = parse_rfc3339(raw, "starts_at")?;
        if starts != existing.starts_at {
            moved("starts_at", when(existing.starts_at), when(starts));
            update.starts_at = Some(starts);
        }
    }
    if args.end_now == Some(true) {
        if args.starts_at.is_some() || args.ends_at.is_some() {
            return Err(McpToolError::invalid_argument(
                "end_now cannot be combined with starts_at or ends_at",
            ));
        }
        if existing.phase(now) == WindowPhase::Upcoming {
            return Err(McpToolError::invalid_argument(
                "this window has not started, so there is nothing to end; cancel_maintenance \
                 withdraws it",
            ));
        }
        moved("ends_at", when(existing.ends_at), "now".to_string());
        update.ends_at = Some(now);
    }
    if let Some(raw) = &args.ends_at {
        let ends = parse_rfc3339(raw, "ends_at")?;
        if ends <= now {
            return Err(McpToolError::invalid_argument(
                "ends_at must be in the future; send end_now to end a running window now",
            ));
        }
        if ends != existing.ends_at {
            moved("ends_at", when(existing.ends_at), when(ends));
            update.ends_at = Some(ends);
        }
    }
    if let Some(ids) = &args.monitor_ids {
        let set = monitor_set(ids, names)?;
        let stored: HashSet<&Uuid> = existing.component_ids.iter().collect();
        if set.iter().collect::<HashSet<_>>() != stored {
            moved(
                "monitor_ids",
                monitor_list(&existing.component_ids, names),
                monitor_list(&set, names),
            );
            update.component_ids = Some(set);
        }
    }
    if let Some(hold) = args
        .suppress_alerts
        .filter(|h| *h != existing.suppress_alerts)
    {
        moved(
            "suppress_alerts",
            yes_no(existing.suppress_alerts),
            yes_no(hold),
        );
        update.suppress_alerts = Some(hold);
    }
    Ok((update, changes))
}

pub(super) fn schedule_prompt(
    new: &NewMaintenanceWindow,
    names: &MonitorNames,
    published: &HashSet<Uuid>,
    now: DateTime<Utc>,
) -> String {
    let start = if new.starts_at > now {
        format!("starts in {}", humanize_duration(new.starts_at - now))
    } else {
        "starts at once".to_string()
    };
    let mut lines = vec![
        format!(
            "{} to {} ({}, {start})",
            when(new.starts_at),
            when(new.ends_at),
            humanize_duration(new.ends_at - new.starts_at)
        ),
        format!("monitors: {}", monitor_list(&new.component_ids, names)),
        if new.suppress_alerts {
            "paging: held, so these monitors page nobody until it ends".to_string()
        } else {
            "paging: not held, so these monitors alert as usual".to_string()
        },
    ];
    let shown: Vec<Uuid> = new
        .component_ids
        .iter()
        .copied()
        .filter(|id| published.contains(id))
        .collect();
    lines.push(if shown.is_empty() {
        "public: none of these monitors is on a published status page".to_string()
    } else {
        format!(
            "public: shown on your status pages for {}; their subscribers are notified now and \
             when it ends",
            monitor_list(&shown, names)
        )
    });
    if let Some(d) = new.description.as_deref().filter(|d| !d.trim().is_empty()) {
        lines.push(format!("description: {}", sanitize_prompt(d)));
    }
    format!(
        "Schedule maintenance \"{}\"?\n\n{}",
        sanitize_prompt(&new.title),
        lines.join("\n")
    )
}

/// Monitor changes are spelled out as additions and removals, since a whole
/// list is capped in the prompt and could hide the one that moved.
pub(super) fn edit_prompt(
    existing: &MaintenanceWindow,
    proposed: &MaintenanceWindowUpdate,
    changes: &[FieldChange],
    names: &MonitorNames,
    published: &HashSet<Uuid>,
) -> String {
    let fields: Vec<FieldChange> = changes
        .iter()
        .filter(|c| c.field != "monitor_ids")
        .cloned()
        .collect();
    let mut lines = Vec::new();
    if !fields.is_empty() {
        lines.push(change_lines(&fields));
    }
    if let Some(ids) = proposed.component_ids.as_deref() {
        let added: Vec<Uuid> = ids
            .iter()
            .copied()
            .filter(|id| !existing.component_ids.contains(id))
            .collect();
        let removed: Vec<Uuid> = existing
            .component_ids
            .iter()
            .copied()
            .filter(|id| !ids.contains(id))
            .collect();
        let shown: Vec<Uuid> = added
            .iter()
            .copied()
            .filter(|id| published.contains(id))
            .collect();
        if !added.is_empty() {
            lines.push(format!("monitors added: {}", monitor_list(&added, names)));
        }
        if !removed.is_empty() {
            lines.push(format!(
                "monitors removed: {}",
                monitor_list(&removed, names)
            ));
        }
        if !shown.is_empty() {
            lines.push(format!(
                "public: now also shown on your status pages for {}, whose subscribers may be \
                 notified",
                monitor_list(&shown, names)
            ));
        }
    }
    format!(
        "Change maintenance \"{}\"?\n\n{}",
        sanitize_prompt(&existing.title),
        lines.join("\n")
    )
}

pub(super) fn cancel_prompt(
    w: &MaintenanceWindow,
    names: &MonitorNames,
    now: DateTime<Utc>,
) -> String {
    let phase = w.phase(now);
    let paging = if phase == WindowPhase::Active && w.suppress_alerts {
        " Its monitors page again unless another window holds them."
    } else {
        ""
    };
    format!(
        "Cancel maintenance \"{}\" ({}, {} to {})?\n\nmonitors: {}\n\nIt is no longer shown on \
         your status pages and stays under past as a record. Subscribers who were already told \
         about it are not sent a cancellation.{paging}",
        sanitize_prompt(&w.title),
        phase.as_str(),
        when(w.starts_at),
        when(w.ends_at),
        monitor_list(&w.component_ids, names),
    )
}

fn when(t: DateTime<Utc>) -> String {
    let format = if t.second() == 0 {
        "%Y-%m-%d %H:%M UTC"
    } else {
        "%Y-%m-%d %H:%M:%S UTC"
    };
    t.format(format).to_string()
}

fn monitor_list(ids: &[Uuid], names: &MonitorNames) -> String {
    let mut listed: Vec<String> = ids
        .iter()
        .map(|id| match names.get(id) {
            Some(name) => sanitize_prompt(name),
            None => id.to_string(),
        })
        .collect();
    listed.sort_by_key(|n| n.to_lowercase());
    if listed.is_empty() {
        "none".to_string()
    } else {
        listed.join(", ")
    }
}

fn yes_no(b: bool) -> String {
    if b { "yes" } else { "no" }.to_string()
}
