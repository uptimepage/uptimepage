//! Settings → alert sources: the list, the form, and a source's page with its
//! URL, the setup for its tool, and its delivery log. Writes go through
//! `/api/v1/alert-sources` from the browser.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, DurationRound, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::api::handlers::alert_sources::{AlertEventItem, events_page, ingest_url};
use crate::app::AppState;
use crate::domain::{
    AlertGrouping, AlertSource, AlertSourceKind, IncidentState, OrgId, TargetAlerts, UserId,
};
use crate::error::{AppError, codes};
use crate::request::{CurrentOrg, CurrentUser};
use crate::storage::alert_delivery::Status;
use crate::storage::incident_ops::IncidentOpsFilter;
use crate::storage::traits::AlertEventCounts;
use crate::templates::filters;
use crate::web::error::{WebError, WebResult};
use crate::web::views::dashboard::{KpiCardSpec, render_spark_path_domain};
use crate::web::views::escalation::{Choice, policy_binding};
use crate::web::views::targets_form::{
    ChannelChoice, SecondsChoice, channel_choices, renotify_choices,
};
use crate::web::views::team_lock::{TeamLock, shows_teaser};
use crate::web::views::{channel_kind_label, resolve_org};

const TAB: &str = "alert-sources";

/// The kinds offered when a source is created, in the order shown.
const KINDS: [AlertSourceKind; 4] = [
    AlertSourceKind::Alertmanager,
    AlertSourceKind::Grafana,
    AlertSourceKind::Datadog,
    AlertSourceKind::Generic,
];

pub struct KindCard {
    pub value: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    pub hint: &'static str,
    pub selected: bool,
}

fn kind_desc(kind: AlertSourceKind) -> &'static str {
    match kind {
        AlertSourceKind::Alertmanager => "a webhook receiver in Prometheus Alertmanager",
        AlertSourceKind::Grafana => "a webhook contact point, signed if you like",
        AlertSourceKind::Datadog => "a webhook with the payload template we give you",
        AlertSourceKind::Generic => "JSON from a script, shaped like PagerDuty Events v2",
    }
}

fn kind_hint(kind: AlertSourceKind) -> &'static str {
    match kind {
        AlertSourceKind::Alertmanager => "prod alertmanager",
        AlertSourceKind::Grafana => "prod grafana",
        AlertSourceKind::Datadog => "prod datadog",
        AlertSourceKind::Generic => "backup job",
    }
}

fn kind_cards(selected: AlertSourceKind) -> Vec<KindCard> {
    KINDS
        .into_iter()
        .map(|k| KindCard {
            value: k.as_db_str(),
            label: k.as_db_str(),
            desc: kind_desc(k),
            hint: kind_hint(k),
            selected: k == selected,
        })
        .collect()
}

/// What the source is doing now. Its badge, its dot and where the list files
/// it all read from this one decision.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    Held,
    Paused,
    PagesNobody,
    /// Live, but no alert has arrived yet.
    Waiting,
    Live,
}

impl Standing {
    fn of(s: &AlertSource) -> Self {
        if s.plan_hold_at.is_some() {
            Self::Held
        } else if !s.enabled {
            Self::Paused
        } else if s.routes_to_nobody {
            Self::PagesNobody
        } else if s.last_event_at.is_none() {
            Self::Waiting
        } else {
            Self::Live
        }
    }

    /// The badge's word and tone.
    fn badge(self) -> (&'static str, &'static str) {
        match self {
            Self::Held => ("held", "held"),
            Self::Paused => ("paused", "disabled"),
            Self::PagesNobody => ("pages nobody", "down"),
            Self::Waiting | Self::Live => ("live", "up"),
        }
    }

    /// The dot's tone: a source still waiting is not shown as up.
    fn dot(self) -> &'static str {
        match self {
            Self::Held => "degraded",
            Self::PagesNobody => "down",
            Self::Live => "up",
            Self::Paused | Self::Waiting => "muted",
        }
    }

    fn shelf(self) -> Shelf {
        match self {
            Self::Live => Shelf::Live,
            Self::Paused => Shelf::Paused,
            Self::Held | Self::PagesNobody | Self::Waiting => Shelf::Attention,
        }
    }
}

/// Alert sources come with Team. The page stays for an account that still
/// holds some, so they can be read and deleted.
async fn lock(
    state: &AppState,
    org: OrgId,
    user: Option<UserId>,
    has_sources: bool,
) -> WebResult<Option<TeamLock>> {
    if state.quotas.limit_for_org(org).await?.max_alert_sources > 0 {
        return Ok(None);
    }
    let payer = match (state.db.as_ref(), user) {
        (Some(pool), Some(user)) => crate::storage::accounts::pays_for_org(pool, user, org).await?,
        _ => false,
    };
    Ok(Some(TeamLock {
        payer,
        teaser: !has_sources,
    }))
}

