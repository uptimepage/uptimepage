//! Server-rendered maintenance pages under `/maintenance`: the windows that
//! have not ended (active and upcoming), a paged history of completed ones,
//! and a create/edit form.
//!
//! Mutations run from the page against the JSON API (`/api/v1/maintenance`),
//! so this module only renders chrome and prefills the form. A completed
//! window is read-only history, matching the API's refusal to edit one.

use std::collections::{HashMap, HashSet};

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{MaintenanceFilter, MaintenanceWindow, OrgId, UserId};
use crate::error::AppError;
use crate::request::CurrentOrg;
use crate::storage::MaintenanceListQuery;
use crate::templates::filters;
use crate::web::error::{NotFoundPage, WebResult};
use crate::web::views::incidents::members_map;
use crate::web::views::pages::kind_label;
use crate::web::views::{PageSizeLink, PagerLink, resolve_org};

const TAB_MAINTENANCE: &str = "maintenance";
const OPEN_LIMIT: u32 = 200;
const PAGE_SIZES: &[usize] = &[25, 50, 100];
const SHOWN_MONITORS: usize = 3;

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    view: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

pub struct WindowRow {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    /// `active`, `upcoming`, `completed` or `cancelled`.
    pub phase: &'static str,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub monitors: Vec<String>,
    pub more_monitors: usize,
    pub suppress_alerts: bool,
    pub managed_by: Option<&'static str>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub cancelled_by: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "maintenance/list.html")]
pub struct MaintenancePage {
    pub active_tab: &'static str,
    pub past: bool,
    pub rows: Vec<WindowRow>,
    pub page: usize,
    pub page_sizes: Vec<PageSizeLink>,
    pub pager_prev: Option<PagerLink>,
    pub pager_next: Option<PagerLink>,
}

pub struct MonitorOption {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub published: bool,
    pub selected: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "maintenance/form.html")]
pub struct MaintenanceForm {
    pub active_tab: &'static str,
    pub mode: &'static str,
    pub action: String,
    pub method: &'static str,
    pub title: String,
    pub description: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub suppress_alerts: bool,
    pub monitors: Vec<MonitorOption>,
}

fn clamp_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn past_href(limit: usize, offset: usize) -> String {
    format!("/maintenance?view=past&limit={limit}&offset={offset}")
}

fn window_row(
    w: MaintenanceWindow,
    names: &HashMap<Uuid, String>,
    members: &HashMap<UserId, String>,
    now: DateTime<Utc>,
) -> WindowRow {
    let phase = w.phase(now).as_str();
    let mut monitors: Vec<&str> = w
        .component_ids
        .iter()
        .filter_map(|id| names.get(id).map(String::as_str))
        .collect();
    monitors.sort_by_key(|n| n.to_lowercase());
    let more_monitors = monitors.len().saturating_sub(SHOWN_MONITORS);
    monitors.truncate(SHOWN_MONITORS);
    WindowRow {
        id: w.id.to_string(),
        title: w.title,
        description: w.description.filter(|d| !d.trim().is_empty()),
        phase,
        starts_at: w.starts_at,
        ends_at: w.ends_at,
        monitors: monitors.into_iter().map(str::to_owned).collect(),
        more_monitors,
        suppress_alerts: w.suppress_alerts,
        managed_by: w.write_source.managed_label(),
        cancelled_at: w.deleted_at,
        cancelled_by: w.deleted_by.and_then(|u| members.get(&u).cloned()),
    }
}

