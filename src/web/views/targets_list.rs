//! Operator Monitors page. Three round-trips per request: PG `list`,
//! one batched CH `dashboard_rollup`, PG `list_members` for owners.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::metrics::DashboardMetrics;
use crate::domain::{CheckStatus, OrgId, Target};
use crate::storage::TimeRange;
use crate::storage::orgs::list_members;
use crate::storage::traits::{TargetFilter, TargetSort};
use crate::web::avatar::{avatar_color, initials_from};
use crate::web::error::WebResult;
use crate::web::filters;
use crate::web::views::{PageSizeLink, PagerLink, describe_check, humanize_duration};
use crate::web::{AuthedBrowser, CurrentOrg};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const UPTIME_WINDOW_DAYS: i64 = 30;
const UNGROUPED_LABEL: &str = "Ungrouped";
const TYPE_CHIPS: &[&str] = &["HTTP", "TCP", "DNS", "TLS", "DOMAIN"];
const PAGE_SIZES: &[usize] = &[25, 50, 100, 200];

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default, deserialize_with = "empty_as_none")]
    pub owner: Option<Uuid>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// Treats an empty query value (`owner=`, `enabled=`) as absent. The
/// filter form's "any" options submit empty strings; without this serde
/// tries to parse `""` as a Uuid/bool and 400s the whole request.
fn empty_as_none<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = <Option<String>>::deserialize(de)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => s.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

pub struct MonitorRow {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub address: String,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub group_name: Option<String>,
    pub last_status: &'static str,
    pub status_class: &'static str,
    /// UTC instant of the last check; `None` when no samples. Drives the
    /// client-side local-time rewrite; `last_check_label` is the no-JS fallback.
    pub last_check_at: Option<chrono::DateTime<Utc>>,
    pub last_check_label: String,
    pub uptime_30d_label: String,
    pub owner: Option<OwnerView>,
    /// `terraform`/`api` chip for externally-managed monitors; `None` (UI) hides it.
    pub managed_by: Option<&'static str>,
    /// Failing and recovering often enough that its alerts are being held.
    pub flapping: bool,
    /// The plan no longer covers this monitor, so nothing is probing it. Shown
    /// rather than hidden: the row is still the customer's, and a monitor that
    /// silently vanished from the list would read as data loss.
    pub plan_held: bool,
}

pub struct OwnerView {
    pub id: Uuid,
    pub initials: String,
    pub label: String,
    pub color: String,
}

pub struct GroupBlock {
    pub name: String,
    /// `false` when bucketed under the `Ungrouped` sentinel — drives
    /// a header variant that hides the name.
    pub has_name: bool,
    pub rows: Vec<MonitorRow>,
    pub total: usize,
    pub worst_status: &'static str,
    pub avg_uptime_label: String,
}

pub struct TypeChip {
    pub label: &'static str,
    pub count: usize,
    pub active: bool,
    /// Full filter query for this chip (current filters with `kind`
    /// swapped to the chip's), so the template renders one string into
    /// both `href` and `hx-get` instead of re-spelling every param.
    pub query: String,
}

pub struct OwnerOption {
    pub id: Uuid,
    pub label: String,
    pub initials: String,
    pub color: String,
    pub selected: bool,
}

pub struct GroupOption {
    pub name: String,
    pub selected: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "targets/list.html")]
pub struct ListPage {
    pub active_tab: &'static str,
    pub groups: Vec<GroupBlock>,
    pub total: usize,
    pub paused_total: usize,
    pub type_chips: Vec<TypeChip>,
    pub owner_options: Vec<OwnerOption>,
    pub group_options: Vec<GroupOption>,
    pub page_sizes: Vec<PageSizeLink>,
    /// Shared filter query (everything but `limit`/`offset`), URL-encoded, for
    /// the footer pagination links' real hrefs + htmx swap targets.
    pub query_suffix: String,
    pub has_more: bool,
    pub limit: usize,
    pub offset: usize,
    pub pager_prev: Option<PagerLink>,
    pub pager_next: Option<PagerLink>,
    pub q: String,
    pub tag: String,
    pub enabled: Option<bool>,
    pub group: String,
    pub owner: Option<Uuid>,
    pub kind: String,
    pub sort: &'static str,
    pub onboarding: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "targets/partials/list_body.html")]