fn not_found() -> AppError {
    AppError::not_found(codes::ALERT_SOURCE_NOT_FOUND, "alert source not found")
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/alert_sources.html")]
pub struct ListPage {
    pub active_tab: &'static str,
    pub lock: Option<TeamLock>,
    /// The tools a locked page names in its pitch.
    pub kinds: Vec<KindCard>,
    /// The list as it looks with sources, greyed out under the pitch; empty
    /// unless the pitch stands in for the page.
    pub example: String,
}

impl ListPage {
    fn teaser(&self) -> bool {
        shows_teaser(&self.lock)
    }
}

pub struct SourceRow {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub standing: &'static str,
    pub tone: &'static str,
    /// The state dot's tone: a source that never sent an alert is not yet
    /// shown as up.
    pub dot: &'static str,
    pub routes_to_nobody: bool,
    pub route: Route,
    pub last_event_at: Option<DateTime<Utc>>,
    /// `None` without ClickHouse.
    pub day: Option<AlertEventCounts>,
    /// Events per hour across the day; empty when none arrived.
    pub spark_path: String,
    pub spark_fill: String,
}

/// Where the list files a source.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Shelf {
    /// Held, paging nobody, or never sent an alert.
    Attention,
    Live,
    Paused,
}

impl Shelf {
    fn label(self) -> &'static str {
        match self {
            Self::Attention => "needs attention",
            Self::Live => "live",
            Self::Paused => "paused",
        }
    }
}

pub struct SourceGroup {
    pub label: &'static str,
    pub attention: bool,
    pub paused: bool,
    pub rows: Vec<SourceRow>,
}

/// One hour of the intake ribbon, with what its tooltip shows: the hour in
/// the viewer's zone from the bounds, the UTC hour without script.
pub struct RibbonSeg {
    pub class: &'static str,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub time: String,
    pub stat: String,
}

/// The newest event across the sources.
pub struct LastEvent {
    pub at: DateTime<Utc>,
    pub source: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/alert_sources_partial.html")]
pub struct ListPartial {
    pub total: usize,
    pub live: usize,
    pub attention: usize,
    pub paused: usize,
    /// Events and not applied over the day, in the dashboard's card.
    pub kpi_cards: Vec<KpiCardSpec>,
    pub last_event: Option<LastEvent>,
    /// Summed over the listed sources; `None` without ClickHouse.
    pub day: Option<AlertEventCounts>,
    /// An hour each, oldest first; empty without ClickHouse.
    pub ribbon: Vec<RibbonSeg>,
    pub groups: Vec<SourceGroup>,
    /// The tools an empty list offers to start from.
    pub kinds: Vec<KindCard>,
}

pub async fn index(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    user: Result<CurrentUser, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/alert-sources") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let has_sources = !state.alert_source_store.names(org).await?.is_empty();
    let lock = lock(&state, org, user.ok().map(|u| u.0), has_sources).await?;
    let example = if shows_teaser(&lock) {
        example_list()
            .render()
            .map_err(|e| AppError::Other(anyhow::anyhow!(e)))?
    } else {
        String::new()
    };
    Ok(ListPage {
        active_tab: TAB,
        lock,
        kinds: kind_cards(AlertSourceKind::Alertmanager),
        example,
    }
    .into_response())
}

pub async fn list_partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/alert-sources") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let (mut sources, routes, day) = tokio::try_join!(
        async { Ok::<_, WebError>(state.alert_source_store.list(org).await?) },
        Routes::load(&state, org),
        day(&state, org, None),
    )?;
    sources.sort_by(|a, b| {
        Standing::of(a)
            .shelf()
            .cmp(&Standing::of(b).shelf())
            .then(b.last_event_at.cmp(&a.last_event_at))
            .then_with(|| a.name.cmp(&b.name))
    });
    let last_event = sources
        .iter()
        .filter_map(|s| s.last_event_at.map(|at| (at, &s.name)))
        .max_by_key(|(at, _)| *at)
        .map(|(at, name)| LastEvent {
            at,
            source: name.clone(),
        });
    let mut totals = [AlertEventCounts::default(); HOURS];
    let mut groups: Vec<(Shelf, Vec<SourceRow>)> = Vec::new();
    for s in sources {
        let now = Standing::of(&s);
        let shelf = now.shelf();
        let (standing, tone) = now.badge();
        let hours = day.as_ref().map(|d| d.of(s.id));
        if let Some(hours) = &hours {
            for (total, h) in totals.iter_mut().zip(hours) {
                *total = *total + *h;
            }
        }
        let (spark_path, spark_fill) = hours
            .as_ref()
            .map_or_else(Default::default, |h| spark(&h.map(|c| c.events)));
        let row = SourceRow {
            id: s.id.to_string(),
            kind: s.kind.as_db_str(),
            standing,
            tone,
            dot: now.dot(),
            routes_to_nobody: s.routes_to_nobody,
            route: routes.of(&s),
            last_event_at: s.last_event_at,
            day: hours.map(|h| h.into_iter().sum()),
            spark_path,
            spark_fill,
            name: s.name,
        };
        match groups.last_mut() {
            Some((last, rows)) if *last == shelf => rows.push(row),
            _ => groups.push((shelf, vec![row])),
        }
    }
    let count = |shelf: Shelf| {
        groups
            .iter()
            .find(|(s, _)| *s == shelf)
            .map_or(0, |(_, rows)| rows.len())
    };
    let total = groups.iter().map(|(_, rows)| rows.len()).sum();
    let day_total = day.as_ref().map(|_| totals.into_iter().sum());
    Ok(ListPartial {
        total,
        live: count(Shelf::Live),
        attention: count(Shelf::Attention),
        paused: count(Shelf::Paused),
        kpi_cards: kpi_cards(day.as_ref().map(|_| &totals), total),
        last_event,
        day: day_total,
        ribbon: day
            .as_ref()
            .map_or_else(Vec::new, |d| ribbon(d.start, &totals)),
        groups: groups
            .into_iter()
            .map(|(shelf, rows)| SourceGroup {
                label: shelf.label(),
                attention: shelf == Shelf::Attention,
                paused: shelf == Shelf::Paused,
                rows,
            })
            .collect(),
        kinds: kind_cards(AlertSourceKind::Alertmanager),
    }
    .into_response())
}

