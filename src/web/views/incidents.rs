//! Org-wide operator incidents console: a management surface for the
//! operational lifecycle (acknowledge / resolve / reopen / declare / note),
//! distinct from the per-monitor incident history under `/targets/{id}`.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{
    CheckResult, CheckStatus, IncidentEvent, IncidentOrigin, IncidentSeverity, IncidentState,
    IncidentStatusPhase, OpsIncident, OrgId, UserId,
};
use crate::error::AppError;
use crate::error::codes;
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::storage::orgs::list_members;
use crate::storage::{ClampedRange, IncidentOpsFilter, IncidentSort, TargetFilter, TimeRange};
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::{PageSizeLink, PagerLink};

const STATE_FILTERS: &[&str] = &["all", "triggered", "acknowledged", "resolved"];
const SEVERITIES: &[&str] = &["minor", "major", "critical"];
const SORTS: &[(&str, &str)] = &[
    ("recent", "sort:recent"),
    ("oldest", "sort:oldest"),
    ("severity", "sort:severity"),
];
const PAGE_SIZES: &[usize] = &[25, 50, 100, 200];
const DEFAULT_PAGE_SIZE: usize = 50;
/// Long enough to catch a slow interval's last check, short enough that a
/// long-paused monitor reads as no evidence rather than as recovered.
const RECOVERY_LOOKBACK: chrono::Duration = chrono::Duration::hours(24);
/// Rows pulled to find each region's latest check: more than one round across
/// every region a monitor can be probed from.
const RECOVERY_SAMPLE: usize = 60;

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