pub struct ListBodyPartial {
    pub groups: Vec<GroupBlock>,
    pub total: usize,
    pub paused_total: usize,
    pub type_chips: Vec<TypeChip>,
    /// Carried by the partial so toolbar selects refresh on every
    /// `#target-rows` swap; without these the chip-active class would
    /// drift away from the URL state.
    pub owner_options: Vec<OwnerOption>,
    pub group_options: Vec<GroupOption>,
    pub page_sizes: Vec<PageSizeLink>,
    /// Shared filter query (everything but `limit`/`offset`), URL-encoded, for
    /// the footer pagination links' real hrefs + htmx swap targets.
    pub query_suffix: String,
    pub has_more: bool,
    pub limit: usize,
    pub offset: usize,
    pub pager_prev: Option<PagerLink>,
    pub pager_next: Option<PagerLink>,
    pub q: String,
    pub tag: String,
    pub enabled: Option<bool>,
    pub group: String,
    pub owner: Option<Uuid>,
    pub kind: String,
    pub sort: &'static str,
}

pub async fn index(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> WebResult<ListPage> {
    let page = build_page(&state, org, &params).await?;
    Ok(page)
}

pub async fn list_partial(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> WebResult<ListBodyPartial> {
    let page = build_page(&state, org, &params).await?;
    Ok(ListBodyPartial {
        groups: page.groups,
        total: page.total,
        paused_total: page.paused_total,
        type_chips: page.type_chips,
        owner_options: page.owner_options,
        group_options: page.group_options,
        page_sizes: page.page_sizes,
        query_suffix: page.query_suffix,
        has_more: page.has_more,
        limit: page.limit,
        offset: page.offset,
        pager_prev: page.pager_prev,
        pager_next: page.pager_next,
        q: page.q,
        tag: page.tag,
        enabled: page.enabled,
        group: page.group,
        owner: page.owner,
        kind: page.kind,
        sort: page.sort,
    })
}

async fn build_page(state: &AppState, org: OrgId, params: &ListParams) -> WebResult<ListPage> {
    let sort = parse_sort(params.sort.as_deref());
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = params.offset.unwrap_or(0);

    let trim_str = |s: &Option<String>| s.as_deref().map(str::trim).map(str::to_owned);
    let q = trim_str(&params.q)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let tag = trim_str(&params.tag)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let group = trim_str(&params.group)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let kind = trim_str(&params.kind)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    let q_filter = (!q.is_empty()).then(|| q.clone());
    let tag_filter = (!tag.is_empty()).then(|| tag.clone());
    let group_filter = (!group.is_empty()).then(|| group.clone());

    // Chip tallies are org-wide and ignore the active kind, so they stay
    // invariant as the user switches chips — counted in SQL, not over the page.
    let kind_counts = state
        .target_store
        .count_by_kind(
            org,
            TargetFilter {
                q: q_filter.clone(),
                tag: tag_filter.clone(),
                enabled: params.enabled,
                group: group_filter.clone(),
                owner: params.owner,
                sort,
                ..TargetFilter::default()
            },
        )
        .await?;
    let mut chip_counts: HashMap<&'static str, usize> = HashMap::new();
    for (db_kind, n) in &kind_counts {
        if let Some(label) = db_kind_to_chip(db_kind) {
            chip_counts.insert(label, *n as usize);
        }
    }
    let all_count: usize = kind_counts.values().map(|n| *n as usize).sum();

    let filter = TargetFilter {
        limit: Some(limit + 1),
        offset,
        q: q_filter,
        tag: tag_filter,
        enabled: params.enabled,
        group: group_filter,
        owner: params.owner,
        // Empty = no filter; an unrecognised label maps to "" so the filter is
        // still honored as "matches nothing" rather than silently dropped.
        kind: (!kind.is_empty()).then(|| chip_to_db_kind(&kind).unwrap_or_default()),
        region: None,
        sort,
    };

    let mut targets = state.target_store.list(org, filter).await?;
    let has_more = targets.len() > limit;
    if has_more {
        targets.truncate(limit);
    }

    let (metrics_by_target, folded_status): (
        HashMap<Uuid, DashboardMetrics>,
        HashMap<Uuid, CheckStatus>,
    ) = if targets.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        let now = Utc::now();
        let range = TimeRange {
            from: now - ChronoDuration::days(UPTIME_WINDOW_DAYS),
            to: now,
        };
        let (rollup, folded) = tokio::join!(
            state.results_store.dashboard_rollup(org, range, None),
            state.folded_status(org, range, crate::app::folded_status_policies(&targets)),
        );
        (
            rollup?.into_iter().map(|m| (m.target_id, m)).collect(),
            folded,
        )
    };

    let members = match state.db.as_ref() {
        Some(pool) => list_members(pool, org).await?,
        None => Vec::new(),
    };
    let owner_lookup: HashMap<Uuid, MemberLite> = members
        .iter()
        .map(|m| {
            (
                m.membership.user_id.0,
                MemberLite {
                    id: m.membership.user_id.0,
                    label: m.email.clone(),
                },
            )
        })
        .collect();

    let now = Utc::now();
    // One aggregate for the whole page: derived, so it is true the moment a
    // monitor settles and there is no stored flag to clear.
    let flap_cfg = &state.cfg.escalation;
    let flapping = state
        .incident_ops_store
        .flapping_targets(
            org,
            now - chrono::Duration::seconds(flap_cfg.flap_window_secs.max(1) as i64),
            // Above, not at: the crossing open still pages, so a monitor at
            // exactly the threshold has had nothing held yet. `0` keeps the
            // store's own "damping off" early-return reachable.
            match flap_cfg.flap_max_opens {
                0 => 0,
                max => max.saturating_add(1),
            },
        )
        .await
        .unwrap_or_default();
    let mut rows: Vec<MonitorRow> = Vec::with_capacity(targets.len());
    let mut paused_total = 0usize;
    for t in targets {
        let metrics = metrics_by_target.get(&t.id);
        let row = build_row(
            &t,
            metrics,
            folded_status.get(&t.id).copied(),
            &owner_lookup,
            now,
            &flapping,
        );
        if !row.enabled {
            paused_total += 1;
        }
        rows.push(row);
    }
    let total = rows.len();
    let groups = bucket_by_group(rows);

    let chip_query =
        |chip_kind: &str| build_query_suffix(&q, &tag, &group, chip_kind, params, sort_key(sort));
    let mut type_chips: Vec<TypeChip> = Vec::with_capacity(TYPE_CHIPS.len() + 1);
    type_chips.push(TypeChip {
        label: "All",
        count: all_count,
        active: kind.is_empty(),
        query: chip_query(""),
    });
    for label in TYPE_CHIPS {
        type_chips.push(TypeChip {
            label,
            count: chip_counts.get(label).copied().unwrap_or(0),
            active: kind.eq_ignore_ascii_case(label),
            query: chip_query(label),
        });
    }

    let mut owner_options: Vec<OwnerOption> = members
        .iter()
        .map(|m| {
            let id = m.membership.user_id.0;
            let label = m.email.clone();
            OwnerOption {
                id,
                initials: initials_from(&label),
                color: avatar_color(id),
                label,
                selected: Some(id) == params.owner,
            }
        })
        .collect();
    owner_options.sort_by(|a, b| a.label.cmp(&b.label));

    let group_options =
        collect_group_options(&state.target_store.distinct_groups(org).await?, &group);

    let query_suffix = build_query_suffix(&q, &tag, &group, &kind, params, sort_key(sort));

    let page_link = |limit: usize, offset: usize| {
        (
            format!("/targets?limit={limit}&offset={offset}&{query_suffix}"),
            format!("/web/targets/list?limit={limit}&offset={offset}&{query_suffix}"),
        )
    };
    let page_sizes = PAGE_SIZES
        .iter()
        .map(|&n| {
            let (href, hx) = page_link(n, 0);
            PageSizeLink {
                n,
                href,
                hx_get: Some(hx),
                active: n == limit,
            }
        })
        .collect();
    let pager = |label: &'static str, offset: usize| {
        let (href, hx) = page_link(limit, offset);
        PagerLink {
            label,
            href,
            hx_get: Some(hx),
        }
    };
    let pager_prev = (offset > 0).then(|| pager("prev", offset.saturating_sub(limit)));
    let pager_next = has_more.then(|| pager("next", offset + limit));

    let onboarding = groups.is_empty()
        && q.is_empty()
        && tag.is_empty()
        && group.is_empty()
        && kind.is_empty()
        && params.owner.is_none()
        && params.enabled.is_none()
        && offset == 0;

    Ok(ListPage {
        active_tab: "targets",
        groups,
        total,
        paused_total,
        type_chips,
        owner_options,
        group_options,
        page_sizes,
        query_suffix,
        has_more,
        limit,
        offset,
        pager_prev,
        pager_next,
        q,
        tag,
        enabled: params.enabled,
        group,
        owner: params.owner,
        kind,
        sort: sort_key(sort),
        onboarding,
    })
}