/// Three made-up sources for the pitch: what the list shows once tools send.
fn example_list() -> ListPartial {
    const PROD: [u64; HOURS] = [
        0, 0, 1, 3, 2, 0, 0, 0, 1, 4, 6, 2, 0, 0, 0, 1, 2, 5, 3, 1, 0, 0, 2, 3,
    ];
    const GRAFANA: [u64; HOURS] = [
        1, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 2, 0, 1,
    ];
    const DEPLOY: [u64; HOURS] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,
    ];
    let now = Utc::now();
    let start = window_start(now);
    let row = |name: &str, kind, route: Route, hours: [u64; HOURS], not_applied: u64, ago_mins| {
        let (spark_path, spark_fill) = spark(&hours);
        SourceRow {
            id: "example".into(),
            name: name.into(),
            kind,
            standing: "live",
            tone: "up",
            dot: "up",
            routes_to_nobody: false,
            route,
            last_event_at: Some(now - chrono::TimeDelta::minutes(ago_mins)),
            day: Some(AlertEventCounts {
                events: hours.iter().sum(),
                not_applied,
            }),
            spark_path,
            spark_fill,
        }
    };
    let policy = |name: &str, inherited| Route {
        policy: Some(PolicyRoute {
            name: name.into(),
            inherited,
        }),
        channels: Vec::new(),
    };
    let rows = vec![
        row(
            "Prod Alertmanager",
            "alertmanager",
            policy("Platform on-call", false),
            PROD,
            0,
            4,
        ),
        row(
            "Grafana",
            "grafana",
            Route {
                policy: None,
                channels: vec!["slack:#ops".into(), "email:oncall".into()],
            },
            GRAFANA,
            0,
            62,
        ),
        row(
            "Deploy script",
            "generic",
            policy("Primary on-call", true),
            DEPLOY,
            1,
            290,
        ),
    ];
    let totals: [AlertEventCounts; HOURS] = std::array::from_fn(|i| AlertEventCounts {
        events: PROD[i] + GRAFANA[i] + DEPLOY[i],
        not_applied: u64::from(i == 19),
    });
    ListPartial {
        total: rows.len(),
        live: rows.len(),
        attention: 0,
        paused: 0,
        kpi_cards: kpi_cards(Some(&totals), rows.len()),
        last_event: rows[0].last_event_at.map(|at| LastEvent {
            at,
            source: rows[0].name.clone(),
        }),
        day: Some(totals.into_iter().sum()),
        ribbon: ribbon(start, &totals),
        groups: vec![SourceGroup {
            label: Shelf::Live.label(),
            attention: false,
            paused: false,
            rows,
        }],
        kinds: Vec::new(),
    }
}

