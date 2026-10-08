//! The incidents list and its live-polled table.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{IncidentSeverity, IncidentState, OpsIncident, OrgId, UserId};
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::storage::{IncidentOpsFilter, IncidentSort};
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::views::{PageSizeLink, PagerLink};

use super::actors::{AckList, ack_list};
use super::{OwnerAvatar, OwnerOption, incident_label, member_avatar, members_map, state_label};

pub(super) const STATE_FILTERS: &[&str] = &["all", "triggered", "acknowledged", "resolved"];
const SEVERITIES: &[&str] = &["minor", "major", "critical"];
pub(super) const SORTS: &[(&str, &str)] = &[
    ("recent", "sort:recent"),
    ("oldest", "sort:oldest"),
    ("severity", "sort:severity"),
];
pub(super) const PAGE_SIZES: &[usize] = &[25, 50, 100, 200];
const DEFAULT_PAGE_SIZE: usize = 50;

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub state: Option<String>,
    pub severity: Option<String>,
    /// `me`, a member's user id, or absent (everyone's). Drives the owner filter.
    pub assignee: Option<String>,
    /// Free-text search over incident title + monitor name.
    pub q: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub struct SortOption {
    pub key: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

pub struct StateTab {
    pub label: &'static str,
    pub href: String,
    pub count: usize,
    pub active: bool,
}

pub struct SeverityChip {
    pub label: &'static str,
    pub href: String,
    pub active: bool,
}

pub struct ConsoleRow {
    pub id: String,
    pub target_id: Option<String>,
    pub label: String,
    pub state: &'static str,
    pub state_label: &'static str,
    /// Monitor check type; `None` for a manual incident (no monitor).
    pub kind: Option<&'static str>,
    /// The incident's monitor has since been deleted.
    pub monitor_deleted: bool,
    /// Whether a person can still reopen it.
    pub reopenable: bool,
    pub severity: &'static str,
    pub urgency: &'static str,
    pub origin: &'static str,
    pub visibility: &'static str,
    pub acks: AckList,
    /// Manual resolver; `None` on a resolved incident = writer auto-close.
    pub resolved_by: Option<OwnerAvatar>,
    pub assignee: Option<OwnerAvatar>,
    pub assigned_to_me: bool,
    pub started_at: DateTime<Utc>,
    /// Coarse age: elapsed for ongoing, lifetime for resolved. Refreshed by the
    /// 10s table poll, so no client-side ticking.
    pub age: String,
    pub ongoing: bool,
}

/// Row + pager state shared by the full console page and its live-polled table
/// fragment.
pub struct ConsoleData {
    pub rows: Vec<ConsoleRow>,
    /// Current user id, so a row can offer "assign to me".
    pub self_id: String,
    pub limit: usize,
    pub total: usize,
    pub page: usize,
    pub total_pages: usize,
    pub range_lo: usize,
    pub range_hi: usize,
    pub pager_prev: Option<PagerLink>,
    pub pager_next: Option<PagerLink>,
    pub page_sizes: Vec<PageSizeLink>,
    /// Query string (sans host) the table fragment re-polls itself with.
    pub partial_query: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/list.html")]
pub struct IncidentsConsolePage {
    pub active_tab: &'static str,
    pub state_tabs: Vec<StateTab>,
    pub severity_chips: Vec<SeverityChip>,
    pub sort_options: Vec<SortOption>,
    pub owner_options: Vec<OwnerOption>,
    pub search: String,
    /// Active state/severity, carried as hidden form fields so the search/sort/
    /// owner controls preserve them on submit.
    pub state_value: &'static str,
    pub severity_value: Option<&'static str>,
    pub total: usize,
    pub data: ConsoleData,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/_console_table.html")]
pub struct IncidentsConsoleTable {
    pub data: ConsoleData,
}

/// Coarse, jitter-free age — minute resolution, no seconds. The 10s table poll
/// keeps it current, so it never ticks per-second on the client.
fn fmt_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        "<1m".to_string()
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d {}h", s / 86_400, (s % 86_400) / 3600)
    }
}

pub(super) fn row_from(
    inc: OpsIncident,
    monitor_name: Option<String>,
    acks: AckList,
    resolved_by: Option<OwnerAvatar>,
    assignee: Option<OwnerAvatar>,
    assigned_to_me: bool,
) -> ConsoleRow {
    let label = incident_label(inc.title.clone(), monitor_name);
    let ongoing = inc.state.is_open();
    // Ongoing: elapsed since start. Resolved: total lifetime.
    let end = inc.ended_at.filter(|_| !ongoing).unwrap_or_else(Utc::now);
    let age = fmt_age((end - inc.started_at).num_seconds());
    ConsoleRow {
        id: inc.id.to_string(),
        target_id: inc.target_id.map(|t| t.to_string()),
        label,
        state: inc.state.as_db_str(),
        state_label: state_label(inc.state),
        kind: None,
        monitor_deleted: inc.monitor_deleted(),
        reopenable: inc.reopenable(),
        severity: inc.severity.as_db_str(),
        urgency: inc.urgency.as_db_str(),
        origin: inc.origin.as_db_str(),
        visibility: inc.visibility.as_db_str(),
        acks,
        resolved_by,
        assignee,
        assigned_to_me,
        started_at: inc.started_at,
        age,
        ongoing,
    }
}

fn parse_state(key: &str) -> Option<IncidentState> {
    match key {
        "triggered" => Some(IncidentState::Triggered),
        "acknowledged" => Some(IncidentState::Acknowledged),
        "resolved" => Some(IncidentState::Resolved),
        _ => None,
    }
}

/// Friendly check-type label for the console: the raw `CheckSpec::kind` with
/// the two-word kinds shortened (`tls_cert` → `tls`, `domain_expiry` → `domain`).
pub(super) fn kind_label(kind: &str) -> &'static str {
    match kind {
        "tcp" => "tcp",
        "ping" => "ping",
        "heartbeat" => "heartbeat",
        "manual" => "manual",
        "dns" => "dns",
        "tls_cert" => "tls",
        "domain_expiry" => "domain",
        "flow" => "flow",
        _ => "http",
    }
}