struct MemberLite {
    id: Uuid,
    label: String,
}

/// Type-chip label (URL param) → `check_spec` type tag for the SQL filter.
fn chip_to_db_kind(label: &str) -> Option<String> {
    match label.to_ascii_uppercase().as_str() {
        "HTTP" => Some("http"),
        "TCP" => Some("tcp"),
        "PING" => Some("ping"),
        "HEARTBEAT" => Some("heartbeat"),
        "DNS" => Some("dns"),
        "TLS" => Some("tls_cert"),
        "DOMAIN" => Some("domain_expiry"),
        "FLOW" => Some("flow"),
        _ => None,
    }
    .map(str::to_owned)
}

/// Inverse of [`chip_to_db_kind`]: type tag → chip label.
fn db_kind_to_chip(kind: &str) -> Option<&'static str> {
    match kind {
        "http" => Some("HTTP"),
        "tcp" => Some("TCP"),
        "ping" => Some("PING"),
        "heartbeat" => Some("HEARTBEAT"),
        "dns" => Some("DNS"),
        "tls_cert" => Some("TLS"),
        "domain_expiry" => Some("DOMAIN"),
        "flow" => Some("FLOW"),
        _ => None,
    }
}

fn build_row(
    t: &Target,
    metrics: Option<&DashboardMetrics>,
    folded: Option<CheckStatus>,
    owner_lookup: &HashMap<Uuid, MemberLite>,
    now: chrono::DateTime<Utc>,
    flapping: &std::collections::HashSet<Uuid>,
) -> MonitorRow {
    let (kind, address) = describe_check(&t.check);
    let class = match metrics {
        // `last_status` alone is whichever region reported last.
        Some(m) if m.samples > 0 => match folded {
            Some(s) => status_class_for(s.as_str()),
            None => status_class_for(m.last_status.as_str()),
        },
        _ => "",
    };
    // A held monitor is not being probed, so whatever it last reported is a
    // claim about the past. It falls back to the same "no status" the code
    // already models for a monitor with no samples, which keeps a green dot
    // off a row nothing is watching.
    let class = status_while_watched(class, t.plan_hold_at.is_some());
    let last_status = class;
    let status_class = class;

    let last_check_at = metrics
        .and_then(|m| m.last_minute_ts)
        .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts, 0));
    let last_check_label = last_check_at
        .map(|then| relative_ago(now - then))
        .unwrap_or_else(|| "—".into());

    let uptime_30d_label = match metrics {
        Some(m) if m.samples > 0 => {
            let pct = (m.up as f64 / m.samples as f64) * 100.0;
            format!("{pct:.2}%")
        }
        _ => "—".into(),
    };

    let owner = t.owner_user_id.and_then(|id| {
        owner_lookup.get(&id).map(|m| OwnerView {
            id: m.id,
            initials: initials_from(&m.label),
            label: m.label.clone(),
            color: avatar_color(m.id),
        })
    });

    MonitorRow {
        id: t.id.to_string(),
        name: t.name.clone(),
        kind,
        address,
        enabled: t.enabled,
        tags: t.tags.clone(),
        group_name: t.group_name.clone(),
        last_status,
        status_class,
        last_check_at,
        last_check_label,
        uptime_30d_label,
        owner,
        managed_by: t.write_source.managed_label(),
        flapping: flapping.contains(&t.id),
        plan_held: t.plan_hold_at.is_some(),
    }
}