/// A count per hour as the dashboard draws it, from zero at the bottom;
/// nothing when every hour is zero.
fn spark(hours: &[u64; HOURS]) -> (String, String) {
    let max = hours.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return (String::new(), String::new());
    }
    let series: Vec<Option<f32>> = hours.iter().map(|&n| Some(n as f32)).collect();
    let (path, fill, _) = render_spark_path_domain(&series, 0.0, max as f32);
    (path, fill)
}

fn kpi_cards(totals: Option<&[AlertEventCounts; HOURS]>, sources: usize) -> Vec<KpiCardSpec> {
    let card = |label: &str, hint: &str, tint, pick: fn(&AlertEventCounts) -> u64| {
        let (value, (spark_path, spark_fill)) = match totals {
            Some(t) => (
                t.iter().map(pick).sum::<u64>().to_string(),
                spark(&t.map(|c| pick(&c))),
            ),
            None => ("—".to_string(), Default::default()),
        };
        KpiCardSpec {
            label: label.to_string(),
            value,
            hint_html: hint.to_string(),
            delta: None,
            spark_tint: tint,
            spark_path,
            spark_fill,
        }
    };
    let across = format!(
        "across {sources} source{}",
        if sources == 1 { "" } else { "s" }
    );
    vec![
        card("Events · 24h", &across, "ink", |c| c.events),
        card(
            "Not applied · 24h",
            "paused, held, capped, unreadable or unavailable",
            "warn",
            |c| c.not_applied,
        ),
    ]
}

/// The day an hour at a time: hatched with nothing in, amber when something
/// did not apply.
fn ribbon(start: DateTime<Utc>, totals: &[AlertEventCounts; HOURS]) -> Vec<RibbonSeg> {
    totals
        .iter()
        .zip(0i64..)
        .map(|(c, i)| {
            let from = start + chrono::TimeDelta::hours(i);
            RibbonSeg {
                class: if c.events == 0 {
                    "none"
                } else if c.not_applied > 0 {
                    "deg"
                } else {
                    "op"
                },
                from,
                to: from + chrono::TimeDelta::hours(1),
                time: from.format("%H:00 UTC").to_string(),
                stat: match (c.events, c.not_applied) {
                    (0, _) => "no alerts".to_string(),
                    (n, 0) => format!("{n} events"),
                    (n, na) => format!("{n} events · {na} not applied"),
                },
            }
        })
        .collect()
}

/// The clock hours the list and the summary count over.
const HOURS: usize = 24;

/// The start of the oldest of the `HOURS` clock hours ending with `now`'s.
fn window_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let hour = chrono::TimeDelta::hours(1);
    now.duration_trunc(hour).unwrap_or(now) - hour * (HOURS as i32 - 1)
}

/// Every source's events per clock hour, the current hour last.
struct Day {
    start: DateTime<Utc>,
    sources: HashMap<Uuid, [AlertEventCounts; HOURS]>,
}

impl Day {
    fn of(&self, source: Uuid) -> [AlertEventCounts; HOURS] {
        self.sources.get(&source).copied().unwrap_or_default()
    }
}