/// Active filter selection resolved from the raw query params.
struct Resolved {
    active: &'static str,
    state: Option<IncidentState>,
    severity_key: Option<&'static str>,
    severity: Option<IncidentSeverity>,
    /// Raw `assignee` param: `me`, a user-id string, or `None`. Owns both the
    /// owner dropdown selection and the URL value.
    assignee: Option<String>,
    query: Option<String>,
    sort: &'static str,
    limit: usize,
}

impl Resolved {
    fn mine(&self) -> bool {
        self.assignee.as_deref() == Some("me")
    }
    /// The selected owner id, when the assignee param is a member (not `me`).
    /// A malformed id (URL tampering) maps to the nil sentinel so the filter
    /// matches no incident, rather than silently falling back to "show all".
    fn owner_id(&self) -> Option<Uuid> {
        match self.assignee.as_deref() {
            Some("me") | None => None,
            Some(s) => Some(Uuid::parse_str(s).unwrap_or(Uuid::nil())),
        }
    }
}

fn resolve(params: &ListParams) -> Resolved {
    let active = STATE_FILTERS
        .iter()
        .copied()
        .find(|s| Some(*s) == params.state.as_deref())
        .unwrap_or("all");
    let severity_key = SEVERITIES
        .iter()
        .copied()
        .find(|s| Some(*s) == params.severity.as_deref());
    let sort = SORTS
        .iter()
        .map(|(k, _)| *k)
        .find(|k| Some(*k) == params.sort.as_deref())
        .unwrap_or("recent");
    Resolved {
        active,
        state: parse_state(active),
        severity_key,
        severity: severity_key.map(IncidentSeverity::from_db_str),
        assignee: params
            .assignee
            .as_deref()
            .map(str::to_owned)
            .filter(|s| !s.is_empty()),
        query: params
            .q
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        sort,
        limit: params
            .limit
            .filter(|n| PAGE_SIZES.contains(n))
            .unwrap_or(DEFAULT_PAGE_SIZE),
    }
}