fn bucket_by_group(rows: Vec<MonitorRow>) -> Vec<GroupBlock> {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<MonitorRow>> = HashMap::new();
    for row in rows {
        let key = row
            .group_name
            .clone()
            .unwrap_or_else(|| UNGROUPED_LABEL.into());
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_default().push(row);
    }
    order
        .into_iter()
        .map(|name| {
            let rows = buckets.remove(&name).unwrap_or_default();
            let total = rows.len();
            let has_name = name != UNGROUPED_LABEL;
            let worst_status = rows.iter().map(|r| r.last_status).fold("up", worst_of);
            let avg_uptime_label = avg_uptime_label(&rows);
            GroupBlock {
                name,
                has_name,
                rows,
                total,
                worst_status,
                avg_uptime_label,
            }
        })
        .collect()
}

fn avg_uptime_label(rows: &[MonitorRow]) -> String {
    let mut sum = 0.0f64;
    let mut n = 0u32;
    for r in rows {
        let s = r.uptime_30d_label.trim_end_matches('%');
        if let Ok(v) = s.parse::<f64>() {
            sum += v;
            n += 1;
        }
    }
    if n == 0 {
        "—".into()
    } else {
        format!("{:.2}%", sum / n as f64)
    }
}

/// A held monitor is not being probed, so whatever it last reported is a claim
/// about the past. It falls back to the same "no status" already modelled for a
/// monitor with no samples, which is what keeps a green dot off a row nothing is
/// watching, and out of the up filter that would count it as fine.
fn status_while_watched(class: &'static str, held: bool) -> &'static str {
    if held { "" } else { class }
}

fn worst_of(acc: &'static str, next: &'static str) -> &'static str {
    let rank = |s: &str| CheckStatus::from_label(s).map_or(0, |c| c.severity_rank() + 1);
    if rank(next) > rank(acc) { next } else { acc }
}