/// The last 24 clock hours of one source's log, or every source's; `None`
/// without ClickHouse.
async fn day(state: &AppState, org: OrgId, source: Option<Uuid>) -> WebResult<Option<Day>> {
    let Some(log) = state.alert_event_log.as_ref() else {
        return Ok(None);
    };
    let start = window_start(Utc::now());
    let mut sources: HashMap<Uuid, [AlertEventCounts; HOURS]> = HashMap::new();
    for h in log.hourly(org, source, start).await? {
        let slot = usize::try_from((h.hour - start).num_hours())
            .ok()
            .and_then(|i| sources.entry(h.source).or_default().get_mut(i));
        if let Some(slot) = slot {
            *slot = *slot + h.counts;
        }
    }
    Ok(Some(Day { start, sources }))
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/alert_source_form.html")]
pub struct FormPage {
    pub active_tab: &'static str,
    /// `create` or `edit`.
    pub mode: &'static str,
    pub action: String,
    pub method: &'static str,
    pub id: Option<String>,
    pub name: String,
    pub kinds: Vec<KindCard>,
    pub kind: &'static str,
    pub grouping: &'static str,
    /// `None` when the org has no escalation policies.
    pub policies: Option<Vec<Choice>>,
    pub policy_hint: String,
    /// Paged through a policy, its own or the org default.
    pub escalating: bool,
    pub channels: Vec<ChannelChoice>,
    pub renotify: Vec<SecondsChoice>,
    pub notify_recovery: bool,
    pub signed: bool,
}

async fn form(
    state: &AppState,
    org: OrgId,
    source: Option<&AlertSource>,
    kind: AlertSourceKind,
) -> WebResult<FormPage> {
    let binding = policy_binding(
        state,
        org,
        source.and_then(|s| s.escalation_policy_id),
        "source",
    )
    .await?;
    let alerts = source.map_or_else(TargetAlerts::default, |s| s.alerts.clone());
    Ok(FormPage {
        active_tab: TAB,
        mode: if source.is_some() { "edit" } else { "create" },
        action: source.map_or_else(
            || "/api/v1/alert-sources".to_string(),
            |s| format!("/api/v1/alert-sources/{}", s.id),
        ),
        method: if source.is_some() { "PATCH" } else { "POST" },
        id: source.map(|s| s.id.to_string()),
        name: source.map(|s| s.name.clone()).unwrap_or_default(),
        kinds: kind_cards(kind),
        kind: kind.as_db_str(),
        grouping: source
            .map_or(AlertGrouping::Group, |s| s.grouping)
            .as_db_str(),
        policies: (!binding.choices.is_empty()).then_some(binding.choices),
        policy_hint: binding.hint,
        escalating: binding.escalating,
        channels: channel_choices(state, org, &alerts).await?,
        renotify: renotify_choices(source.map_or(3600, |s| s.renotify_interval_secs)),
        notify_recovery: source.is_none_or(|s| s.notify_recovery),
        signed: source.is_some_and(|s| s.signed),
    })
}

#[derive(Debug, Deserialize)]
pub struct NewParams {
    /// The tool to start with, as its stored name.
    pub kind: Option<String>,
}

pub async fn new_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Query(q): Query<NewParams>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/alert-sources/new") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    if state.quotas.limit_for_org(org).await?.max_alert_sources <= 0 {
        return Ok(axum::response::Redirect::to("/settings/alert-sources").into_response());
    }
    let kind = KINDS
        .into_iter()
        .find(|k| q.kind.as_deref() == Some(k.as_db_str()))
        .unwrap_or(AlertSourceKind::Alertmanager);
    Ok(form(&state, org, None, kind).await?.into_response())
}