/// Resolve the `assignee` filter param to a concrete user id: `me` → the
/// caller, a member id → that member, anything else → unfiltered.
fn assignee_filter(r: &Resolved, uid: UserId) -> Option<UserId> {
    if r.mine() {
        Some(uid)
    } else {
        r.owner_id().map(UserId)
    }
}

/// Canonical query string for a console URL (nav links + the table fragment's
/// self-poll), omitting defaults so URLs stay clean. `assignee` carries `me` or
/// a member id verbatim.
fn build_query(
    r: &Resolved,
    state: &str,
    severity: Option<&str>,
    limit: usize,
    offset: usize,
) -> String {
    use crate::auth::url::push_param;
    let mut q = String::new();
    push_param(&mut q, "state", state);
    push_param(&mut q, "limit", &limit.to_string());
    if let Some(s) = severity {
        push_param(&mut q, "severity", s);
    }
    if let Some(a) = r.assignee.as_deref() {
        push_param(&mut q, "assignee", a);
    }
    if let Some(query) = r.query.as_deref() {
        push_param(&mut q, "q", query);
    }
    if r.sort != "recent" {
        push_param(&mut q, "sort", r.sort);
    }
    if offset > 0 {
        push_param(&mut q, "offset", &offset.to_string());
    }
    q
}

/// Load the rows + pager shared by the full page and the table fragment.
async fn console_data(
    state: &AppState,
    org: OrgId,
    uid: UserId,
    params: &ListParams,
    members: &HashMap<UserId, String>,
) -> WebResult<ConsoleData> {
    let r = resolve(params);
    let base = IncidentOpsFilter {
        state: r.state,
        severity: r.severity,
        assignee: assignee_filter(&r, uid),
        query: r.query.clone(),
        sort: IncidentSort::from_key(r.sort),
        ..Default::default()
    };
    let total = state.incident_ops_store.count(org, &base).await?;
    let max_offset = if total == 0 {
        0
    } else {
        ((total - 1) / r.limit) * r.limit
    };
    // Snap a hand-typed offset to a page boundary, then clamp to the last
    // populated page, so the page number and prev/next links stay aligned.
    let offset = ((params.offset.unwrap_or(0) / r.limit) * r.limit).min(max_offset);

    let incidents = state
        .incident_ops_store
        .list(
            org,
            IncidentOpsFilter {
                limit: Some(r.limit),
                offset,
                ..base.clone()
            },
        )
        .await?;
    let ids: Vec<Uuid> = incidents.iter().map(|i| i.id).collect();
    let mut acks = state.incident_ops_store.acknowledgements(org, &ids).await?;
    let rows: Vec<ConsoleRow> = incidents
        .into_iter()
        .map(|i| {
            let avatar_of = |u: UserId| member_avatar(u, members);
            // The incident carries its monitor's name and kind, so a deleted
            // monitor's incident still reads as it did.
            let name = i.target_name.clone();
            let kind = i.target_kind.as_deref().map(kind_label);
            let incident_acks = acks.remove(&i.id).unwrap_or_default();
            let resolved = i.resolved_by.and_then(avatar_of);
            let assignee = i.assigned_to.and_then(avatar_of);
            let mine_row = i.assigned_to == Some(uid);
            let acks = ack_list(&incident_acks, uid, members);
            let mut row = row_from(i, name, acks, resolved, assignee, mine_row);
            row.kind = kind;
            row
        })
        .collect();

    let shown = rows.len();
    let total_pages = if total == 0 {
        1
    } else {
        total.div_ceil(r.limit)
    };
    let nav = |off: usize| {
        format!(
            "/incidents?{}",
            build_query(&r, r.active, r.severity_key, r.limit, off)
        )
    };
    Ok(ConsoleData {
        rows,
        self_id: uid.0.to_string(),
        limit: r.limit,
        total,
        page: offset / r.limit + 1,
        total_pages,
        range_lo: if total == 0 { 0 } else { offset + 1 },
        range_hi: offset + shown,
        pager_prev: (offset > 0).then(|| PagerLink {
            label: "prev",
            href: nav(offset.saturating_sub(r.limit)),
            hx_get: None,
        }),
        pager_next: (offset + r.limit < total).then(|| PagerLink {
            label: "next",
            href: nav(offset + r.limit),
            hx_get: None,
        }),
        page_sizes: PAGE_SIZES
            .iter()
            .copied()
            .map(|n| PageSizeLink {
                n,
                href: format!(
                    "/incidents?{}",
                    build_query(&r, r.active, r.severity_key, n, 0)
                ),
                hx_get: None,
                active: n == r.limit,
            })
            .collect(),
        partial_query: build_query(&r, r.active, r.severity_key, r.limit, offset),
    })
}

