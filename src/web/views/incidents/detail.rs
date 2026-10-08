//! One incident: its state, who acknowledged, the timeline, public updates
//! and every page it sent.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{
    CheckResult, CheckStatus, IncidentEvent, IncidentStatusPhase, OpsIncident, OrgId, UserId,
};
use crate::error::AppError;
use crate::error::codes;
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::storage::{ClampedRange, TimeRange};
use crate::templates::filters;
use crate::web::error::WebResult;

use super::actors::{AckList, ack_list, actor_label, author_label};
use super::{
    OwnerAvatar, OwnerOption, PageChoice, fmt_secs, incident_label, member_avatar, members_map,
    page_choices, state_label,
};

/// Long enough to catch a slow interval's last check, short enough that a
/// long-paused monitor reads as no evidence rather than as recovered.
const RECOVERY_LOOKBACK: chrono::Duration = chrono::Duration::hours(24);
/// Rows pulled to find each region's latest check: more than one round across
/// every region a monitor can be probed from.
const RECOVERY_SAMPLE: usize = 60;

pub struct TimelineRow {
    pub kind: &'static str,
    /// Who acted: a member's email, `system` for automated transitions, or
    /// `former member` when the actor has since left the org.
    pub who: String,
    /// Where a named member acted when it was not the console: "MCP",
    /// "Telegram" or "Pushover".
    pub via: Option<&'static str>,
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

#[derive(Template, WebTemplate)]
#[template(path = "incidents/detail.html")]
pub struct IncidentDetailPage {
    pub active_tab: &'static str,
    pub id: String,
    pub label: String,
    pub target_id: Option<String>,
    pub monitor_name: Option<String>,
    /// False once the incident closed with its deleted monitor.
    pub reopenable: bool,
    pub state: &'static str,
    pub state_label: &'static str,
    pub severity: &'static str,
    pub urgency: &'static str,
    pub origin: &'static str,
    pub visibility: &'static str,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub acks: AckList,
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
    /// The org's pages, marked where this incident is posted. Empty for an
    /// incident with a monitor, whose pages are the ones carrying it.
    pub pages: Vec<PageChoice>,
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
        MonitorDeleted => "monitor deleted",
    }
}

pub async fn detail(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(uid): CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<IncidentDetailPage> {
    let inc = state
        .incident_ops_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::INCIDENT_NOT_FOUND, "incident not found"))?;
    let monitor_name = inc.target_name.clone();
    let (members, mut acks, events) = tokio::try_join!(
        members_map(&state, org),
        async {
            Ok(state
                .incident_ops_store
                .acknowledgements(org, &[id])
                .await?)
        },
        async { Ok(state.incident_ops_store.timeline(org, id).await?) },
    )?;
    let acks = acks.remove(&id).unwrap_or_default();
    let timeline = events
        .iter()
        .map(|e| {
            let (who, via) = actor_label(e, &members);
            TimelineRow {
                kind: event_kind_label(e),
                who,
                via,
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
    let owner = assigned_to.and_then(|u| member_avatar(u, &members));
    let acks = ack_list(&acks, uid, &members);
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
    let pages = if inc.target_id.is_none() {
        let linked = state.incident_ops_store.status_pages(org, id).await?;
        page_choices(&state, org, &linked).await?
    } else {
        Vec::new()
    };
    let label = incident_label(inc.title.clone(), monitor_name.clone());
    let mut page = make_detail_page(
        inc,
        monitor_name,
        acks,
        label,
        timeline,
        public_updates,
        postmortem.as_ref(),
    );
    page.owner = owner;
    page.owner_options = owner_options;
    page.notifications = notifications;
    page.monitor_recovered_at = recovered_at;
    page.pages = pages;
    Ok(page)
}

pub(super) fn make_detail_page(
    inc: OpsIncident,
    monitor_name: Option<String>,
    acks: AckList,
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
        reopenable: inc.reopenable(),
        state: inc.state.as_db_str(),
        state_label: state_label(inc.state),
        severity: inc.severity.as_db_str(),
        urgency: inc.urgency.as_db_str(),
        origin: inc.origin.as_db_str(),
        visibility: inc.visibility.as_db_str(),
        started_at: inc.started_at,
        ended_at: inc.ended_at,
        acks,
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
        pages: Vec::new(),
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