pub async fn edit_form(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
) -> WebResult<Response> {
    let org = match resolve_org(org, &format!("/settings/alert-sources/{id}/edit")) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let source = state
        .alert_source_store
        .get(org, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(form(&state, org, Some(&source), source.kind)
        .await?
        .into_response())
}

/// One block of the setup panel: what to put where in the tool.
pub struct Snippet {
    pub title: &'static str,
    pub body: String,
    pub note: &'static str,
}

/// A value that carries the token: shown masked until asked, copied whole.
pub struct Secret {
    pub full: String,
    pub masked: String,
}

/// A setup snippet as the page shows it.
pub struct SetupStep {
    pub title: &'static str,
    pub body: Secret,
    pub note: &'static str,
}

/// The last four characters stay, so a rotated token can be told apart.
fn mask(token: &str) -> String {
    let tail = token
        .get(token.len().saturating_sub(4)..)
        .unwrap_or_default();
    format!("••••••••{tail}")
}

fn hide(full: String, token: &str) -> Secret {
    Secret {
        masked: full.replace(token, &mask(token)),
        full,
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/alert_source.html")]
pub struct DetailPage {
    pub active_tab: &'static str,
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub standing: &'static str,
    pub tone: &'static str,
    pub enabled: bool,
    pub held: bool,
    pub routes_to_nobody: bool,
    pub grouping: &'static str,
    pub signed: bool,
    /// `None` when the stored token cannot be opened (the key that sealed it
    /// was rotated out); rotating mints a fresh one.
    pub ingest_url: Option<Secret>,
    pub token: Option<Secret>,
    pub rotated_at: Option<DateTime<Utc>>,
    pub previous_expires_at: Option<DateTime<Utc>>,
    pub previous_last_used_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub kinds: Vec<KindCard>,
    pub route: Route,
    pub setup: Vec<SetupStep>,
    pub summary: Summary,
    pub events: EventsPartial,
}

impl DetailPage {
    fn filters(&self) -> &'static [Filter] {
        &FILTERS
    }

    /// A source that has never sent anything opens on how to connect it.
    fn connecting(&self) -> bool {
        self.last_event_at.is_none()
    }
}

/// The strip above the tabs.
pub struct Summary {
    /// `None` without ClickHouse.
    pub day: Option<AlertEventCounts>,
    pub open_incidents: usize,
}

/// A delivery log filter: the key its link carries, its label, and the
/// outcomes it shows, every one when empty.
type Filter = (&'static str, &'static str, &'static [Status]);

const FILTERS: [Filter; 6] = [
    ("all", "all", &[]),
    ("opened", "opened", &[Status::Opened]),
    ("repeat", "repeat", &[Status::Repeat]),
    ("resolved", "resolved", &[Status::Resolved]),
    ("ignored", "ignored", &[Status::Ignored]),
    ("not_applied", "not applied", &Status::NOT_APPLIED),
];

fn filter(show: Option<&str>) -> &'static Filter {
    FILTERS
        .iter()
        .find(|(key, ..)| Some(*key) == show)
        .unwrap_or(&FILTERS[0])
}

/// Who an alert from the source pages: a policy when one applies, else its
/// bound channels.
pub struct Route {
    pub policy: Option<PolicyRoute>,
    /// Bound channels as `kind:name`.
    pub channels: Vec<String>,
}

pub struct PolicyRoute {
    pub name: String,
    /// Inherited from the org default rather than set on the source.
    pub inherited: bool,
}

/// What routes are read from, loaded once for any number of sources.
struct Routes {
    policies: HashMap<Uuid, String>,
    org_default: Option<Uuid>,
    /// Each channel as `kind:name`.
    channels: HashMap<Uuid, String>,
}

impl Routes {
    async fn load(state: &AppState, org: OrgId) -> WebResult<Self> {
        let (policies, channels, org_default) = tokio::try_join!(
            state.escalation_policy_store.list(org),
            state.notification_channel_store.list(org),
            state.escalation_policy_store.org_default(org),
        )?;
        Ok(Self {
            policies: policies.into_iter().map(|p| (p.id, p.name)).collect(),
            org_default,
            // A disabled channel pages nobody, as `routes_to_nobody` counts it.
            channels: channels
                .into_iter()
                .filter(|c| c.enabled)
                .map(|c| (c.id, format!("{}:{}", channel_kind_label(c.kind), c.name)))
                .collect(),
        })
    }

    fn of(&self, source: &AlertSource) -> Route {
        let name = |id: Uuid| self.policies.get(&id).cloned();
        let policy = match source.escalation_policy_id.and_then(name) {
            Some(name) => Some(PolicyRoute {
                name,
                inherited: false,
            }),
            None => self.org_default.and_then(name).map(|name| PolicyRoute {
                name,
                inherited: true,
            }),
        };
        let channels = source
            .alerts
            .iter()
            .filter_map(|b| self.channels.get(&b.channel_id).cloned())
            .collect();
        Route { policy, channels }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/alert_source_events.html")]
pub struct EventsPartial {
    pub id: String,
    /// The filter key, carried by the "older" link.
    pub show: &'static str,
    pub events: Vec<AlertEventItem>,
    pub next_cursor: Option<String>,
    /// The state of each incident the page links.
    pub incident_states: HashMap<Uuid, IncidentState>,
}

impl EventsPartial {
    /// A linked incident's state and its badge tone.
    fn incident(&self, id: &Uuid) -> Option<(&'static str, &'static str)> {
        self.incident_states.get(id).map(|s| {
            let tone = match s {
                IncidentState::Triggered => "down",
                IncidentState::Acknowledged => "degraded",
                IncidentState::Resolved => "up",
            };
            (s.as_db_str(), tone)
        })
    }

    /// Badge tone: what opened, what closed, what changed nothing, and what
    /// did not apply.
    fn tone(&self, outcome: &str) -> &'static str {
        match outcome {
            "opened" => "down",
            "resolved" => "up",
            "repeat" | "ignored" => "disabled",
            _ => "degraded",
        }
    }
}

pub async fn detail(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
) -> WebResult<Response> {
    let org = match resolve_org(org, &format!("/settings/alert-sources/{id}")) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let s = state
        .alert_source_store
        .get(org, id)
        .await?
        .ok_or_else(not_found)?;
    let (log, summary, routes) = tokio::try_join!(
        log_page(&state, org, id, None, None),
        summary(&state, org, &s),
        Routes::load(&state, org),
    )?;
    let route = routes.of(&s);
    let base = state.cfg.auth.public_base_url.trim_end_matches('/');
    let (standing, tone) = Standing::of(&s).badge();
    let (ingest_url, token, setup) = match s.token.as_deref() {
        Some(t) => (
            Some(hide(ingest_url(base, t), t)),
            Some(hide(t.to_string(), t)),
            setup(s.kind, base, t, s.signed)
                .into_iter()
                .map(|n| SetupStep {
                    title: n.title,
                    body: hide(n.body, t),
                    note: n.note,
                })
                .collect(),
        ),
        None => (None, None, Vec::new()),
    };
    Ok(DetailPage {
        active_tab: TAB,
        id: s.id.to_string(),
        kind: s.kind.as_db_str(),
        standing,
        tone,
        enabled: s.enabled,
        held: s.plan_hold_at.is_some(),
        routes_to_nobody: s.routes_to_nobody,
        grouping: s.grouping.as_db_str(),
        signed: s.signed,
        ingest_url,
        token,
        setup,
        rotated_at: s.token_rotated_at,
        previous_expires_at: s.open_overlap(),
        previous_last_used_at: s.open_overlap().and(s.prev_token_last_used_at),
        last_event_at: s.last_event_at,
        kinds: kind_cards(s.kind),
        route,
        summary,
        events: log,
        name: s.name,
    }
    .into_response())
}

async fn summary(state: &AppState, org: OrgId, source: &AlertSource) -> WebResult<Summary> {
    let day = day(state, org, Some(source.id))
        .await?
        .map(|d| d.of(source.id).into_iter().sum());
    let open = state
        .incident_ops_store
        .counts_by_state(
            org,
            &IncidentOpsFilter {
                alert_source: Some(source.id),
                ..IncidentOpsFilter::default()
            },
        )
        .await?;
    Ok(Summary {
        day,
        open_incidents: open.triggered + open.acknowledged,
    })
}

#[derive(Debug, Deserialize)]
pub struct EventsParams {
    pub cursor: Option<String>,
    pub show: Option<String>,
}

/// The next page of the delivery log, for the page's "older" link.
pub async fn events_partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsParams>,
) -> WebResult<Response> {
    let org = match resolve_org(org, &format!("/settings/alert-sources/{id}")) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    state
        .alert_source_store
        .get(org, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(
        log_page(&state, org, id, q.show.as_deref(), q.cursor.as_deref())
            .await?
            .into_response(),
    )
}

/// One page of the delivery log under a filter, with the state of each
/// incident it links.
async fn log_page(
    state: &AppState,
    org: OrgId,
    id: Uuid,
    show: Option<&str>,
    cursor: Option<&str>,
) -> WebResult<EventsPartial> {
    let (show, _, outcomes) = *filter(show);
    let page = events_page(state, org, id, outcomes, cursor, None).await?;
    let mut ids: Vec<Uuid> = page.items.iter().filter_map(|e| e.incident_id).collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(EventsPartial {
        id: id.to_string(),
        show,
        events: page.items,
        next_cursor: page.next_cursor,
        incident_states: state.incident_ops_store.states(org, &ids).await?,
    })
}

/// What to configure in the tool, and a command that sends one test alert
/// the source's parser reads, then its resolve.
fn setup(kind: AlertSourceKind, base: &str, token: &str, signed: bool) -> Vec<Snippet> {
    let hook = format!("{base}/hooks/alerts");
    let url = format!("{hook}/{token}");
    let curl = |body: &str| {
        format!(
            "curl -sS -X POST {hook} \\\n  -H 'Authorization: Bearer {token}' \\\n  -H 'Content-Type: application/json' \\\n  -d '{body}'"
        )
    };
    let am = |status: &str| {
        format!(
            r#"{{"groupKey":"uptimepage-test","status":"{status}","alerts":[{{"status":"{status}","fingerprint":"uptimepage-test","labels":{{"alertname":"UptimepageTest","severity":"minor"}},"annotations":{{"summary":"Test alert from the setup panel"}}}}]}}"#
        )
    };
    match kind {
        AlertSourceKind::Alertmanager => vec![
            Snippet {
                title: "alertmanager.yml",
                body: format!(
                    "receivers:\n  - name: uptimepage\n    webhook_configs:\n      - url: {hook}\n        send_resolved: true\n        http_config:\n          authorization:\n            credentials: {token}"
                ),
                note: "Route the alerts this source should page to this receiver. Leave max_alerts at 0: a cut payload never closes an incident.",
            },
            Snippet {
                title: "send a test alert",
                body: curl(&am("firing")),
                note: "Opens a real incident and pages this source's route. Send it again with \"resolved\" in both status fields to close it.",
            },
        ],
        AlertSourceKind::Grafana => {
            let mut contact = format!(
                "Contact point type:   Webhook\nURL:                  {hook}\nHTTP method:          POST\nAuthorization header: scheme Bearer, credentials {token}"
            );
            if signed {
                contact.push_str(
                    "\nHMAC signature:       the secret set on this source\nSignature header:     X-Grafana-Alerting-Signature\nTimestamp header:     X-Grafana-Alerting-Timestamp",
                );
            }
            vec![
                Snippet {
                    title: "grafana contact point",
                    body: contact,
                    note: "Point the notification policies whose alerts this source should page at the contact point.",
                },
                Snippet {
                    title: "send a test alert",
                    body: curl(&am("firing")),
                    note: if signed {
                        "This source checks signatures, so an unsigned command is refused. Use the contact point's own test button instead."
                    } else {
                        "Opens a real incident and pages this source's route. Send it again with \"resolved\" in both status fields to close it."
                    },
                },
            ]
        }
        AlertSourceKind::Datadog => vec![
            Snippet {
                title: "datadog webhook",
                body: format!(
                    "Name:    uptimepage\nURL:     {url}\nPayload:\n{{\n  \"transition\": \"$ALERT_TRANSITION\",\n  \"cycle_key\": \"$ALERT_CYCLE_KEY\",\n  \"monitor_id\": \"$ALERT_ID\",\n  \"scope\": \"$ALERT_SCOPE\",\n  \"title\": \"$EVENT_TITLE\",\n  \"message\": \"$TEXT_ONLY_MSG\",\n  \"priority\": \"$ALERT_PRIORITY\",\n  \"link\": \"$LINK\",\n  \"tags\": \"$TAGS\",\n  \"date\": \"$DATE\"\n}}"
                ),
                note: "Add it under Integrations → Webhooks, then mention @webhook-uptimepage in the message of each monitor that should page.",
            },
            Snippet {
                title: "send a test alert",
                body: curl(
                    r#"{"transition":"Triggered","cycle_key":"uptimepage-test","monitor_id":"0","title":"Test alert from the setup panel","priority":"P3"}"#,
                ),
                note: "Opens a real incident and pages this source's route. Send it again with \"Recovered\" as the transition to close it.",
            },
        ],
        AlertSourceKind::Generic => vec![Snippet {
            title: "send an alert",
            body: curl(
                r#"{"event_action":"trigger","dedup_key":"uptimepage-test","payload":{"summary":"Test alert from the setup panel","severity":"info"}}"#,
            ),
            note: "event_action is trigger or resolve, and dedup_key ties them together. This one opens a real incident; send it again as resolve to close it.",
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_key_names_its_outcomes_and_anything_else_shows_all() {
        assert_eq!(filter(Some("not_applied")).2, Status::NOT_APPLIED);
        assert_eq!(filter(Some("opened")).2, [Status::Opened]);
        for unknown in [None, Some(""), Some("all"), Some("bogus")] {
            let (key, _, outcomes) = *filter(unknown);
            assert_eq!((key, outcomes.len()), ("all", 0), "{unknown:?}");
        }
    }

    #[test]
    fn the_token_shows_only_masked() {
        let token = "EilA_2CsiMBuVMtey82nYSFdX-93ns8PH9PHeI0nVYc";
        for kind in KINDS {
            for snippet in setup(kind, "https://up.example", token, true) {
                let shown = hide(snippet.body.clone(), token);
                assert!(!shown.masked.contains(token), "{kind:?} {}", snippet.title);
                assert_eq!(shown.full, snippet.body);
            }
        }
        let url = hide(ingest_url("https://up.example", token), token);
        assert_eq!(url.masked, "https://up.example/hooks/alerts/••••••••nVYc");
    }

    #[test]
    fn every_test_command_is_a_body_its_parser_reads() {
        for kind in KINDS {
            for snippet in setup(kind, "https://up.example", "tok", false) {
                if !snippet.title.contains("test") && !snippet.title.contains("send") {
                    continue;
                }
                let body = snippet
                    .body
                    .split("-d '")
                    .nth(1)
                    .and_then(|b| b.strip_suffix('\''))
                    .expect("a curl body");
                let events = crate::intake::parse(kind, AlertGrouping::Group, body.as_bytes())
                    .unwrap_or_else(|e| panic!("{kind:?}: {}", e.reason));
                assert_eq!(events.len(), 1, "{kind:?}");
            }
        }
    }
}