fn collect_group_options(group_names: &[String], selected: &str) -> Vec<GroupOption> {
    let mut names: Vec<String> = group_names.to_vec();
    names.sort();
    names.dedup();
    if !selected.is_empty() && !names.iter().any(|n| n == selected) {
        names.push(selected.to_owned());
    }
    names
        .into_iter()
        .map(|name| GroupOption {
            selected: name == selected,
            name,
        })
        .collect()
}

fn build_query_suffix(
    q: &str,
    tag: &str,
    group: &str,
    kind: &str,
    params: &ListParams,
    sort: &str,
) -> String {
    use crate::auth::url::push_param;
    let mut s = String::new();
    push_param(&mut s, "q", q);
    push_param(&mut s, "tag", tag);
    push_param(&mut s, "group", group);
    push_param(&mut s, "kind", kind);
    if let Some(o) = params.owner {
        push_param(&mut s, "owner", &o.to_string());
    }
    if let Some(e) = params.enabled {
        push_param(&mut s, "enabled", if e { "true" } else { "false" });
    }
    push_param(&mut s, "sort", sort);
    s
}

fn status_class_for(status: &str) -> &'static str {
    match status {
        "up" => "up",
        "down" => "down",
        "degraded" => "degraded",
        "error" => "error",
        _ => "",
    }
}

fn relative_ago(d: ChronoDuration) -> String {
    if d.num_seconds() < 0 {
        return "just now".into();
    }
    format!("{} ago", humanize_duration(d))
}