pub async fn index(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Query(params): Query<ListParams>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/maintenance") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let past = params.view.as_deref() == Some("past");
    let limit = params
        .limit
        .filter(|n| PAGE_SIZES.contains(n))
        .unwrap_or(PAGE_SIZES[0]);
    let offset = params.offset.unwrap_or(0);
    let store = &state.maintenance_store;

    let (windows, has_next) = if past {
        let mut found = store
            .list(
                org,
                MaintenanceListQuery {
                    filter: MaintenanceFilter::Past,
                    limit: clamp_u32(limit + 1),
                    offset: clamp_u32(offset),
                },
            )
            .await?;
        let has_next = found.len() > limit;
        found.truncate(limit);
        (found, has_next)
    } else {
        let mut open = Vec::new();
        for filter in [MaintenanceFilter::Active, MaintenanceFilter::Upcoming] {
            open.extend(
                store
                    .list(
                        org,
                        MaintenanceListQuery {
                            filter,
                            limit: OPEN_LIMIT,
                            offset: 0,
                        },
                    )
                    .await?,
            );
        }
        open.sort_by_key(|w| w.starts_at);
        (open, false)
    };

    let names = state.target_store.names(org).await?;
    let members = if past {
        members_map(&state, org).await?
    } else {
        HashMap::new()
    };
    let now = Utc::now();
    Ok(MaintenancePage {
        active_tab: TAB_MAINTENANCE,
        past,
        rows: windows
            .into_iter()
            .map(|w| window_row(w, &names, &members, now))
            .collect(),
        page: offset / limit + 1,
        page_sizes: PAGE_SIZES
            .iter()
            .map(|&n| PageSizeLink {
                n,
                href: past_href(n, 0),
                hx_get: None,
                active: n == limit,
            })
            .collect(),
        pager_prev: (past && offset > 0).then(|| PagerLink {
            label: "newer",
            href: past_href(limit, offset.saturating_sub(limit)),
            hx_get: None,
        }),
        pager_next: has_next.then(|| PagerLink {
            label: "older",
            href: past_href(limit, offset + limit),
            hx_get: None,
        }),
    }
    .into_response())
}

async fn monitor_options(
    state: &AppState,
    org: OrgId,
    selected: &HashSet<Uuid>,
) -> WebResult<Vec<MonitorOption>> {
    let published = state.status_page_store.published_target_ids(org).await?;
    let mut options: Vec<MonitorOption> = state
        .target_store
        .names_and_kinds(org)
        .await?
        .into_iter()
        .map(|(id, (name, kind))| MonitorOption {
            id: id.to_string(),
            name,
            kind: kind_label(&kind),
            published: published.contains(&id),
            selected: selected.contains(&id),
        })
        .collect();
    options.sort_by_key(|m| m.name.to_lowercase());
    Ok(options)
}

pub async fn new_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/maintenance/new") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    Ok(MaintenanceForm {
        active_tab: TAB_MAINTENANCE,
        mode: "create",
        action: "/api/v1/maintenance".into(),
        method: "POST",
        title: String::new(),
        description: String::new(),
        starts_at: None,
        ends_at: None,
        suppress_alerts: true,
        monitors: monitor_options(&state, org, &HashSet::new()).await?,
    }
    .into_response())
}

pub async fn edit_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
) -> WebResult<Response> {
    let redirect_to = format!("/maintenance/{id}/edit");
    let org = match resolve_org(org, &redirect_to) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let Some(window) = state.maintenance_store.get(org, id).await? else {
        return Ok((
            axum::http::StatusCode::NOT_FOUND,
            NotFoundPage {
                active_tab: TAB_MAINTENANCE,
            },
        )
            .into_response());
    };
    if window.phase(Utc::now()).is_closed() {
        return Ok(Redirect::to("/maintenance?view=past").into_response());
    }
    let selected: HashSet<Uuid> = window.component_ids.iter().copied().collect();
    Ok(MaintenanceForm {
        active_tab: TAB_MAINTENANCE,
        mode: "edit",
        action: format!("/api/v1/maintenance/{id}"),
        method: "PATCH",
        title: window.title,
        description: window.description.unwrap_or_default(),
        starts_at: Some(window.starts_at),
        ends_at: Some(window.ends_at),
        suppress_alerts: window.suppress_alerts,
        monitors: monitor_options(&state, org, &selected).await?,
    }
    .into_response())
}
