use super::ALLOWED_SCHEMES;
use super::dispatch::flow_capable_set;
use std::collections::HashSet;
use std::net::IpAddr;

use url::Host;
use uuid::Uuid;

use crate::api::error::codes;
use crate::api::redaction::REDACTED;
use crate::app::AppState;
use crate::domain::{
    CheckSpec, NewTarget, OrgId, RegionIncidentPolicy, Target, TargetAlerts, TargetUpdate,
    min_interval_secs_for_kind,
};
use crate::error::{AppError, Result};
use crate::security::SsrfGuard;

/// Reject a monitor whose `{{var}}` references don't all resolve against the
/// org's variables — an unknown key or a secret used in a field that forbids it.
/// Fails fast at save instead of silently dropping the monitor at probe time.
/// Non-HTTP or no-variable specs are a no-op.
pub(crate) async fn validate_variable_refs(
    state: &AppState,
    org: OrgId,
    check: &CheckSpec,
) -> Result<()> {
    use crate::worker::interpolate::{
        flow_uses_vars, repoint_risk, resolve_flow_spec, resolve_http_spec, uses_vars,
    };

    let unresolved = |e: crate::worker::interpolate::ResolveError| {
        AppError::unprocessable(codes::UNRESOLVED_VARIABLE, e.to_string())
    };
    match check {
        CheckSpec::Http(http) if uses_vars(http) => {
            let vars = state.variable_store.resolve_map(org).await?;
            resolve_http_spec(http, &vars)
                .map(drop)
                .map_err(unresolved)?;
            if let Some(risk) = repoint_risk(http, &vars) {
                tracing::warn!(
                    org = %org.0,
                    url_variables = ?risk.url_keys,
                    secret_headers = ?risk.secret_header_keys,
                    "monitor combines a url variable with a secret header; repointing the \
                     url variable would send the secret to a different host"
                );
            }
        }
        CheckSpec::Flow(flow) if flow_uses_vars(flow) => {
            let vars = state.variable_store.resolve_map(org).await?;
            resolve_flow_spec(flow, &vars)
                .map(drop)
                .map_err(unresolved)?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn ssrf_guard(state: &AppState) -> SsrfGuard {
    SsrfGuard::new(state.cfg.security.allow_private_targets)
}

/// Interactive probing (test-now / check-now) is meaningless for a passive
/// check, since there is nothing to reach out to.
pub(crate) fn reject_passive_probe(check: &CheckSpec) -> Result<()> {
    if check.is_passive() {
        return Err(AppError::bad_request(
            codes::HEARTBEAT_NOT_PROBEABLE,
            "heartbeat monitors receive pings from your systems; there is nothing to probe",
        ));
    }
    Ok(())
}

/// Apply the plan's flow limits: whether the kind is available at all, and how
/// long a journey it may declare. Runs on every admission path (create, update,
/// bulk, test) so a flow the plan would refuse to save is also refused a test.
pub(crate) fn gate_flow(check: &CheckSpec, plan: &crate::domain::Plan) -> Result<()> {
    let CheckSpec::Flow(flow) = check else {
        return Ok(());
    };
    if plan.max_flow_checks <= 0 {
        return Err(AppError::forbidden_code(
            codes::FLOW_CHECKS_DISABLED,
            "flow monitors are not available on your plan",
        ));
    }
    let allowed = crate::domain::FlowCheck::allowed_steps(plan.max_flow_steps);
    if flow.steps.len() > allowed {
        return Err(AppError::bad_request_field(
            codes::INVALID_FLOW_PARAMS,
            format!("your plan allows at most {allowed} steps in a flow monitor"),
            "check.steps",
        ));
    }
    Ok(())
}

/// Reject a flow monitor whose regions have no node that can run it — otherwise
/// it is accepted and then silently never probed. Capable = an enabled
/// flow-capable agent in the region, or the control plane's own region when it
/// runs flow in-process. No-op for non-flow checks.
pub(crate) async fn ensure_flow_regions_covered(
    state: &AppState,
    check: &CheckSpec,
    regions: &[String],
) -> Result<()> {
    if !matches!(check, CheckSpec::Flow(_)) {
        return Ok(());
    }
    flow_covered(&flow_capable_set(state).await?, check, regions)
}

fn flow_covered(capable: &HashSet<String>, check: &CheckSpec, regions: &[String]) -> Result<()> {
    if !matches!(check, CheckSpec::Flow(_)) || regions.iter().any(|r| capable.contains(r)) {
        return Ok(());
    }
    Err(AppError::unprocessable(
        codes::NO_FLOW_CAPABLE_AGENT,
        "no flow-capable agent runs in this monitor's region; enable the flow \
         engine on an agent there before creating the monitor",
    ))
}

/// What a create needs to know about regions, read once per request so a bulk
/// of thousands does not ask the database per item.
pub(crate) struct RegionSnapshot {
    available: Vec<String>,
    preferred: Vec<String>,
    flow_capable: HashSet<String>,
    default_region: String,
}

impl RegionSnapshot {
    pub(crate) async fn load(state: &AppState) -> Result<Self> {
        Ok(Self {
            available: state.target_store.available_regions().await?,
            preferred: state.target_store.default_selected_regions().await?,
            flow_capable: flow_capable_set(state).await?,
            default_region: state.cfg.scheduler.effective_default_region().to_string(),
        })
    }

    pub(crate) fn available_count(&self) -> usize {
        self.available.len()
    }

    /// The set a create that names none gets. A flow only runs where an engine
    /// exists, so the default-selected preference cannot be what leaves one
    /// with nowhere to run: it falls back to the full catalog.
    pub(crate) fn default_for(&self, check: &CheckSpec, max_regions: i32) -> Vec<String> {
        let mut preferred = self.preferred.clone();
        if matches!(check, CheckSpec::Flow(_)) {
            preferred.retain(|r| self.flow_capable.contains(r));
            if preferred.is_empty() {
                preferred = self
                    .available
                    .iter()
                    .filter(|r| self.flow_capable.contains(*r))
                    .cloned()
                    .collect();
            }
        }
        default_region_set(preferred, max_regions, &self.default_region)
    }

    pub(crate) fn ensure_flow_covered(&self, check: &CheckSpec, regions: &[String]) -> Result<()> {
        flow_covered(&self.flow_capable, check, regions)
    }

    /// A named set is refused rather than filtered: dropping a region would
    /// leave the caller expecting coverage nobody has.
    pub(crate) fn ensure_flow_runs_in_each(
        &self,
        check: &CheckSpec,
        regions: &[String],
    ) -> Result<()> {
        if !matches!(check, CheckSpec::Flow(_)) {
            return Ok(());
        }
        match regions
            .iter()
            .find(|r| !self.flow_capable.contains(r.as_str()))
        {
            Some(bad) => Err(AppError::unprocessable(
                codes::NO_FLOW_CAPABLE_AGENT,
                format!(
                    "no flow-capable agent runs in {bad}; drop it from the region set \
                     or enable the flow engine on an agent there"
                ),
            )),
            None => Ok(()),
        }
    }
}

/// Abuse admission control for one user-supplied check. Every handler that
/// accepts a `CheckSpec` (create, bulk per item, update, test) routes through
/// this single chokepoint, so a denylisted URL/domain can never enter the
/// store — and every block is audited fire-and-forget to `quota_events`.
pub(crate) fn check_abuse(
    state: &AppState,
    org: OrgId,
    check: &crate::domain::CheckSpec,
) -> Result<()> {
    let Some(hit) = state.abuse.inspect(check) else {
        return Ok(());
    };
    crate::quotas::service::record_quota_event(
        state.db.clone(),
        Some(org),
        None,
        "abuse_blocked",
        Some(hit.quota_name()),
        serde_json::json!({ "detail": hit.detail }),
        None,
    );
    Err(hit.into_app_error())
}

/// The PATCH counterpart of the floor check in [`validate_new_target`]. A kind
/// change is validated against the stored interval, since switching to a slower
/// kind while omitting `interval` would otherwise keep a cadence that kind
/// rejects. A missing target is left for the update itself to 404.
pub(crate) async fn validate_patch_interval(
    state: &AppState,
    org: OrgId,
    id: Uuid,
    update: &TargetUpdate,
    prefetched: Option<&Target>,
) -> Result<()> {
    let requested = update.interval.map(|i| i.as_secs() as i64);
    if requested.is_none() && update.check.is_none() {
        return Ok(());
    }
    // The row is only worth reading for the half the request leaves out. A high
    // interval answers the kind floor on its own, but never the heartbeat
    // pairing, which needs the spec to know the window it is judged against.
    let needs_row = requested.is_none() || update.check.is_none();
    let fetched = match (needs_row, prefetched) {
        (true, None) => state.target_store.get(org, id).await?,
        _ => None,
    };
    let stored = prefetched.or(fetched.as_ref());
    let Some(requested) = requested.or_else(|| stored.map(|t| t.interval.as_secs() as i64)) else {
        return Ok(());
    };
    let kind = update
        .check
        .as_ref()
        .map(|c| c.kind())
        .or_else(|| stored.map(|t| t.check.kind()));
    let plan = state.quotas.limit_for_org(org).await?;
    let plan_min = i64::from(plan.min_check_interval_secs);
    // No kind means the read was skipped, which happens only when no kind floor
    // could bind. The plan floor applies either way.
    let effective_floor = plan_min.max(kind.map_or(0, |k| min_interval_secs_for_kind(k) as i64));
    if requested < effective_floor {
        return Err(AppError::min_check_interval(
            requested,
            effective_floor,
            plan.id.clone(),
        ));
    }
    // Either half can arrive alone, so the pairing is judged on the merge of
    // the request and the stored row.
    if let Some(check) = update.check.as_ref().or(stored.map(|t| &t.check)) {
        validate_heartbeat_cadence(
            check,
            std::time::Duration::from_secs(requested.max(0) as u64),
        )?;
    }
    Ok(())
}

pub(crate) fn validate_new_target(
    new: &mut NewTarget,
    guard: &SsrfGuard,
    plan: &crate::domain::quota::Plan,
) -> Result<()> {
    let requested = new.interval.as_secs() as i64;
    let kind_floor = min_interval_secs_for_kind(new.check.kind()) as i64;
    let effective_floor = i64::from(plan.min_check_interval_secs).max(kind_floor);
    if requested < effective_floor {
        return Err(AppError::min_check_interval(
            requested,
            effective_floor,
            plan.id.clone(),
        ));
    }
    validate_check(&new.check, guard)?;
    validate_heartbeat_cadence(&new.check, new.interval)?;
    new.tags = normalize_tags(&new.tags)?;
    validate_alerts(&new.alerts)?;
    validate_alert_confirmations(Some(new.alert_confirmations))?;
    validate_renotify_interval(Some(new.renotify_interval_secs))?;
    validate_group_name(new.group_name.as_deref())
}

pub(crate) fn validate_heartbeat_cadence(
    check: &CheckSpec,
    interval: std::time::Duration,
) -> Result<()> {
    let Some(hb) = check.as_heartbeat() else {
        return Ok(());
    };
    let window = hb.period.as_secs().saturating_add(hb.grace.as_secs());
    if interval.as_secs() > window {
        return Err(AppError::bad_request_field(
            codes::INVALID_HEARTBEAT_PARAMS,
            format!(
                "check interval ({}s) is longer than the heartbeat window it judges ({}s) — lower the interval or raise the period",
                interval.as_secs(),
                window
            ),
            "interval",
        ));
    }
    Ok(())
}

/// The outage reminder cadence is either off (0) or no tighter than a minute —
/// a sub-minute reminder would just spam responders.
pub(crate) fn validate_renotify_interval(secs: Option<u32>) -> Result<()> {
    if matches!(secs, Some(n) if n > 0 && n < 60) {
        return Err(AppError::bad_request_field(
            codes::INVALID_ALERT_CONFIG,
            "renotify_interval_secs must be 0 (off) or at least 60",
            "renotify_interval_secs",
        ));
    }
    Ok(())
}

/// One confirmation minimum — alerting after zero failures is meaningless.
pub(crate) fn validate_alert_confirmations(n: Option<u32>) -> Result<()> {
    if matches!(n, Some(0)) {
        return Err(AppError::bad_request_field(
            codes::INVALID_ALERT_CONFIG,
            "alert_confirmations must be >= 1",
            "alert_confirmations",
        ));
    }
    Ok(())
}

/// The one definition of a tag list, whichever front door it came through:
/// trimmed, de-duplicated, and bounded. A blank tag cannot be selected or
/// filtered on, and an invisible character in one reaches a confirmation prompt
/// and a terminal. Returns what to store, so every door stores the same thing
/// for the same request.
pub(crate) fn normalize_tags(tags: &[String]) -> Result<Vec<String>> {
    use crate::domain::target::{MAX_TAG_LEN, MAX_TAGS_PER_TARGET};

    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    // Positions, not values: an invisible character is the one fault the
    // operator cannot see in the tag the error would echo back.
    for (i, tag) in tags.iter().enumerate() {
        let nth = i + 1;
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(AppError::bad_request_field(
                codes::INVALID_TAG,
                format!("tag {nth} is blank"),
                "tags",
            ));
        }
        if tag
            .chars()
            .any(|c| c.is_control() || crate::domain::text::is_invisible(c))
        {
            return Err(AppError::bad_request_field(
                codes::INVALID_TAG,
                format!("tag {nth} contains a control or invisible character"),
                "tags",
            ));
        }
        if tag.chars().count() > MAX_TAG_LEN {
            return Err(AppError::bad_request_field(
                codes::TAG_TOO_LONG,
                format!("tag {nth} is longer than {MAX_TAG_LEN} characters"),
                "tags",
            ));
        }
        if !out.iter().any(|kept| kept == tag) {
            out.push(tag.to_string());
        }
    }
    if out.len() > MAX_TAGS_PER_TARGET {
        return Err(AppError::bad_request_field(
            codes::TOO_MANY_TAGS,
            format!("at most {MAX_TAGS_PER_TARGET} tags"),
            "tags",
        ));
    }
    Ok(out)
}

pub(crate) fn validate_group_name(group: Option<&str>) -> Result<()> {
    use crate::api::handlers::validation;
    if let Some(g) = group {
        validation::check_length(g, "group_name", 50, codes::GROUP_TOO_LONG)?;
    }
    Ok(())
}

/// `targets_owner_is_member_fk` refuses a non-member owner too; this turns that
/// into a 400 naming the field instead of a constraint violation.
pub(crate) async fn validate_owner_is_member(
    state: &AppState,
    org: OrgId,
    owner: Option<Uuid>,
) -> Result<()> {
    let Some(uid) = owner else { return Ok(()) };
    let pool = state.require_db()?;
    let members = crate::storage::orgs::list_members(pool, org).await?;
    if !members.iter().any(|m| m.membership.user_id.0 == uid) {
        return Err(AppError::bad_request_field(
            codes::OWNER_NOT_MEMBER,
            format!("owner_user_id {uid} is not a member of this org"),
            "owner_user_id",
        ));
    }
    Ok(())
}

/// Structural-only (no I/O): reject duplicate channel bindings. The bound
/// `channel_id` is checked to exist in the caller's org by the async
/// [`verify_alert_channels`] — kept separate so the sync per-item path
/// (`validate_new_target`, also used by bulk) stays sync.
pub(crate) fn validate_alerts(alerts: &TargetAlerts) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for (i, b) in alerts.iter().enumerate() {
        // A monitor delivers each open/resolve once per bound channel; a
        // duplicate binding would just double-page the same channel.
        if !seen.insert(b.channel_id) {
            return Err(AppError::bad_request_field(
                codes::INVALID_ALERT_CONFIG,
                format!(
                    "alerts[{i}]: duplicate binding for channel {}",
                    b.channel_id
                ),
                format!("alerts[{i}].channel_id"),
            ));
        }
    }
    Ok(())
}

const MAX_QUORUM: u32 = 64;

/// `any`/`majority`/`all` (and `None`) always pass — they track the live region
/// count. A fixed `count` must be in `1..=min(64, region_count)`; a count larger
/// than the regions that exist can never be met.
pub(crate) fn validate_region_policy(
    policy: Option<RegionIncidentPolicy>,
    region_count: usize,
) -> Result<()> {
    let max = (region_count as u32).clamp(1, MAX_QUORUM);
    match policy {
        Some(RegionIncidentPolicy::Count(n)) if !(1..=max).contains(&n) => {
            Err(AppError::unprocessable(
                codes::INVALID_REGION_POLICY,
                format!("region count must be between 1 and {max}"),
            ))
        }
        _ => Ok(()),
    }
}

/// The default region set for a new monitor: the regions flagged
/// `default_selected`, capped at the plan's `max_regions` (default region kept
/// first when the cap bites), so the default can never exceed the quota. An
/// opted-out region stays pickable on the form, just unchecked.
pub(crate) fn default_region_set(
    preferred: Vec<String>,
    max_regions: i32,
    default_region: &str,
) -> Vec<String> {
    let cap = max_regions.max(1) as usize;
    let set: Vec<String> = if preferred.len() <= cap {
        preferred
    } else {
        // The control plane's own region leads when the cap bites, but only if
        // it is a default at all: an opted-out region must not seed itself back
        // in and displace one the operator actually chose.
        let mut v: Vec<String> = match preferred.iter().any(|r| r == default_region) {
            true => vec![default_region.to_string()],
            false => Vec::new(),
        };
        for r in preferred {
            if r != default_region && v.len() < cap {
                v.push(r);
            }
        }
        v
    };
    if set.is_empty() {
        vec![default_region.to_string()]
    } else {
        set
    }
}

/// A set that cleans down to nothing is refused, not defaulted: quietly probing
/// somewhere else is the wrong answer to a typo.
pub(crate) fn normalize_region_ids(requested: &[String]) -> Result<Vec<String>> {
    let mut regions: Vec<String> = requested
        .iter()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    regions.sort();
    regions.dedup();
    if regions.is_empty() {
        return Err(AppError::unprocessable(
            codes::REGION_INVALID,
            "at least one region is required",
        ));
    }
    Ok(regions)
}

/// Shared so naming regions at create time and setting them afterwards cannot
/// drift into two ideas of a valid set. Existence before the cap: a misspelt
/// set must not read as a quota hit, nor be audited as one.
pub(crate) async fn vet_requested_regions(
    state: &AppState,
    org: OrgId,
    requested: &[String],
    snapshot: &RegionSnapshot,
) -> Result<Vec<String>> {
    let regions = normalize_region_ids(requested)?;
    if let Some(bad) = regions.iter().find(|r| !snapshot.available.contains(r)) {
        return Err(AppError::unprocessable(
            codes::REGION_INVALID,
            format!("unknown or disabled region: {bad}"),
        ));
    }
    state
        .quotas
        .check_region_assignment(org, None, regions.len() as i64)
        .await?;
    Ok(regions)
}

/// Reject a binding to a channel the caller's org doesn't own (the store is
/// org-scoped, so a foreign or deleted id resolves to `None`). Closes the
/// IDOR where a target could otherwise reference another tenant's channel.
pub(crate) async fn verify_alert_channels(
    state: &AppState,
    org: OrgId,
    alerts: &TargetAlerts,
) -> Result<()> {
    if alerts.is_empty() {
        return Ok(());
    }
    // One batched org-scoped query (mirrors maintenance's
    // `validate_component_ids`) instead of N point lookups.
    let ids: Vec<uuid::Uuid> = alerts.iter().map(|b| b.channel_id).collect();
    let known = state
        .notification_channel_store
        .existing_channel_ids(org, &ids)
        .await?;
    if let Some(missing) = ids.iter().find(|id| !known.contains(id)) {
        return Err(AppError::bad_request_field(
            codes::INVALID_ALERT_CONFIG,
            format!("notification channel {missing} does not exist"),
            "alerts.channel_id",
        ));
    }
    Ok(())
}

/// An empty-string credential means "clear"; an omitted one is kept.
pub(crate) fn take_cleared_credentials(http: &mut crate::domain::HttpCheck) -> (bool, bool) {
    let cleared_basic = matches!(&http.basic_auth, Some((u, p)) if u.is_empty() && p.is_empty());
    let cleared_bearer = matches!(http.bearer_token.as_deref(), Some(""));
    if cleared_basic {
        http.basic_auth = None;
    }
    if cleared_bearer {
        http.bearer_token = None;
    }
    (cleared_basic, cleared_bearer)
}

/// A credential carries from the stored target only when omitted and not cleared.
pub(crate) fn carry_flags(
    http: &crate::domain::HttpCheck,
    cleared_basic: bool,
    cleared_bearer: bool,
) -> (bool, bool) {
    (
        http.basic_auth.is_none() && !cleared_basic,
        http.bearer_token.is_none() && !cleared_bearer,
    )
}

pub(crate) fn carry_credentials(
    http: &mut crate::domain::HttpCheck,
    stored: &crate::domain::HttpCheck,
    carry_basic: bool,
    carry_bearer: bool,
) {
    if carry_basic {
        http.basic_auth = stored.basic_auth.clone();
    }
    if carry_bearer {
        http.bearer_token = stored.bearer_token.clone();
    }
}

/// Carry each masked (`***`) fill value forward from the stored flow, matched by
/// selector, so an edit that leaves a login secret untouched keeps it. A
/// sentinel with no stored match survives and `validate_check` rejects it, which
/// tells the user to re-enter that value.
pub(crate) fn carry_flow_secrets(
    new: &mut crate::domain::FlowCheck,
    stored: &crate::domain::FlowCheck,
) {
    use crate::domain::FlowStep;
    for step in &mut new.steps {
        if let FlowStep::Fill { selector, value } = step
            && value == REDACTED
            && let Some(FlowStep::Fill { value: prev, .. }) = stored
                .steps
                .iter()
                .find(|s| matches!(s, FlowStep::Fill { selector: ss, .. } if ss == selector))
        {
            *value = prev.clone();
        }
    }
}

/// Normalises the host/domain of the check in place: IDN-encoded, ASCII
/// lowercase, trailing dot stripped. After this runs, downstream stores the
/// canonical form and every layer (circuit breaker, host throttle, RDAP
/// singleflight) keys on the same string regardless of how the user typed it.
/// HTTP URLs are skipped — `url::Url` already canonicalises hosts on parse.
/// Returns `400` when IDN encoding fails on a user-supplied host.
pub(crate) fn canonicalize_check(check: &mut crate::domain::CheckSpec) -> Result<()> {
    use crate::domain::CheckSpec;
    use crate::worker::host_throttle::canonical_host_strict;
    use std::net::IpAddr;
    fn canon_host(host: &mut String, field: &'static str, code: &'static str) -> Result<()> {
        let raw = std::mem::take(host);
        let unbracketed = crate::security::unbracket(&raw);
        // IPs bypass IDN, but not canonicalisation: `2001:db8::1` and
        // `2001:db8:0:0::1` are one address written two ways, and storing them
        // verbatim gave each its own breaker and throttle bucket. Displaying
        // the parsed address collapses the spellings, lowercase included.
        if let Ok(ip) = unbracketed.parse::<IpAddr>() {
            *host = ip.to_string();
            return Ok(());
        }
        match canonical_host_strict(unbracketed) {
            Ok(canon) => {
                *host = canon;
                Ok(())
            }
            Err(_) => Err(AppError::bad_request_field(
                code,
                format!("host '{raw}' is not a valid IDN domain"),
                field,
            )),
        }
    }
    match check {
        // Host lives in the URL, already normalized on parse.
        CheckSpec::Http(_) | CheckSpec::Heartbeat(_) | CheckSpec::Flow(_) => Ok(()),
        CheckSpec::Tcp(tcp) => canon_host(&mut tcp.host, "check.host", codes::INVALID_TCP_HOST),
        CheckSpec::Ping(p) => canon_host(&mut p.host, "check.host", codes::INVALID_PING_HOST),
        CheckSpec::TlsCert(cert) => {
            canon_host(&mut cert.host, "check.host", codes::INVALID_TLS_CERT_PARAMS)
        }
        CheckSpec::DomainExpiry(d) => {
            canon_host(&mut d.domain, "check.domain", codes::INVALID_DOMAIN_PARAMS)
        }
        CheckSpec::Dns(d) => {
            canon_host(&mut d.domain, "check.domain", codes::INVALID_DNS_PARAMS)?;
            Ok(())
        }
    }
}

// Zero = instant fail; the cap stops a tenant hogging a worker slot.
pub(crate) fn validate_timeout(timeout: std::time::Duration) -> Result<()> {
    if !(100..=60_000).contains(&timeout.as_millis()) {
        return Err(AppError::bad_request_field(
            codes::INVALID_TIMEOUT,
            "timeout must be between 100 and 60000 ms",
            "check.timeout",
        ));
    }
    Ok(())
}

// Bound the alert lead-time window; warn must stay above critical.
pub(crate) fn validate_cert_days(
    warn_days: u32,
    critical_days: u32,
    code: &'static str,
) -> Result<()> {
    for (val, field) in [
        (warn_days, "check.warn_days"),
        (critical_days, "check.critical_days"),
    ] {
        if !(1..=365).contains(&val) {
            return Err(AppError::bad_request_field(
                code,
                "warn_days and critical_days must be between 1 and 365",
                field,
            ));
        }
    }
    if warn_days <= critical_days {
        return Err(AppError::bad_request_field(
            code,
            "warn_days must be > critical_days",
            "check.warn_days",
        ));
    }
    Ok(())
}

pub(crate) fn validate_check(check: &crate::domain::CheckSpec, guard: &SsrfGuard) -> Result<()> {
    use crate::domain::CheckSpec;
    match check {
        CheckSpec::Http(http) => {
            let scheme = http.url.scheme();
            if !ALLOWED_SCHEMES.contains(&scheme) {
                return Err(AppError::bad_request_field(
                    codes::INVALID_URL_SCHEME,
                    format!("url scheme '{scheme}' not allowed"),
                    "check.url",
                ));
            }
            validate_timeout(http.timeout)?;
            if http.max_redirects > crate::domain::HttpCheck::MAX_REDIRECTS {
                return Err(AppError::bad_request_field(
                    codes::INVALID_HTTP_PARAMS,
                    format!(
                        "max_redirects must be at most {}",
                        crate::domain::HttpCheck::MAX_REDIRECTS
                    ),
                    "check.max_redirects",
                ));
            }
            if let Some((u, p)) = &http.basic_auth
                && (u == REDACTED || p == REDACTED)
            {
                return Err(AppError::bad_request_field(
                    codes::REDACTION_SENTINEL,
                    "basic_auth contains redaction sentinel — re-supply the real credential",
                    "check.basic_auth",
                ));
            }
            if http.bearer_token.as_deref() == Some(REDACTED) {
                return Err(AppError::bad_request_field(
                    codes::REDACTION_SENTINEL,
                    "bearer_token contains redaction sentinel — re-supply the real credential",
                    "check.bearer_token",
                ));
            }
            if http.method == crate::domain::HttpMethod::Head
                && http.expected_body_contains.is_some()
            {
                return Err(AppError::bad_request_field(
                    codes::INVALID_HEAD_BODY_MATCH,
                    "expected_body_contains cannot be combined with method=HEAD (HEAD responses carry no body)",
                    "check.expected_body_contains",
                ));
            }
            // Plain http already sends creds in the clear, so this rule only
            // protects the https + forged-cert MITM path that bypasses the
            // confidentiality the operator was relying on.
            if !http.verify_tls
                && scheme == "https"
                && (http.basic_auth.is_some() || http.bearer_token.is_some())
            {
                return Err(AppError::bad_request_field(
                    codes::INVALID_TLS_CRED_COMBO,
                    "verify_tls = false cannot be combined with basic_auth or bearer_token over https — credentials would be exposed to any host presenting a forged certificate",
                    "check.verify_tls",
                ));
            }
            // Port 0 is unroutable, and rejecting it everywhere keeps the
            // ping throttle's pseudo-port 0 collision-free across kinds.
            if http.url.port() == Some(0) {
                return Err(AppError::bad_request_field(
                    codes::INVALID_URL_FORMAT,
                    "url port must be > 0",
                    "check.url",
                ));
            }
            match http.url.host() {
                Some(Host::Ipv4(v4)) => check_ip(IpAddr::V4(v4), guard)?,
                Some(Host::Ipv6(v6)) => check_ip(IpAddr::V6(v6), guard)?,
                Some(Host::Domain("")) => {
                    return Err(AppError::bad_request_field(
                        codes::INVALID_URL_FORMAT,
                        "url missing host",
                        "check.url",
                    ));
                }
                Some(Host::Domain(_)) => {}
                None => {
                    return Err(AppError::bad_request_field(
                        codes::INVALID_URL_FORMAT,
                        "url missing host",
                        "check.url",
                    ));
                }
            }
        }
        CheckSpec::Tcp(tcp) => {
            if tcp.host.is_empty() {
                return Err(AppError::bad_request_field(
                    codes::INVALID_TCP_HOST,
                    "tcp host required",
                    "check.host",
                ));
            }
            if tcp.port == 0 {
                return Err(AppError::bad_request_field(
                    codes::INVALID_TCP_PORT,
                    "tcp port must be > 0",
                    "check.port",
                ));
            }
            validate_timeout(tcp.timeout)?;
            let host = crate::security::unbracket(&tcp.host);
            if let Ok(ip) = host.parse::<IpAddr>() {
                check_ip(ip, guard)?;
            }
        }
        CheckSpec::Ping(p) => {
            if p.host.is_empty() {
                return Err(AppError::bad_request_field(
                    codes::INVALID_PING_HOST,
                    "ping host required",
                    "check.host",
                ));
            }
            validate_timeout(p.timeout)?;
            let host = crate::security::unbracket(&p.host);
            if let Ok(ip) = host.parse::<IpAddr>() {
                check_ip(ip, guard)?;
            }
        }
        CheckSpec::Heartbeat(h) => {
            // Sub-minute periods can't be judged by once-a-minute evaluation;
            // the 30-day ceiling keeps the maths in range.
            const MAX_MS: u128 = 30 * 24 * 3_600 * 1_000;
            if !(60_000..=MAX_MS).contains(&h.period.as_millis()) {
                return Err(AppError::bad_request_field(
                    codes::INVALID_HEARTBEAT_PARAMS,
                    "heartbeat period must be between 1 minute and 30 days",
                    "check.period",
                ));
            }
            if h.grace.as_millis() > MAX_MS {
                return Err(AppError::bad_request_field(
                    codes::INVALID_HEARTBEAT_PARAMS,
                    "heartbeat grace must be at most 30 days",
                    "check.grace",
                ));
            }
            // Same floor as the period: the evaluation that judges it runs no
            // finer than once a minute.
            if let Some(max) = h.max_runtime
                && !(60_000..=MAX_MS).contains(&max.as_millis())
            {
                return Err(AppError::bad_request_field(
                    codes::INVALID_HEARTBEAT_PARAMS,
                    "heartbeat max runtime must be between 1 minute and 30 days",
                    "check.max_runtime",
                ));
            }
        }
        CheckSpec::TlsCert(cert) => {
            if cert.host.is_empty() {
                return Err(AppError::bad_request_field(
                    codes::INVALID_TLS_CERT_PARAMS,
                    "tls_cert host required",
                    "check.host",
                ));
            }
            if cert.port == 0 {
                return Err(AppError::bad_request_field(
                    codes::INVALID_TLS_CERT_PARAMS,
                    "tls_cert port must be > 0",
                    "check.port",
                ));
            }
            validate_timeout(cert.timeout)?;
            validate_cert_days(
                cert.warn_days,
                cert.critical_days,
                codes::INVALID_TLS_CERT_PARAMS,
            )?;
            let host = crate::security::unbracket(&cert.host);
            if let Ok(ip) = host.parse::<IpAddr>() {
                check_ip(ip, guard)?;
            }
        }
        CheckSpec::DomainExpiry(d) => {
            if d.domain.is_empty() {
                return Err(AppError::bad_request_field(
                    codes::INVALID_DOMAIN_PARAMS,
                    "domain_expiry domain required",
                    "check.domain",
                ));
            }
            // Require at least one non-empty label on each side of the final
            // dot — rejects degenerate inputs like ".", ".a", "a." that would
            // pass a naive `.contains('.')` gate.
            let well_formed = d
                .domain
                .rsplit_once('.')
                .is_some_and(|(label, tld)| !label.is_empty() && !tld.is_empty());
            if !well_formed {
                return Err(AppError::bad_request_field(
                    codes::INVALID_DOMAIN_PARAMS,
                    "domain_expiry domain must be of the form 'name.tld'",
                    "check.domain",
                ));
            }
            // Accepting a check that can never succeed would alert forever.
            if let Some(tld) = d.domain.rsplit('.').next()
                && !crate::worker::registration::is_monitorable(&tld.to_ascii_lowercase())
            {
                return Err(AppError::bad_request_field(
                    codes::INVALID_DOMAIN_PARAMS,
                    format!(
                        "the .{} registry does not publish domain expiry dates, so this domain cannot be monitored for expiry",
                        tld.to_ascii_lowercase()
                    ),
                    "check.domain",
                ));
            }
            validate_timeout(d.timeout)?;
            validate_cert_days(d.warn_days, d.critical_days, codes::INVALID_DOMAIN_PARAMS)?;
        }
        CheckSpec::Dns(d) => {
            if d.domain.is_empty() {
                return Err(AppError::bad_request_field(
                    codes::INVALID_DNS_PARAMS,
                    "dns domain required",
                    "check.domain",
                ));
            }
            validate_timeout(d.timeout)?;
            if let Some(resolver) = &d.resolver
                && !resolver.is_empty()
            {
                let sock = crate::http_client::parse_resolver_addr(resolver).map_err(|_| {
                    AppError::bad_request_field(
                        codes::INVALID_DNS_PARAMS,
                        format!("dns resolver '{resolver}' must be an IP or ip:port"),
                        "check.resolver",
                    )
                })?;
                // A custom resolver address is just another outbound
                // target; reuse the SSRF guard so users can't aim the
                // probe at an internal DNS server.
                check_ip(sock.ip(), guard)?;
            }
        }
        CheckSpec::Flow(flow) => {
            use crate::domain::{FlowCheck, FlowStep};
            const FLOW: &str = codes::INVALID_FLOW_PARAMS;
            if !(1_000..=120_000).contains(&flow.timeout.as_millis()) {
                return Err(AppError::bad_request_field(
                    FLOW,
                    "flow timeout must be between 1000 and 120000 ms",
                    "check.timeout",
                ));
            }
            if !(100..=60_000).contains(&flow.step_timeout.as_millis()) {
                return Err(AppError::bad_request_field(
                    FLOW,
                    "flow step_timeout must be between 100 and 60000 ms",
                    "check.step_timeout",
                ));
            }
            if flow.steps.is_empty() {
                return Err(AppError::bad_request_field(
                    FLOW,
                    "flow requires at least one step",
                    "check.steps",
                ));
            }
            if flow.steps.len() > FlowCheck::MAX_STEPS {
                return Err(AppError::bad_request_field(
                    FLOW,
                    format!("flow allows at most {} steps", FlowCheck::MAX_STEPS),
                    "check.steps",
                ));
            }
            // No assertion → the flow can't fail, reporting Up even when login is
            // broken; require an explicit success signal.
            let asserts = flow
                .steps
                .iter()
                .any(|s| matches!(s, FlowStep::AssertText { .. } | FlowStep::AssertUrl { .. }));
            if !asserts {
                return Err(AppError::bad_request_field(
                    FLOW,
                    "flow requires at least one assert_text or assert_url step",
                    "check.steps",
                ));
            }
            validate_flow_url(&flow.start_url, guard)?;
            for (i, step) in flow.steps.iter().enumerate() {
                // 1-based: the form numbers the rows the reader is looking at.
                let n = i + 1;
                match step {
                    FlowStep::Goto { url } => validate_flow_url(url, guard)?,
                    FlowStep::Click { selector } | FlowStep::WaitFor { selector } => {
                        require_nonempty_step(selector, n, "selector")?
                    }
                    FlowStep::Fill { selector, value } => {
                        require_nonempty_step(selector, n, "selector")?;
                        if value == REDACTED {
                            return Err(AppError::bad_request_field(
                                codes::REDACTION_SENTINEL,
                                format!(
                                    "step {n}: fill value contains redaction sentinel — re-supply the real value"
                                ),
                                "check.steps",
                            ));
                        }
                    }
                    FlowStep::AssertText { selector, contains } => {
                        if let Some(sel) = selector {
                            require_nonempty_step(sel, n, "selector")?;
                        }
                        require_nonempty_step(contains, n, "expected text")?;
                    }
                    FlowStep::AssertUrl { contains } => {
                        require_nonempty_step(contains, n, "expected URL fragment")?
                    }
                }
            }
        }
    }
    Ok(())
}

/// Save-time scheme + host + SSRF gate for a flow's nav URLs, mirroring the HTTP
/// gate. Runtime egress is separately sandboxed in the engine.
pub(crate) fn validate_flow_url(url: &url::Url, guard: &SsrfGuard) -> Result<()> {
    let scheme = url.scheme();
    if !ALLOWED_SCHEMES.contains(&scheme) {
        return Err(AppError::bad_request_field(
            codes::INVALID_URL_SCHEME,
            format!("url scheme '{scheme}' not allowed"),
            "check.url",
        ));
    }
    if url.port() == Some(0) {
        return Err(AppError::bad_request_field(
            codes::INVALID_URL_FORMAT,
            "url port must be > 0",
            "check.url",
        ));
    }
    match url.host() {
        Some(Host::Ipv4(v4)) => check_ip(IpAddr::V4(v4), guard),
        Some(Host::Ipv6(v6)) => check_ip(IpAddr::V6(v6), guard),
        Some(Host::Domain(d)) if !d.is_empty() => Ok(()),
        _ => Err(AppError::bad_request_field(
            codes::INVALID_URL_FORMAT,
            "url missing host",
            "check.url",
        )),
    }
}

/// Names the row and field, so the author knows which step to go fix.
pub(crate) fn require_nonempty_step(value: &str, n: usize, what: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(AppError::bad_request_field(
            codes::INVALID_FLOW_PARAMS,
            format!("step {n}: {what} must not be empty"),
            "check.steps",
        ));
    }
    Ok(())
}

pub(crate) fn check_ip(ip: IpAddr, guard: &SsrfGuard) -> Result<()> {
    guard.check(ip).map_err(|err| {
        AppError::bad_request_field(codes::SSRF_BLOCKED, err.to_string(), "check.url")
    })
}