pub struct OwnerOption {
    pub value: String,
    pub label: String,
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

/// An incident owner rendered as a deterministic initials avatar.
pub struct OwnerAvatar {
    pub initials: String,
    pub color: String,
    pub label: String,
}

pub struct ConsoleRow {
    pub id: String,
    pub target_id: Option<String>,
    pub label: String,
    pub state: &'static str,
    pub state_label: &'static str,
    /// Monitor check type; `None` for a manual incident (no monitor).
    pub kind: Option<&'static str>,
    pub severity: &'static str,
    pub urgency: &'static str,
    pub origin: &'static str,
    pub visibility: &'static str,
    pub acked_by: Option<OwnerAvatar>,
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

fn state_label(s: IncidentState) -> &'static str {
    match s {
        IncidentState::Triggered => "triggered",
        IncidentState::Acknowledged => "acknowledged",
        IncidentState::Resolved => "resolved",
    }
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

fn row_from(
    inc: OpsIncident,
    monitor_name: Option<String>,
    acked_by: Option<OwnerAvatar>,
    resolved_by: Option<OwnerAvatar>,
    assignee: Option<OwnerAvatar>,
    assigned_to_me: bool,
) -> ConsoleRow {
    let label = inc
        .title
        .clone()
        .or(monitor_name)
        .unwrap_or_else(|| "Untitled incident".to_string());
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
        severity: inc.severity.as_db_str(),
        urgency: inc.urgency.as_db_str(),
        origin: inc.origin.as_db_str(),
        visibility: inc.visibility.as_db_str(),
        acked_by,
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

async fn name_map(state: &AppState, org: OrgId) -> WebResult<HashMap<Uuid, String>> {
    Ok(state.target_store.names(org).await?)
}

/// Friendly check-type label for the console: the raw `CheckSpec::kind` with
/// the two-word kinds shortened (`tls_cert` → `tls`, `domain_expiry` → `domain`).
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "tcp" => "tcp",
        "ping" => "ping",
        "heartbeat" => "heartbeat",
        "dns" => "dns",
        "tls_cert" => "tls",
        "domain_expiry" => "domain",
        "flow" => "flow",
        _ => "http",
    }
}

/// User id → email label for the org, for rendering "acknowledged by …".
async fn members_map(state: &AppState, org: OrgId) -> WebResult<HashMap<UserId, String>> {
    let Some(pool) = &state.db else {
        return Ok(HashMap::new());
    };
    let members = list_members(pool, org).await?;
    Ok(members
        .into_iter()
        .map(|m| (m.membership.user_id, m.email))
        .collect())
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
    // One lean projection (id, name, check kind) — no full target decode.
    let targets = state.target_store.names_and_kinds(org).await?;
    let rows: Vec<ConsoleRow> = incidents
        .into_iter()
        .map(|i| {
            let avatar_of = |u: UserId| {
                members.get(&u).map(|email| OwnerAvatar {
                    initials: crate::web::avatar::initials_from(email),
                    color: crate::web::avatar::avatar_color(u.0),
                    label: email.clone(),
                })
            };
            let name = i
                .target_id
                .and_then(|t| targets.get(&t).map(|(n, _)| n.clone()));
            let kind = i
                .target_id
                .and_then(|t| targets.get(&t).map(|(_, k)| kind_label(k)));
            let acked = i.acknowledged_by.and_then(avatar_of);
            let resolved = i.resolved_by.and_then(avatar_of);
            let assignee = i.assigned_to.and_then(avatar_of);
            let mine_row = i.assigned_to == Some(uid);
            let mut row = row_from(i, name, acked, resolved, assignee, mine_row);
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

pub struct TimelineRow {
    pub kind: &'static str,
    /// Who acted: a member's email, `system` for automated transitions, or
    /// `former member` when the actor has since left the org.
    pub who: String,
    /// The actor drove this through the MCP server rather than the console.
    pub via_mcp: bool,
    pub occurred_at: DateTime<Utc>,
    pub message: Option<String>,
}

/// A public `incident_updates` entry, distinct from the internal `TimelineRow`.
pub struct PublicUpdateRow {
    pub phase: &'static str,
    pub message: String,
    pub posted_at: DateTime<Utc>,
    /// Operator-facing only; never rendered on the public status page.
    pub author: String,
}

/// Stored author (user id / `system` / NULL) → label; a departed member reads
/// `former member`, not a raw id.
fn author_label(author: Option<&str>, members: &HashMap<UserId, String>) -> String {
    match author {
        None | Some("system") => "system".to_string(),
        Some(s) => match Uuid::parse_str(s) {
            Ok(u) => members
                .get(&UserId(u))
                .cloned()
                .unwrap_or_else(|| "former member".to_string()),
            Err(_) => s.to_string(),
        },
    }
}

/// Operator-facing update timeline — selects `author`, which the public
/// hydrate deliberately omits.
async fn public_update_rows(
    state: &AppState,
    org: OrgId,
    id: Uuid,
    members: &HashMap<UserId, String>,
) -> WebResult<Vec<PublicUpdateRow>> {
    let Some(pool) = &state.db else {
        return Ok(Vec::new());
    };
    let rows: Vec<(DateTime<Utc>, String, String, Option<String>)> = sqlx::query_as(
        "SELECT posted_at, phase, message, author FROM incident_updates \
         WHERE incident_id = $1 AND org_id = $2 ORDER BY posted_at ASC LIMIT $3",
    )
    .bind(id)
    .bind(org.0)
    .bind(crate::storage::incident_ops::INCIDENT_DETAIL_ROW_CAP)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Other(anyhow::anyhow!("load incident updates: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|(posted_at, phase, message, author)| PublicUpdateRow {
            phase: IncidentStatusPhase::from_db_str(&phase).as_db_str(),
            message,
            posted_at,
            author: author_label(author.as_deref(), members),
        })
        .collect())
}

/// Resolve an event's actor to a human label + whether it came via MCP.
fn actor_label(e: &IncidentEvent, members: &HashMap<UserId, String>) -> (String, bool) {
    use crate::domain::ActorType;
    match e.actor_type {
        ActorType::System => ("system".to_string(), false),
        // Whoever held the notification; the event message says which one.
        ActorType::Link => ("notification".to_string(), false),
        ActorType::User | ActorType::Mcp => {
            let who = match e.actor_id {
                Some(u) => members
                    .get(&u)
                    .cloned()
                    .unwrap_or_else(|| "former member".to_string()),
                None => "unknown".to_string(),
            };
            (who, e.actor_type == ActorType::Mcp)
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/detail.html")]
pub struct IncidentDetailPage {
    pub active_tab: &'static str,
    pub id: String,
    pub label: String,
    pub target_id: Option<String>,
    pub monitor_name: Option<String>,
    pub state: &'static str,
    pub state_label: &'static str,
    pub severity: &'static str,
    pub urgency: &'static str,
    pub origin: &'static str,
    pub visibility: &'static str,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub acknowledged_by: Option<String>,
    pub error_sample: Option<String>,
    pub ongoing: bool,
    pub timeline: Vec<TimelineRow>,
    pub public_updates: Vec<PublicUpdateRow>,
    pub owner: Option<OwnerAvatar>,
    /// `Unassigned` + each member.
    pub owner_options: Vec<OwnerOption>,
    pub has_postmortem: bool,
    pub postmortem_published: bool,
    /// Per-channel delivery log (the `incident_notifications` rows).
    pub notifications: Vec<NotificationRow>,
    /// Set when the incident is still open but its monitor is passing again.
    /// Nothing else in the product reconciles the two.
    pub monitor_recovered_at: Option<DateTime<Utc>>,
    /// Time open: elapsed so far while ongoing, total once it ended.
    pub duration_label: String,
    /// How long anyone took to take the page. `None` until acked.
    pub ack_delay_label: Option<String>,
    /// Bad checks folded into this incident. Zero for a hand-declared one.
    pub check_count: u64,
}

/// One paging-delivery row for the incident's notifications section.
pub struct NotificationRow {
    pub channel: String,
    pub transport: String,
    pub reason: &'static str,
    pub status: &'static str,
    pub status_label: &'static str,
    pub attempt: i32,
    pub error: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Failed with no retry scheduled — delivery gave up.
    pub dead_lettered: bool,
}

fn notification_status_label(s: crate::domain::NotificationStatus) -> &'static str {
    use crate::domain::NotificationStatus::*;
    match s {
        Queued => "queued",
        Sent => "sent",
        Failed => "failed",
        Suppressed => "suppressed",
    }
}

fn notification_row(
    n: &crate::domain::IncidentNotification,
    channel_names: &std::collections::HashMap<Uuid, String>,
) -> NotificationRow {
    use crate::domain::NotificationStatus;
    let channel = n
        .channel_id
        .map(|c| {
            channel_names
                .get(&c)
                .cloned()
                .unwrap_or_else(|| "(deleted channel)".to_string())
        })
        .unwrap_or_else(|| n.transport.clone());
    NotificationRow {
        channel,
        transport: n.transport.clone(),
        reason: n.reason.as_db_str(),
        status: n.status.as_db_str(),
        status_label: notification_status_label(n.status),
        attempt: n.attempt,
        error: n.error.clone(),
        sent_at: n.sent_at,
        next_attempt_at: n.next_attempt_at,
        dead_lettered: n.status == NotificationStatus::Failed && n.next_attempt_at.is_none(),
    }
}

fn event_kind_label(e: &IncidentEvent) -> &'static str {
    use crate::domain::IncidentEventKind::*;
    match e.kind {
        Triggered => "triggered",
        Acknowledged => "acknowledged",
        Assigned => "assigned",
        Unassigned => "unassigned",
        Escalated => "escalated",
        Notified => "notified",
        Note => "note",
        SeverityChanged => "severity changed",
        DowntimeChanged => "downtime accounting changed",
        StateChanged => "state changed",
        Resolved => "resolved",
        Reopened => "reopened",
        Published => "published",
        Unpublished => "unpublished",
        PostmortemPublished => "postmortem published",
        PostmortemUnpublished => "postmortem unpublished",
    }
}

pub async fn detail(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<IncidentDetailPage> {
    let inc = state
        .incident_ops_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let monitor_name = match inc.target_id {
        Some(t) => state.target_store.get(org, t).await?.map(|x| x.name),
        None => None,
    };
    let members = members_map(&state, org).await?;
    let acknowledged_by = inc.acknowledged_by.and_then(|u| members.get(&u).cloned());
    let events = state.incident_ops_store.timeline(org, id).await?;
    let timeline = events
        .iter()
        .map(|e| {
            let (who, via_mcp) = actor_label(e, &members);
            TimelineRow {
                kind: event_kind_label(e),
                who,
                via_mcp,
                occurred_at: e.occurred_at,
                message: e.message.clone(),
            }
        })
        .collect();
    let postmortem = state.postmortem_store.get(org, id).await?;
    let public_updates = public_update_rows(&state, org, id, &members).await?;

    let channel_names: std::collections::HashMap<Uuid, String> = state
        .notification_channel_store
        .list(org)
        .await?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect();
    let notifications = state
        .incident_ops_store
        .notifications_for(org, id)
        .await?
        .iter()
        // The damper's bookkeeping rows reached no channel; listing them as
        // deliveries invents channels named "damped" or "held". Keyed on the
        // marker, not `channel_id`, which a deleted channel NULLs on rows that
        // really were delivered.
        .filter(|n| !crate::escalation::is_damper_marker(&n.transport))
        .map(|n| notification_row(n, &channel_names))
        .collect();

    let assigned_to = inc.assigned_to;
    let owner = assigned_to.and_then(|u| {
        members.get(&u).map(|email| OwnerAvatar {
            initials: crate::web::avatar::initials_from(email),
            color: crate::web::avatar::avatar_color(u.0),
            label: email.clone(),
        })
    });
    let mut sorted: Vec<(UserId, String)> = members.into_iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1));
    let mut owner_options = vec![OwnerOption {
        value: String::new(),
        label: "Unassigned".into(),
        selected: assigned_to.is_none(),
    }];
    owner_options.extend(sorted.into_iter().map(|(u, email)| OwnerOption {
        selected: assigned_to == Some(u),
        value: u.0.to_string(),
        label: email,
    }));

    let recovered_at = monitor_recovered_at(&state, org, &inc).await;
    let label = inc
        .title
        .clone()
        .or_else(|| monitor_name.clone())
        .unwrap_or_else(|| "Untitled incident".to_string());
    let mut page = make_detail_page(
        inc,
        monitor_name,
        acknowledged_by,
        label,
        timeline,
        public_updates,
        postmortem.as_ref(),
    );
    page.owner = owner;
    page.owner_options = owner_options;
    page.notifications = notifications;
    page.monitor_recovered_at = recovered_at;
    Ok(page)
}

fn make_detail_page(
    inc: OpsIncident,
    monitor_name: Option<String>,
    acknowledged_by: Option<String>,
    label: String,
    timeline: Vec<TimelineRow>,
    public_updates: Vec<PublicUpdateRow>,
    postmortem: Option<&crate::domain::IncidentPostmortem>,
) -> IncidentDetailPage {
    let until = inc.ended_at.unwrap_or_else(Utc::now);
    let duration_label = fmt_secs(Some((until - inc.started_at).num_seconds().max(0) as f64))
        .unwrap_or_else(|| "0s".to_string());
    let ack_delay_label = inc
        .acknowledged_at
        .and_then(|at| fmt_secs(Some((at - inc.started_at).num_seconds().max(0) as f64)));
    let check_count = inc.check_count;
    IncidentDetailPage {
        active_tab: "incidents",
        id: inc.id.to_string(),
        label,
        target_id: inc.target_id.map(|t| t.to_string()),
        monitor_name,
        state: inc.state.as_db_str(),
        state_label: state_label(inc.state),
        severity: inc.severity.as_db_str(),
        urgency: inc.urgency.as_db_str(),
        origin: inc.origin.as_db_str(),
        visibility: inc.visibility.as_db_str(),
        started_at: inc.started_at,
        ended_at: inc.ended_at,
        acknowledged_at: inc.acknowledged_at,
        acknowledged_by,
        error_sample: inc.error_sample.clone(),
        ongoing: inc.state.is_open(),
        timeline,
        public_updates,
        owner: None,
        owner_options: Vec::new(),
        has_postmortem: postmortem.is_some(),
        postmortem_published: postmortem.is_some_and(|p| p.published_at.is_some()),
        notifications: Vec::new(),
        monitor_recovered_at: None,
        duration_label,
        ack_delay_label,
        check_count,
    }
}

/// The moment every region that reported had been passing by, if all of them
/// are. `None` for a standalone incident, a closed one, or a monitor any region
/// still calls bad.
///
/// Per region on purpose: the newest single row belongs to whichever agent
/// reported last, so a partial outage would read as a recovery about a third of
/// the time on a three-region monitor.
async fn monitor_recovered_at(
    state: &AppState,
    org: OrgId,
    inc: &OpsIncident,
) -> Option<DateTime<Utc>> {
    if !inc.state.is_open() {
        return None;
    }
    let target_id = inc.target_id?;
    let now = Utc::now();
    let range = ClampedRange::unclamped(TimeRange {
        from: now - RECOVERY_LOOKBACK,
        to: now,
    });
    let rows = state
        .results_store
        .list_results_by_region(org, target_id, range, RECOVERY_SAMPLE, 0)
        .await
        .ok()?;
    // Rows arrive newest first, so a region's first row is its latest check.
    let mut latest_per_region: HashMap<String, CheckResult> = HashMap::new();
    for (region, result) in rows {
        latest_per_region.entry(region).or_insert(result);
    }
    if latest_per_region.is_empty()
        || latest_per_region
            .values()
            .any(|r| r.status != CheckStatus::Up)
    {
        return None;
    }
    // The weakest claim the evidence supports.
    latest_per_region.values().map(|r| r.timestamp).min()
}

pub struct MonitorOption {
    pub id: String,
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/declare.html")]
pub struct DeclareIncidentPage {
    pub active_tab: &'static str,
    pub monitors: Vec<MonitorOption>,
}

pub async fn declare_form(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
) -> WebResult<DeclareIncidentPage> {
    let targets = state
        .target_store
        .list(
            org,
            TargetFilter {
                limit: Some(10_000),
                ..Default::default()
            },
        )
        .await?;
    let monitors = targets
        .into_iter()
        .map(|t| MonitorOption {
            id: t.id.to_string(),
            name: t.name,
        })
        .collect();
    Ok(DeclareIncidentPage {
        active_tab: "incidents",
        monitors,
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/edit.html")]
pub struct EditIncidentPage {
    pub active_tab: &'static str,
    pub id: String,
    pub title: String,
    /// Read-only: one monitor holds one open declaration, so rebinding would
    /// collide with the open-incident index.
    pub monitor_name: Option<String>,
    pub target_id: Option<String>,
    pub severity: &'static str,
    pub urgency: &'static str,
    pub visibility: &'static str,
    pub public_title: String,
    pub public_description: String,
    /// Manual origin and bound to a monitor: anything else has no uptime to move.
    pub downtime_editable: bool,
    pub counts_as_downtime: bool,
}

pub async fn edit_form(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<EditIncidentPage> {
    let inc = state
        .incident_ops_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let monitor_name = match inc.target_id {
        Some(t) => state.target_store.get(org, t).await?.map(|x| x.name),
        None => None,
    };
    // Separate read-model over the same row; the ops one carries no public copy.
    let narration = state.incident_narration_store.get(org, id).await?;
    Ok(EditIncidentPage {
        active_tab: "incidents",
        id: inc.id.to_string(),
        title: inc.title.clone().unwrap_or_default(),
        monitor_name,
        target_id: inc.target_id.map(|t| t.to_string()),
        severity: inc.severity.as_db_str(),
        urgency: inc.urgency.as_db_str(),
        visibility: inc.visibility.as_db_str(),
        public_title: narration
            .as_ref()
            .and_then(|n| n.public_title.clone())
            .unwrap_or_default(),
        public_description: narration
            .as_ref()
            .and_then(|n| n.public_description.clone())
            .unwrap_or_default(),
        downtime_editable: inc.origin == IncidentOrigin::Manual && inc.target_id.is_some(),
        counts_as_downtime: inc.counts_as_downtime,
    })
}

// ── Reports ──────────────────────────────────────────────────────────────

const WINDOW_DAYS: &[u32] = &[7, 30, 90];
const DEFAULT_WINDOW: u32 = 30;

#[derive(Debug, Default, Deserialize)]
pub struct ReportParams {
    pub window_days: Option<u32>,
}

pub struct WindowOption {
    pub days: u32,
    pub active: bool,
}

pub struct ReportBucket {
    pub label: String,
    pub count: u64,
}

pub struct ReportMonitorRow {
    pub id: String,
    pub name: String,
    pub count: u64,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/reports.html")]
pub struct IncidentsReportPage {
    pub active_tab: &'static str,
    pub window_days: u32,
    pub windows: Vec<WindowOption>,
    pub total: u64,
    pub mtta: Option<String>,
    pub mttr: Option<String>,
    pub by_severity: Vec<ReportBucket>,
    pub by_state: Vec<ReportBucket>,
    pub auto_resolved: u64,
    pub human_resolved: u64,
    pub top_monitors: Vec<ReportMonitorRow>,
}

/// Humanise a mean duration in seconds to a compact `1h 3m` / `5m 12s` / `8s`.
fn fmt_secs(secs: Option<f64>) -> Option<String> {
    let s = secs?.round().max(0.0) as u64;
    Some(if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    })
}

pub async fn reports(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Query(params): Query<ReportParams>,
) -> WebResult<IncidentsReportPage> {
    let window = params
        .window_days
        .filter(|d| WINDOW_DAYS.contains(d))
        .unwrap_or(DEFAULT_WINDOW);
    // 30s cache: a report view tolerates slight staleness and the aggregate
    // scans need not re-run on every load / window flip.
    let m = match state.incident_metrics_cache.get(&(org, window)) {
        Some(m) => m,
        None => {
            let m = state.incident_ops_store.metrics(org, window).await?;
            state
                .incident_metrics_cache
                .insert((org, window), m.clone());
            m
        }
    };
    let names = name_map(&state, org).await?;
    let bucket = |b: crate::domain::MetricBucket| ReportBucket {
        label: b.key,
        count: b.count,
    };
    Ok(IncidentsReportPage {
        active_tab: "incidents",
        window_days: m.window_days,
        windows: WINDOW_DAYS
            .iter()
            .map(|d| WindowOption {
                days: *d,
                active: *d == window,
            })
            .collect(),
        total: m.total,
        mtta: fmt_secs(m.mtta_secs),
        mttr: fmt_secs(m.mttr_secs),
        by_severity: m.by_severity.into_iter().map(bucket).collect(),
        by_state: m.by_state.into_iter().map(bucket).collect(),
        auto_resolved: m.auto_resolved,
        human_resolved: m.human_resolved,
        top_monitors: m
            .top_monitors
            .into_iter()
            .map(|t| ReportMonitorRow {
                id: t.target_id.to_string(),
                name: names
                    .get(&t.target_id)
                    .cloned()
                    .unwrap_or_else(|| "deleted monitor".to_string()),
                count: t.count,
            })
            .collect(),
    })
}

// ── Postmortem editor ─────────────────────────────────────────────────────

pub struct ActionItemModel {
    pub text: String,
    pub owner_user_id: String,
    pub done: bool,
}

pub struct MemberChoice {
    pub id: String,
    pub email: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "incidents/postmortem_form.html")]
pub struct PostmortemFormPage {
    pub active_tab: &'static str,
    pub incident_id: String,
    pub incident_label: String,
    pub exists: bool,
    pub published: bool,
    pub summary: String,
    pub root_cause: String,
    pub impact: String,
    pub action_items: Vec<ActionItemModel>,
    pub members: Vec<MemberChoice>,
}

pub async fn postmortem_form(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<PostmortemFormPage> {
    let inc = state
        .incident_ops_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let monitor_name = match inc.target_id {
        Some(t) => state.target_store.get(org, t).await?.map(|x| x.name),
        None => None,
    };
    let incident_label = inc
        .title
        .clone()
        .or(monitor_name)
        .unwrap_or_else(|| "Untitled incident".to_string());

    let pm = state.postmortem_store.get(org, id).await?;
    let members: Vec<MemberChoice> = members_map(&state, org)
        .await?
        .into_iter()
        .map(|(uid, email)| MemberChoice {
            id: uid.to_string(),
            email,
        })
        .collect();

    let action_items = pm
        .as_ref()
        .map(|p| {
            p.action_items
                .iter()
                .map(|a| ActionItemModel {
                    text: a.text.clone(),
                    owner_user_id: a.owner_user_id.map(|u| u.to_string()).unwrap_or_default(),
                    done: a.done,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(PostmortemFormPage {
        active_tab: "incidents",
        incident_id: id.to_string(),
        incident_label,
        exists: pm.is_some(),
        published: pm.as_ref().is_some_and(|p| p.published_at.is_some()),
        summary: pm
            .as_ref()
            .and_then(|p| p.summary.clone())
            .unwrap_or_default(),
        root_cause: pm
            .as_ref()
            .and_then(|p| p.root_cause.clone())
            .unwrap_or_default(),
        impact: pm
            .as_ref()
            .and_then(|p| p.impact.clone())
            .unwrap_or_default(),
        action_items,
        members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use askama::Template;

    #[test]
    fn every_check_kind_has_console_label() {
        for kind in crate::domain::CheckSpec::ALL_KINDS {
            assert!(
                kind == "http" || kind_label(kind) != "http",
                "kind {kind} falls through to the http label"
            );
        }
    }

    fn ops(state: IncidentState) -> OpsIncident {
        OpsIncident {
            id: Uuid::now_v7(),
            target_id: Some(Uuid::now_v7()),
            title: None,
            state,
            severity: crate::domain::IncidentSeverity::Major,
            urgency: crate::domain::IncidentUrgency::High,
            origin: crate::domain::IncidentOrigin::Monitor,
            visibility: crate::domain::IncidentVisibility::Internal,
            paging_enabled: true,
            counts_as_downtime: true,
            started_at: Utc::now(),
            ended_at: None,
            acknowledged_at: None,
            acknowledged_by: None,
            assigned_to: None,
            resolved_by: None,
            escalation_policy_id: None,
            escalation_level: 0,
            escalation_round: 0,
            next_escalation_at: None,
            check_count: 2,
            error_sample: None,
            regions_down: Vec::new(),
            regions_up: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn data(rows: Vec<ConsoleRow>) -> ConsoleData {
        ConsoleData {
            rows,
            self_id: Uuid::nil().to_string(),
            limit: 50,
            total: 0,
            page: 1,
            total_pages: 1,
            range_lo: 0,
            range_hi: 0,
            pager_prev: None,
            pager_next: None,
            page_sizes: PAGE_SIZES
                .iter()
                .copied()
                .map(|n| PageSizeLink {
                    n,
                    href: format!("/incidents?limit={n}"),
                    hx_get: None,
                    active: n == 50,
                })
                .collect(),
            partial_query: "state=all&limit=50".into(),
        }
    }

    fn page(rows: Vec<ConsoleRow>) -> IncidentsConsolePage {
        IncidentsConsolePage {
            active_tab: "incidents",
            state_tabs: STATE_FILTERS
                .iter()
                .copied()
                .map(|k| StateTab {
                    label: k,
                    href: format!("/incidents?state={k}"),
                    count: 0,
                    active: k == "all",
                })
                .collect(),
            severity_chips: vec![SeverityChip {
                label: "any",
                href: "/incidents".into(),
                active: true,
            }],
            sort_options: SORTS
                .iter()
                .map(|(key, label)| SortOption {
                    key,
                    label,
                    selected: *key == "recent",
                })
                .collect(),
            owner_options: vec![OwnerOption {
                value: String::new(),
                label: "Owner: any".into(),
                selected: true,
            }],
            search: String::new(),
            state_value: "all",
            severity_value: None,
            total: 0,
            data: data(rows),
        }
    }

    #[test]
    fn console_pauses_polling_while_hidden() {
        let html = page(vec![]).render().unwrap();
        // The resume carries the filter guard too: without it a catch-up poll
        // built from pre-click params can land after the filtered swap.
        assert!(html.contains(
            r#"hx-trigger="every 10s [!document.querySelector('#incidents-filter.htmx-request')], sm:poll-resume[!document.querySelector('#incidents-filter.htmx-request')] from:body""#
        ));
        assert!(html.contains("data-poll-pause"));
    }

    #[test]
    fn console_empty_renders_empty_state() {
        let html = page(vec![]).render().unwrap();
        assert!(html.contains("No incidents match"));
    }

    #[test]
    fn console_triggered_row_shows_ack_and_resolve() {
        let row = row_from(
            ops(IncidentState::Triggered),
            Some("api-gateway".into()),
            None,
            None,
            None,
            false,
        );
        let html = page(vec![row]).render().unwrap();
        assert!(html.contains("api-gateway"));
        assert!(html.contains(r#"data-incident-action="acknowledge""#));
        assert!(html.contains(r#"data-incident-action="resolve""#));
        assert!(!html.contains(r#"data-incident-action="reopen""#));
    }

    #[test]
    fn console_resolved_row_shows_reopen_only() {
        let mut inc = ops(IncidentState::Resolved);
        inc.ended_at = Some(Utc::now());
        let row = row_from(inc, Some("api".into()), None, None, None, false);
        let html = page(vec![row]).render().unwrap();
        assert!(html.contains(r#"data-incident-action="reopen""#));
        assert!(!html.contains(r#"data-incident-action="acknowledge""#));
    }

    #[test]
    fn console_shows_acked_by() {
        let mut inc = ops(IncidentState::Acknowledged);
        inc.acknowledged_at = Some(Utc::now());
        let acker = OwnerAvatar {
            initials: "AL".into(),
            color: "oklch(0.62 0.12 200)".into(),
            label: "alice@example.com".into(),
        };
        let row = row_from(inc, Some("api".into()), Some(acker), None, None, false);
        let mut p = page(vec![row]);
        p.data.total = 1;
        p.data.range_lo = 1;
        p.data.range_hi = 1;
        let html = p.render().unwrap();
        // Acknowledger shown as an avatar; email only in the tooltip.
        assert!(html.contains("monitors-avatar"));
        assert!(html.contains("acknowledged by alice@example.com"));
    }

    #[test]
    fn console_row_shows_monitor_kind() {
        let mut row = row_from(
            ops(IncidentState::Triggered),
            Some("api".into()),
            None,
            None,
            None,
            false,
        );
        row.kind = Some("tls");
        assert!(page(vec![row]).render().unwrap().contains(">tls<"));

        let manual = row_from(ops(IncidentState::Triggered), None, None, None, None, false);
        assert!(page(vec![manual]).render().unwrap().contains("—"));
    }

    #[test]
    fn console_resolved_shows_resolver_avatar_and_auto_marker() {
        let mut inc = ops(IncidentState::Resolved);
        inc.ended_at = Some(Utc::now());
        let resolver = OwnerAvatar {
            initials: "CA".into(),
            color: "oklch(0.62 0.12 50)".into(),
            label: "carol@example.com".into(),
        };
        let row = row_from(inc, Some("api".into()), None, Some(resolver), None, false);
        let html = page(vec![row]).render().unwrap();
        assert!(html.contains("resolved by carol@example.com"));

        let mut auto = ops(IncidentState::Resolved);
        auto.ended_at = Some(Utc::now());
        let arow = row_from(auto, Some("api".into()), None, None, None, false);
        let ahtml = page(vec![arow]).render().unwrap();
        assert!(ahtml.contains(">auto<"));
        assert!(!ahtml.contains("resolved by"));
    }

    #[test]
    fn console_row_shows_assignee_urgency_and_assign_to_me() {
        let mut inc = ops(IncidentState::Triggered);
        inc.urgency = crate::domain::IncidentUrgency::Low;
        let unassigned = row_from(inc, Some("api".into()), None, None, None, false);
        let html = page(vec![unassigned]).render().unwrap();
        // Low urgency surfaces, and an unassigned row offers assign-to-me.
        assert!(html.contains("notify"));
        assert!(html.contains(r#"data-incident-assign-self"#));

        let assigned = row_from(
            ops(IncidentState::Triggered),
            Some("api".into()),
            None,
            None,
            Some(OwnerAvatar {
                initials: "BO".into(),
                color: "oklch(0.62 0.12 100)".into(),
                label: "bob@example.com".into(),
            }),
            true,
        );
        let html = page(vec![assigned]).render().unwrap();
        // Owner shown as a ringed avatar (mine), email in tooltip, no take button.
        assert!(html.contains("monitors-avatar--me"));
        assert!(html.contains("BO"));
        assert!(html.contains("bob@example.com"));
        assert!(!html.contains(">take<"));
    }

    #[test]
    fn detail_renders_actions_timeline_and_acker() {
        let mut inc = ops(IncidentState::Acknowledged);
        inc.title = Some("Payments degraded".into());
        inc.target_id = None;
        inc.origin = crate::domain::IncidentOrigin::Manual;
        inc.acknowledged_at = Some(Utc::now());
        let timeline = vec![TimelineRow {
            kind: "Triggered",
            who: "alice@example.com".to_string(),
            via_mcp: false,
            occurred_at: Utc::now(),
            message: None,
        }];
        let updates = vec![PublicUpdateRow {
            phase: "identified",
            message: "Root cause found.".into(),
            posted_at: Utc::now(),
            author: "bob@example.com".into(),
        }];
        let page = make_detail_page(
            inc,
            None,
            Some("alice@example.com".into()),
            "Payments degraded".to_string(),
            timeline,
            updates,
            None,
        );
        let html = page.render().unwrap();
        assert!(html.contains("Payments degraded"));
        assert!(html.contains(r#"data-incident-note"#));
        assert!(html.contains("activity"));
        assert!(html.contains("alice@example.com"));
        // Public-update timeline + post form both present, with the author.
        assert!(html.contains(r#"data-incident-update-form"#));
        assert!(html.contains("status updates"));
        assert!(html.contains("Root cause found."));
        assert!(html.contains("bob@example.com"));
        assert!(html.contains("owner"));
        assert!(html.contains(r#"data-incident-assign-select"#));
    }

    /// The form's defaults are the promise: record it, tell nobody yet.
    #[test]
    fn declare_form_defaults_to_telling_nobody() {
        let html = DeclareIncidentPage {
            active_tab: "incidents",
            monitors: vec![MonitorOption {
                id: Uuid::now_v7().to_string(),
                name: "api-prod".into(),
            }],
        }
        .render()
        .unwrap();
        assert!(
            html.contains(r#"name="notify" value="0" class="sr-only" checked"#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="visibility" value="internal" class="sr-only" checked"#),
            "{html}"
        );
        assert!(html.contains("api-prod"), "{html}");
    }

    /// The only place these can change after declaring, so it has to arrive
    /// holding what the incident already says.
    #[test]
    fn edit_form_arrives_prefilled_and_leaves_the_monitor_alone() {
        let html = EditIncidentPage {
            active_tab: "incidents",
            id: Uuid::now_v7().to_string(),
            title: "partner API degraded".into(),
            monitor_name: Some("api-prod".into()),
            target_id: Some(Uuid::now_v7().to_string()),
            severity: "critical",
            urgency: "low",
            visibility: "public",
            public_title: "Elevated errors".into(),
            public_description: "Some checkouts fail.".into(),
            downtime_editable: true,
            counts_as_downtime: false,
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"value="partner API degraded""#), "{html}");
        assert!(
            html.contains(r#"name="severity" value="critical" checked"#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="counts_as_downtime" value="0" class="sr-only" checked"#),
            "{html}"
        );
        assert!(
            html.contains(r#"name="urgency" value="low" class="sr-only" checked"#),
            "{html}"
        );
        assert!(html.contains("Elevated errors"), "{html}");
        assert!(html.contains("Some checkouts fail."), "{html}");
        // Context, not a field: rebinding collides with the open-incident rule.
        assert!(!html.contains(r#"name="target_id""#), "{html}");
        assert!(html.contains("fixed after declaring"), "{html}");
    }

    /// Nothing else reconciles a passing monitor against an incident still
    /// open over it.
    #[test]
    fn detail_offers_to_close_an_incident_whose_monitor_recovered() {
        let mut page = make_detail_page(
            ops(IncidentState::Triggered),
            Some("api".into()),
            None,
            "api".to_string(),
            vec![],
            vec![],
            None,
        );
        assert!(
            !page.render().unwrap().contains("still open"),
            "no claim without evidence the monitor is passing"
        );
        page.monitor_recovered_at = Some(Utc::now());
        let html = page.render().unwrap();
        assert!(html.contains("still open"), "{html}");
        assert!(
            html.contains("every region's latest check passed"),
            "{html}"
        );
    }

    /// The first row answers what a review asks: how long, how fast anyone
    /// took it, how loud it was, how much broke.
    #[test]
    fn detail_leads_with_how_long_how_fast_how_loud() {
        let mut inc = ops(IncidentState::Resolved);
        inc.started_at = Utc::now() - chrono::Duration::minutes(30);
        inc.acknowledged_at = Some(inc.started_at + chrono::Duration::minutes(5));
        inc.ended_at = Some(inc.started_at + chrono::Duration::minutes(22));
        inc.check_count = 7;
        let page = make_detail_page(
            inc,
            Some("api".into()),
            Some("alice@example.com".into()),
            "api".to_string(),
            vec![],
            vec![],
            None,
        );
        let html = page.render().unwrap();
        assert!(html.contains("lasted"), "{html}");
        assert!(html.contains("22m 0s"), "{html}");
        assert!(html.contains("5m 0s"), "acknowledged in: {html}");
        assert!(html.contains(">7</p>"), "failed checks: {html}");
        // Nothing paged: that reads as quiet, not as a measured zero.
        assert!(html.contains("dashboard-rail__value--muted"), "{html}");
    }

    #[test]
    fn detail_renders_delivery_log_with_dead_letter_and_retry() {
        let mut page = make_detail_page(
            ops(IncidentState::Triggered),
            Some("api".into()),
            None,
            "api".to_string(),
            vec![],
            vec![],
            None,
        );
        page.notifications = vec![
            NotificationRow {
                channel: "Ops Slack".into(),
                transport: "slack".into(),
                reason: "opened",
                status: "sent",
                status_label: "sent",
                attempt: 1,
                error: None,
                sent_at: Some(Utc::now()),
                next_attempt_at: None,
                dead_lettered: false,
            },
            NotificationRow {
                channel: "Pager webhook".into(),
                transport: "webhook".into(),
                reason: "opened",
                status: "failed",
                status_label: "failed",
                attempt: 5,
                error: Some("connection refused".into()),
                sent_at: None,
                next_attempt_at: None,
                dead_lettered: true,
            },
        ];
        let html = page.render().unwrap();
        assert!(html.contains("delivery"));
        assert!(html.contains("Ops Slack"));
        assert!(html.contains("Pager webhook"));
        assert!(html.contains("dead-letter"));
        assert!(html.contains("connection refused"));
    }

    #[test]
    fn author_label_resolves_member_system_and_departed() {
        let u = UserId(Uuid::now_v7());
        let mut members = HashMap::new();
        members.insert(u, "alice@example.com".to_string());
        assert_eq!(
            author_label(Some(&u.0.to_string()), &members),
            "alice@example.com"
        );
        assert_eq!(author_label(Some("system"), &members), "system");
        assert_eq!(author_label(None, &members), "system");
        let gone = UserId(Uuid::now_v7());
        assert_eq!(
            author_label(Some(&gone.0.to_string()), &members),
            "former member"
        );
    }

    #[test]
    fn detail_internal_shows_publish_public_shows_unpublish() {
        let internal = make_detail_page(
            ops(IncidentState::Triggered),
            None,
            None,
            "x".into(),
            vec![],
            vec![],
            None,
        );
        let html = internal.render().unwrap();
        assert!(html.contains(r#"data-incident-publish"#));
        assert!(!html.contains(r#"data-incident-unpublish"#));

        let mut pubinc = ops(IncidentState::Triggered);
        pubinc.visibility = crate::domain::IncidentVisibility::Public;
        let public = make_detail_page(pubinc, None, None, "x".into(), vec![], vec![], None);
        let html = public.render().unwrap();
        assert!(html.contains(r#"data-incident-unpublish"#));
        assert!(!html.contains(r#"data-incident-publish"#));
    }

    #[test]
    fn actor_label_resolves_who_and_mcp() {
        use crate::domain::{ActorType, IncidentEventKind};
        let u = UserId(Uuid::now_v7());
        let mut members = HashMap::new();
        members.insert(u, "alice@example.com".to_string());
        let ev = |actor_type, actor_id| IncidentEvent {
            id: Uuid::now_v7(),
            incident_id: Uuid::now_v7(),
            occurred_at: Utc::now(),
            kind: IncidentEventKind::Acknowledged,
            actor_type,
            actor_id,
            detail: serde_json::Value::Null,
            message: None,
        };
        assert_eq!(
            actor_label(&ev(ActorType::System, None), &members),
            ("system".into(), false)
        );
        assert_eq!(
            actor_label(&ev(ActorType::User, Some(u)), &members),
            ("alice@example.com".into(), false)
        );
        assert_eq!(
            actor_label(&ev(ActorType::Mcp, Some(u)), &members),
            ("alice@example.com".into(), true)
        );
        // Names nobody, and must not borrow one from a stray actor_id.
        assert_eq!(
            actor_label(&ev(ActorType::Link, None), &members),
            ("notification".into(), false)
        );
        assert_eq!(
            actor_label(&ev(ActorType::Link, Some(u)), &members),
            ("notification".into(), false)
        );
        // An actor who has left the org no longer resolves to an email.
        let gone = UserId(Uuid::now_v7());
        assert_eq!(
            actor_label(&ev(ActorType::User, Some(gone)), &members),
            ("former member".into(), false)
        );
    }

    #[test]
    fn detail_shows_write_vs_edit_postmortem() {
        let none = make_detail_page(
            ops(IncidentState::Resolved),
            None,
            None,
            "x".into(),
            vec![],
            vec![],
            None,
        );
        assert!(none.render().unwrap().contains("write postmortem"));

        let pm = crate::domain::IncidentPostmortem {
            incident_id: Uuid::now_v7(),
            summary: Some("s".into()),
            root_cause: None,
            impact: None,
            action_items: vec![],
            author_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            published_at: Some(Utc::now()),
        };
        let with = make_detail_page(
            ops(IncidentState::Resolved),
            None,
            None,
            "x".into(),
            vec![],
            vec![],
            Some(&pm),
        );
        let html = with.render().unwrap();
        assert!(html.contains("edit postmortem"));
        assert!(html.contains("published"));
    }

    #[test]
    fn fmt_secs_humanises() {
        assert_eq!(fmt_secs(None), None);
        assert_eq!(fmt_secs(Some(8.0)).as_deref(), Some("8s"));
        assert_eq!(fmt_secs(Some(312.0)).as_deref(), Some("5m 12s"));
        assert_eq!(fmt_secs(Some(3780.0)).as_deref(), Some("1h 3m"));
    }

    #[test]
    fn reports_page_renders_kpis_and_top_monitors() {
        let page = IncidentsReportPage {
            active_tab: "incidents",
            window_days: 30,
            windows: WINDOW_DAYS
                .iter()
                .map(|d| WindowOption {
                    days: *d,
                    active: *d == 30,
                })
                .collect(),
            total: 4,
            mtta: Some("5m 0s".into()),
            mttr: Some("1h 2m".into()),
            by_severity: vec![ReportBucket {
                label: "major".into(),
                count: 3,
            }],
            by_state: vec![ReportBucket {
                label: "resolved".into(),
                count: 2,
            }],
            auto_resolved: 1,
            human_resolved: 1,
            top_monitors: vec![ReportMonitorRow {
                id: Uuid::now_v7().to_string(),
                name: "api-gateway".into(),
                count: 2,
            }],
        };
        let html = page.render().unwrap();
        assert!(html.contains("5m 0s"));
        assert!(html.contains("1h 2m"));
        assert!(html.contains("api-gateway"));
        // Shares the dashboard's range tabs, so labels are bare keys.
        assert!(html.contains("range-tabs__btn"), "{html}");
        assert!(html.contains(">7d</a>"), "{html}");
    }

    #[test]
    fn postmortem_form_renders_fields_and_publish() {
        let page = PostmortemFormPage {
            active_tab: "incidents",
            incident_id: Uuid::now_v7().to_string(),
            incident_label: "Payments degraded".into(),
            exists: true,
            published: false,
            summary: "cache stampede".into(),
            root_cause: String::new(),
            impact: String::new(),
            action_items: vec![ActionItemModel {
                text: "add jitter".into(),
                owner_user_id: String::new(),
                done: false,
            }],
            members: vec![MemberChoice {
                id: Uuid::now_v7().to_string(),
                email: "alice@example.com".into(),
            }],
        };
        let html = page.render().unwrap();
        assert!(html.contains("cache stampede"));
        assert!(html.contains("add jitter"));
        assert!(html.contains("alice@example.com"));
        assert!(html.contains(r#"data-postmortem-publish="true""#));
        // A draft that reads as published is the expensive mistake here.
        assert!(html.contains("check-type-card--on"), "{html}");
        assert!(html.contains("yours alone"), "{html}");
        // The saved row and the clone <template> render from one macro, so a
        // row the operator adds gets the same combobox as the ones on load.
        assert_eq!(html.matches("data-ai-owner data-sm-combobox").count(), 2);
    }

    /// It cannot be published before it exists, so the button must be absent
    /// rather than fail on click, and the card must say why it is out of reach.
    #[test]
    fn postmortem_form_offers_publish_only_once_there_is_something_to_publish() {
        let page = PostmortemFormPage {
            active_tab: "incidents",
            incident_id: Uuid::now_v7().to_string(),
            incident_label: "Payments degraded".into(),
            exists: false,
            published: false,
            summary: String::new(),
            root_cause: String::new(),
            impact: String::new(),
            action_items: vec![],
            members: vec![],
        };
        let html = page.render().unwrap();
        assert!(!html.contains("data-postmortem-publish"), "{html}");
        assert!(html.contains(r#"card-badge--warn">save first"#), "{html}");
    }
}