pub async fn list(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(uid): CurrentUser,
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> WebResult<IncidentsConsolePage> {
    let r = resolve(&params);
    let members = members_map(&state, org).await?;
    let data = console_data(&state, org, uid, &params, &members).await?;

    // Tab counts honour the active severity + assignee + search, across states.
    let count_filter = IncidentOpsFilter {
        severity: r.severity,
        assignee: assignee_filter(&r, uid),
        query: r.query.clone(),
        ..Default::default()
    };
    let counts = state
        .incident_ops_store
        .counts_by_state(org, &count_filter)
        .await?;

    let state_tabs = STATE_FILTERS
        .iter()
        .copied()
        .map(|k| StateTab {
            label: k,
            href: format!(
                "/incidents?{}",
                build_query(&r, k, r.severity_key, r.limit, 0)
            ),
            count: counts.for_state(parse_state(k)),
            active: k == r.active,
        })
        .collect();

    let sev_href =
        |sev: Option<&str>| format!("/incidents?{}", build_query(&r, r.active, sev, r.limit, 0));
    let mut severity_chips = vec![SeverityChip {
        label: "any",
        href: sev_href(None),
        active: r.severity_key.is_none(),
    }];
    severity_chips.extend(SEVERITIES.iter().copied().map(|s| SeverityChip {
        label: s,
        href: sev_href(Some(s)),
        active: r.severity_key == Some(s),
    }));

    let sort_options = SORTS
        .iter()
        .map(|(key, label)| SortOption {
            key,
            label,
            selected: *key == r.sort,
        })
        .collect();

    let mut owner_options = vec![
        OwnerOption {
            value: String::new(),
            label: "owner:any".into(),
            selected: r.assignee.is_none(),
        },
        OwnerOption {
            value: "me".into(),
            label: "owner:me".into(),
            selected: r.mine(),
        },
    ];
    let mut members: Vec<(UserId, String)> = members.into_iter().collect();
    members.sort_by(|a, b| a.1.cmp(&b.1));
    let owner_id = r.owner_id();
    owner_options.extend(members.into_iter().map(|(uid, email)| OwnerOption {
        selected: owner_id == Some(uid.0),
        value: uid.0.to_string(),
        label: format!("owner:{email}"),
    }));

    Ok(IncidentsConsolePage {
        active_tab: "incidents",
        state_tabs,
        severity_chips,
        sort_options,
        owner_options,
        search: r.query.clone().unwrap_or_default(),
        state_value: r.active,
        severity_value: r.severity_key,
        total: data.total,
        data,
    })
}

pub async fn list_partial(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(uid): CurrentUser,
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> WebResult<IncidentsConsoleTable> {
    let members = members_map(&state, org).await?;
    Ok(IncidentsConsoleTable {
        data: console_data(&state, org, uid, &params, &members).await?,
    })
}