/// Splits on `@` first so emails don't all collapse to identical initials.
fn parse_sort(raw: Option<&str>) -> TargetSort {
    match raw.unwrap_or("recent") {
        "name" => TargetSort::Name,
        "created" => TargetSort::Created,
        _ => TargetSort::RecentActivity,
    }
}

fn sort_key(s: TargetSort) -> &'static str {
    match s {
        TargetSort::RecentActivity => "recent",
        TargetSort::Name => "name",
        TargetSort::Created => "created",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_check_kind_round_trips_through_the_chip_maps() {
        for kind in crate::domain::CheckSpec::ALL_KINDS {
            let chip = db_kind_to_chip(kind).unwrap_or_else(|| panic!("no chip for {kind}"));
            assert_eq!(chip_to_db_kind(chip).as_deref(), Some(kind));
        }
    }

    fn row(name: &str, group: Option<&str>, status: &'static str, enabled: bool) -> MonitorRow {
        MonitorRow {
            id: Uuid::nil().to_string(),
            name: name.into(),
            kind: "HTTP",
            address: "https://example.com".into(),
            enabled,
            tags: vec![],
            group_name: group.map(str::to_owned),
            last_status: status,
            status_class: status_class_for(status),
            last_check_at: None,
            last_check_label: "3s ago".into(),
            uptime_30d_label: "99.94%".into(),
            owner: None,
            managed_by: None,
            flapping: false,
            plan_held: false,
        }
    }

    #[test]
    fn a_held_monitor_reports_no_status_at_all() {
        assert_eq!(
            status_while_watched("up", true),
            "",
            "nothing has probed it since the hold, so green is a claim about last week"
        );
        assert_eq!(status_while_watched("down", true), "");
        assert_eq!(status_while_watched("up", false), "up");
    }

    #[test]
    fn worst_of_picks_highest_severity() {
        let g = [row("a", None, "up", true), row("b", None, "down", true)];
        let s = g.iter().map(|r| r.last_status).fold("up", worst_of);
        assert_eq!(s, "down");
    }

    #[test]
    fn bucket_by_group_preserves_first_appearance_order() {
        let rows = vec![
            row("a", Some("B"), "up", true),
            row("b", Some("A"), "up", true),
            row("c", Some("B"), "up", true),
        ];
        let groups = bucket_by_group(rows);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "B");
        assert_eq!(groups[0].total, 2);
        assert_eq!(groups[1].name, "A");
    }

    #[test]
    fn bucket_renames_none_to_ungrouped() {
        let rows = vec![row("a", None, "up", true)];
        let groups = bucket_by_group(rows);
        assert_eq!(groups[0].name, "Ungrouped");
        assert!(!groups[0].has_name);
    }

    #[test]
    fn avg_uptime_label_skips_dashes() {
        let mut rs = vec![row("a", None, "up", true), row("b", None, "", true)];
        rs[1].uptime_30d_label = "—".into();
        assert_eq!(avg_uptime_label(&rs), "99.94%");
    }

    #[test]
    fn initials_split_on_at_sign() {
        assert_eq!(initials_from("ada@acme.io"), "AD");
        assert_eq!(initials_from("zoe"), "ZO");
        assert_eq!(initials_from(""), "—");
    }

    #[test]
    fn list_page_renders_onboarding_when_empty_no_filters() {
        let page = ListPage {
            active_tab: "targets",
            groups: vec![],
            total: 0,
            paused_total: 0,
            type_chips: vec![],
            owner_options: vec![],
            group_options: vec![],
            page_sizes: vec![],
            query_suffix: String::new(),
            has_more: false,
            limit: 50,
            offset: 0,
            pager_prev: None,
            pager_next: None,
            q: String::new(),
            tag: String::new(),
            enabled: None,
            group: String::new(),
            owner: None,
            kind: String::new(),
            sort: "recent",
            onboarding: true,
        };
        let html = page.render().unwrap();
        assert!(html.contains("nothing to watch yet."));
        assert!(html.contains("add your first monitor"));
    }

    #[test]
    fn list_page_renders_grouped_table() {
        let g = GroupBlock {
            name: "API & Web".into(),
            has_name: true,
            total: 1,
            worst_status: "up",
            avg_uptime_label: "99.99%".into(),
            rows: vec![row("api", Some("API & Web"), "up", true)],
        };
        let page = ListPage {
            active_tab: "targets",
            groups: vec![g],
            total: 1,
            paused_total: 0,
            type_chips: vec![TypeChip {
                label: "All",
                count: 1,
                active: true,
                query: String::new(),
            }],
            owner_options: vec![],
            group_options: vec![],
            page_sizes: vec![],
            query_suffix: String::new(),
            has_more: false,
            limit: 50,
            offset: 0,
            pager_prev: None,
            pager_next: None,
            q: String::new(),
            tag: String::new(),
            enabled: None,
            group: String::new(),
            owner: None,
            kind: String::new(),
            sort: "recent",
            onboarding: false,
        };
        let html = page.render().unwrap();
        // Askama escapes `&` to the numeric reference `&#38;`.
        assert!(html.contains("API &#38; Web"));
        assert!(html.contains("api"));
        assert!(html.contains("99.99%"));
        // Per-row uptime is also rendered.
        assert!(html.contains("99.94%"));
    }
    #[test]
    fn a_flapping_row_carries_the_chip() {
        let mut r = row("api", Some("API & Web"), "up", true);
        r.flapping = true;
        let g = GroupBlock {
            name: "API & Web".into(),
            has_name: true,
            total: 1,
            worst_status: "up",
            avg_uptime_label: "99.99%".into(),
            rows: vec![r],
        };
        let page = ListPage {
            active_tab: "targets",
            groups: vec![g],
            total: 1,
            paused_total: 0,
            type_chips: vec![],
            owner_options: vec![],
            group_options: vec![],
            page_sizes: vec![],
            query_suffix: String::new(),
            has_more: false,
            limit: 50,
            offset: 0,
            pager_prev: None,
            pager_next: None,
            q: String::new(),

            tag: String::new(),
            enabled: None,
            group: String::new(),
            owner: None,
            kind: String::new(),
            sort: "recent",
            onboarding: false,
        };
        let html = page.render().unwrap();
        assert!(html.contains("flapping-chip"));
        assert!(html.contains(">flapping<"));
    }

    #[test]
    fn a_held_row_says_so_instead_of_disappearing() {
        let mut r = row("api", Some("API & Web"), "up", true);
        r.plan_held = true;
        // What build_row produces for a held monitor: no status, so no dot and
        // no answer to the up filter.
        r.status_class = "";
        r.last_status = "";
        let g = GroupBlock {
            name: "API & Web".into(),
            has_name: true,
            total: 1,
            worst_status: "up",
            avg_uptime_label: "99.99%".into(),
            rows: vec![r],
        };
        let page = ListPage {
            active_tab: "targets",
            groups: vec![g],
            total: 1,
            paused_total: 0,
            type_chips: vec![],
            owner_options: vec![],
            group_options: vec![],
            page_sizes: vec![],
            query_suffix: String::new(),
            has_more: false,
            limit: 50,
            offset: 0,
            pager_prev: None,
            pager_next: None,
            q: String::new(),

            tag: String::new(),
            enabled: None,
            group: String::new(),
            owner: None,
            kind: String::new(),
            sort: "recent",
            onboarding: false,
        };
        let html = page.render().unwrap();
        assert!(
            html.contains("held-chip") && html.contains(">held<"),
            "a held monitor stays on the list wearing its reason; dropping the row would read as data loss"
        );
        assert!(!html.contains("flapping-chip"), "held is not flapping");
        // Nothing is probing it, so the last colour it wore is a claim about a
        // week ago. An operator scanning the list must not read it as watched.
        let row = &html[html.find("data-row-id").expect("row")..];
        assert!(
            row.contains(r#"data-held="true""#) && !row.contains(r#"data-status="up""#),
            "a held monitor is neither up nor down, and must not answer the up filter"
        );
        assert!(
            !row.contains("monitors-dot--up"),
            "a green dot on an unprobed monitor is the whole failure"
        );
    }
}
