use super::text::sanitize_data;
use super::view::{channel_names, region_policy_str, sorted, tag_list};
use crate::mcp::error::config_error;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::domain::notification_channel::NotificationChannel;
use crate::domain::public::IncidentStatusPhase;
use crate::domain::target::{RegionIncidentPolicy, Target, TargetUpdate};
use crate::domain::{AlertBinding, CheckSpec, ExpectedStatus, TargetAlerts};
use crate::storage::TimeRange;

use crate::mcp::error::McpToolError;
use crate::mcp::schema::{
    FieldChange, NewCheck, RegionPolicyArg, RegionPolicyMode, UpdateMonitorArgs,
};

pub(super) const DEFAULT_INCIDENT_WINDOW_DAYS: i64 = 30;
/// Widest window `list_incidents` will accept, so a far-past `from` can't turn
/// one tool call into a full-table scan.
pub(super) const MAX_INCIDENT_WINDOW_DAYS: i64 = 366;

/// The patch to apply plus one [`FieldChange`] per field that moves. A field
/// sent with the value it already has is dropped from both.
pub(super) fn build_monitor_patch(
    args: &UpdateMonitorArgs,
    target: &Target,
    channels: &[NotificationChannel],
    failure_limit: u32,
) -> Result<(TargetUpdate, Vec<FieldChange>), McpToolError> {
    let mut update = TargetUpdate::default();
    let mut changes = Vec::new();
    let mut moved = |field: &str, from: String, to: String| {
        changes.push(FieldChange {
            field: field.to_string(),
            from,
            to,
        });
    };

    if let Some(secs) = args.interval_secs
        && secs != target.interval.as_secs()
    {
        fits_i32(secs, "interval_secs")?;
        moved(
            "interval_secs",
            target.interval.as_secs().to_string(),
            secs.to_string(),
        );
        update.interval = Some(std::time::Duration::from_secs(secs));
    }
    if let Some(n) = args.alert_confirmations
        && n != target.alert_confirmations
    {
        fits_i32(u64::from(n), "alert_confirmations")?;
        moved(
            "alert_confirmations",
            target.alert_confirmations.to_string(),
            n.to_string(),
        );
        update.alert_confirmations = Some(n);
    }
    if let Some(on) = args.notify_recovery
        && on != target.notify_recovery
    {
        moved(
            "notify_recovery",
            target.notify_recovery.to_string(),
            on.to_string(),
        );
        update.notify_recovery = Some(on);
    }
    if let Some(secs) = args.renotify_interval_secs
        && secs != target.renotify_interval_secs
    {
        fits_i32(u64::from(secs), "renotify_interval_secs")?;
        moved(
            "renotify_interval_secs",
            target.renotify_interval_secs.to_string(),
            secs.to_string(),
        );
        update.renotify_interval_secs = Some(secs);
    }
    if let Some(tags) = args.tags.as_ref() {
        let tags = crate::api::handlers::targets::normalize_tags(tags).map_err(config_error)?;
        if sorted(&tags) != sorted(&target.tags) {
            moved("tags", tag_list(&target.tags), tag_list(&tags));
            update.tags = Some(tags);
        }
    }
    if let Some(group) = args.group_name.as_ref() {
        let group = match group.as_deref().map(str::trim) {
            Some("") => {
                return Err(McpToolError::invalid_argument(
                    "group_name must not be blank; send null to clear it",
                ));
            }
            other => other.map(str::to_string),
        };
        if group != target.group_name {
            let shown = |g: &Option<String>| {
                g.as_deref()
                    .map(sanitize_data)
                    .unwrap_or("none".to_string())
            };
            moved("group_name", shown(&target.group_name), shown(&group));
            update.group_name = Some(group);
        }
    }
    if let Some(policy) = args.region_policy.as_ref() {
        let policy = parse_region_policy(policy)?;
        if policy != target.region_policy {
            moved(
                "region_policy",
                region_policy_str(target.region_policy),
                region_policy_str(policy),
            );
            update.region_policy = Some(policy);
        }
    }
    let rebound = match args.channel_ids.as_ref() {
        Some(ids) => Some(resolve_bindings(ids, channels)?),
        None => None,
    };
    // A set, not a sequence: the same channels in another order alert the same
    // people, and calling that a change spends a confirmation on one.
    let rebinds = rebound
        .as_ref()
        .is_some_and(|a| bound_ids(a) != bound_ids(&target.alerts));
    // Who pages this monitor after the patch, however it moved: a retag alone
    // hands coverage from one channel to another, and a confirmation that
    // showed only the tags would have the human approve that unseen.
    let names = |a: &TargetAlerts, tags: &[String]| {
        channel_names(a, tags, channels, failure_limit).unwrap_or_else(|| "nobody".to_string())
    };
    let before = names(&target.alerts, &target.tags);
    let after = names(
        rebound
            .as_ref()
            .filter(|_| rebinds)
            .unwrap_or(&target.alerts),
        update.tags.as_deref().unwrap_or(&target.tags),
    );
    if before != after {
        moved("alerts", before, after);
    }
    if rebinds {
        update.alerts = rebound;
    }
    Ok((update, changes))
}

/// The cadence a caller gets for omitting one: where the app's own picker opens
/// a monitor of this kind, raised to the plan floor. The hard minimum would be
/// legal but far noisier, probing a certificate twelve times more often than
/// any other front door does. A heartbeat gets the cadence its window calls
/// for, since a coarser tick only delays the alarm.
pub(super) fn default_interval_secs(check: &CheckSpec, plan_floor_secs: u64) -> u64 {
    let opening = plan_floor_secs.max(crate::domain::interval_hints_for_kind(check.kind()).default);
    match check.as_heartbeat() {
        // Never below the floor: a default the plan forbids would be refused
        // as an argument the caller never sent.
        Some(hb) => hb.evaluation_cadence().as_secs().max(plan_floor_secs),
        None => opening,
    }
}

/// Field names for the audit row on a call that failed before the diff existed.
pub(super) fn requested_fields(args: &UpdateMonitorArgs) -> Vec<&'static str> {
    [
        ("interval_secs", args.interval_secs.is_some()),
        ("alert_confirmations", args.alert_confirmations.is_some()),
        ("notify_recovery", args.notify_recovery.is_some()),
        (
            "renotify_interval_secs",
            args.renotify_interval_secs.is_some(),
        ),
        ("tags", args.tags.is_some()),
        ("group_name", args.group_name.is_some()),
        ("region_policy", args.region_policy.is_some()),
        ("channel_ids", args.channel_ids.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, sent)| sent.then_some(name))
    .collect()
}

/// The column is `integer`; an out-of-range count would wrap rather than fail.
pub(super) fn fits_i32(secs: u64, field: &str) -> Result<(), McpToolError> {
    if secs > i32::MAX as u64 {
        return Err(McpToolError::invalid_argument(format!(
            "{field} must be at most {} seconds",
            i32::MAX
        )));
    }
    Ok(())
}

pub(super) fn parse_region_policy(
    arg: &RegionPolicyArg,
) -> Result<RegionIncidentPolicy, McpToolError> {
    match arg.mode {
        RegionPolicyMode::Any => Ok(RegionIncidentPolicy::Any),
        RegionPolicyMode::Majority => Ok(RegionIncidentPolicy::Majority),
        RegionPolicyMode::All => Ok(RegionIncidentPolicy::All),
        RegionPolicyMode::Count => arg.count.map(RegionIncidentPolicy::Count).ok_or_else(|| {
            McpToolError::invalid_argument("region_policy mode `count` needs a `count`")
        }),
    }
}

/// Header names that must reference a variable rather than spell a credential
/// out: a pasted one echoes back in the transcript and lands in `check_spec` as
/// plaintext, where a variable would have been sealed.
pub(super) const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "cookie",
];

/// Past any real check, under what a runaway generation could bloat a spec to.
const MAX_HEADERS: usize = 30;

/// Body parameters that name a credential. A token endpoint's `client_secret`
/// is the same paste the header rule refuses, and refusing it in one field
/// while waving it through in the other would make the rule theatre.
const CREDENTIAL_BODY_PARAMS: &[&str] = &[
    "client_secret",
    "password",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
];

/// The value a body assigns to `param`, in either `param=v` or `"param": "v"`
/// form, is a literal rather than a `{{ key }}` reference.
///
/// Both boundaries are load-bearing. Without the left one `login_password`
/// would read as `password`; without the right one so would `passwordless=1`,
/// and — the case that actually bites — the *value* in `grant_type=password`,
/// whose own assignment is legitimate and whose real `password=` field a few
/// bytes later may well be a proper `{{ ref }}`.
fn pastes_credential(body: &str, param: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(param) {
        let start = from + at;
        let after = start + param.len();
        // `login_password` is not `password`: only a whole parameter counts.
        let bounded = !lower[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !bounded {
            from = after;
            continue;
        }
        // Only an assignment counts. The separator may sit behind a closing
        // quote (`"password": "v"`) or whitespace; anything else means this
        // occurrence names nothing.
        let rest = body[after..].trim_start();
        let rest = rest.strip_prefix('"').map_or(rest, |r| r.trim_start());
        let Some(rest) = rest.strip_prefix([':', '=']) else {
            from = after;
            continue;
        };
        let rest = rest.trim_start().trim_start_matches(['"', '\'']);
        if !rest.is_empty() && !rest.starts_with("{{") {
            return true;
        }
        from = after;
    }
    false
}

fn check_body(body: Option<&String>) -> Result<Option<String>, McpToolError> {
    let Some(body) = body else {
        return Ok(None);
    };
    if let Some(param) = CREDENTIAL_BODY_PARAMS
        .iter()
        .find(|p| pastes_credential(body, p))
    {
        return Err(McpToolError::invalid_argument(format!(
            "the body sets `{param}` to a literal value, so it must reference an org variable \
             instead, as `{param}={{{{ my_key }}}}`. Call list_variables for the keys this org \
             has, and add the variable in the app if it is missing. Do not paste the credential \
             here"
        )));
    }
    Ok(Some(body.clone()))
}

/// Exactly an optional scheme word plus one `{{ key }}`, as `Bearer {{ k }}`.
/// Anything looser in a credential header we cannot tell from a pasted secret.
fn is_variable_reference(value: &str) -> bool {
    let v = value.trim();
    let Some(open) = v.find("{{") else {
        return false;
    };
    if !v.ends_with("}}") {
        return false;
    }
    let scheme = v[..open].trim_end();
    if !scheme.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let key = &v[open + 2..v.len() - 2];
    // One reference, not a concatenation of several.
    !key.contains("{{") && crate::domain::validate_var_key(key.trim()).is_ok()
}

fn check_headers(
    headers: Option<&std::collections::HashMap<String, String>>,
) -> Result<std::collections::HashMap<String, String>, McpToolError> {
    let Some(headers) = headers else {
        return Ok(Default::default());
    };
    if headers.len() > MAX_HEADERS {
        return Err(McpToolError::invalid_argument(format!(
            "at most {MAX_HEADERS} request headers"
        )));
    }
    let mut out = std::collections::HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name = name.trim();
        if hyper::header::HeaderName::try_from(name).is_err() {
            return Err(McpToolError::invalid_argument(format!(
                "`{name}` is not a valid header name"
            )));
        }
        if hyper::header::HeaderValue::try_from(value.as_str()).is_err() {
            return Err(McpToolError::invalid_argument(format!(
                "the value of `{name}` is not a valid header value"
            )));
        }
        let lower = name.to_ascii_lowercase();
        if CREDENTIAL_HEADERS.contains(&lower.as_str()) && !is_variable_reference(value) {
            return Err(McpToolError::invalid_argument(format!(
                "`{name}` carries a credential, so it must reference an org variable rather than \
                 spell one out, as `Bearer {{{{ my_key }}}}`. Call list_variables for the keys \
                 this org has, and add the variable in the app if it is missing. Do not paste the \
                 credential here"
            )));
        }
        out.insert(name.to_string(), value.clone());
    }
    Ok(out)
}

/// The narrow create surface widened into a real check, with the fields this
/// tool refuses to take left at their defaults.
pub(super) fn new_check_spec(check: &NewCheck) -> Result<CheckSpec, McpToolError> {
    use crate::domain::{
        DnsCheck, DomainExpiryCheck, HeartbeatCheck, HttpCheck, PingCheck, TcpCheck, TlsCertCheck,
    };
    use std::time::Duration;

    let ms = |v: Option<u64>, default: u64| Duration::from_millis(v.unwrap_or(default));
    Ok(match check {
        NewCheck::Http {
            url,
            method,
            expected_status,
            expected_body_contains,
            timeout_ms,
            follow_redirects,
            verify_tls,
            headers,
            body,
        } => {
            let url = url::Url::parse(url)
                .map_err(|e| McpToolError::invalid_argument(format!("url: {e}")))?;
            // Userinfo is a password by another name, and this tool refuses to
            // carry one. It would also be echoed back in the prompt and the
            // audit row, since `address` reports the URL as configured.
            if !url.username().is_empty() || url.password().is_some() {
                return Err(McpToolError::invalid_argument(
                    "url must not carry a username or password; add credentials to the monitor in the app",
                ));
            }
            let follow = follow_redirects.unwrap_or(true);
            CheckSpec::Http(HttpCheck {
                url,
                method: parse_http_method(method.as_deref())?,
                timeout: ms(*timeout_ms, 10_000),
                follow_redirects: follow,
                max_redirects: if follow { 5 } else { 0 },
                expected_status: parse_expected_status(expected_status.as_deref())?,
                expected_body_contains: expected_body_contains.clone(),
                headers: check_headers(headers.as_ref())?,
                body: check_body(body.as_ref())?,
                verify_tls: verify_tls.unwrap_or(true),
                basic_auth: None,
                bearer_token: None,
            })
        }
        NewCheck::Tcp {
            host,
            port,
            timeout_ms,
        } => CheckSpec::Tcp(TcpCheck {
            host: host.clone(),
            port: *port,
            timeout: ms(*timeout_ms, 5_000),
        }),
        NewCheck::Ping { host, timeout_ms } => CheckSpec::Ping(PingCheck {
            host: host.clone(),
            timeout: ms(*timeout_ms, 5_000),
        }),
        NewCheck::Dns {
            domain,
            record_type,
            resolver,
            expected_contains,
            timeout_ms,
        } => CheckSpec::Dns(DnsCheck {
            domain: domain.clone(),
            record_type: parse_record_type(record_type.as_deref())?,
            resolver: resolver.clone(),
            expected_contains: expected_contains.clone(),
            timeout: ms(*timeout_ms, 5_000),
        }),
        NewCheck::TlsCert {
            host,
            port,
            warn_days,
            critical_days,
            timeout_ms,
        } => CheckSpec::TlsCert(TlsCertCheck {
            host: host.clone(),
            port: port.unwrap_or(443),
            server_name: None,
            warn_days: warn_days.unwrap_or(30),
            critical_days: critical_days.unwrap_or(7),
            timeout: ms(*timeout_ms, 10_000),
        }),
        NewCheck::DomainExpiry {
            domain,
            warn_days,
            critical_days,
            timeout_ms,
        } => CheckSpec::DomainExpiry(DomainExpiryCheck {
            domain: domain.clone(),
            warn_days: warn_days.unwrap_or(30),
            critical_days: critical_days.unwrap_or(7),
            timeout: ms(*timeout_ms, 10_000),
        }),
        NewCheck::Heartbeat {
            period_secs,
            grace_secs,
            max_runtime_secs,
        } => CheckSpec::Heartbeat(HeartbeatCheck {
            period: Duration::from_secs(*period_secs),
            grace: Duration::from_secs(*grace_secs),
            max_runtime: max_runtime_secs.map(Duration::from_secs),
        }),
    })
}

pub(super) fn parse_http_method(
    method: Option<&str>,
) -> Result<crate::domain::HttpMethod, McpToolError> {
    use crate::domain::HttpMethod;
    Ok(
        match method.unwrap_or("get").to_ascii_lowercase().as_str() {
            "get" => HttpMethod::Get,
            "head" => HttpMethod::Head,
            "post" => HttpMethod::Post,
            "put" => HttpMethod::Put,
            "patch" => HttpMethod::Patch,
            "delete" => HttpMethod::Delete,
            "options" => HttpMethod::Options,
            other => {
                return Err(McpToolError::invalid_argument(format!(
                    "unknown method `{other}`; expected one of get, head, post, put, patch, delete, options"
                )));
            }
        },
    )
}

pub(super) fn parse_record_type(
    kind: Option<&str>,
) -> Result<crate::domain::DnsRecordType, McpToolError> {
    use crate::domain::DnsRecordType;
    Ok(match kind.unwrap_or("a").to_ascii_lowercase().as_str() {
        "a" => DnsRecordType::A,
        "aaaa" => DnsRecordType::Aaaa,
        "cname" => DnsRecordType::Cname,
        "mx" => DnsRecordType::Mx,
        "ns" => DnsRecordType::Ns,
        "txt" => DnsRecordType::Txt,
        "soa" => DnsRecordType::Soa,
        "ptr" => DnsRecordType::Ptr,
        "caa" => DnsRecordType::Caa,
        "srv" => DnsRecordType::Srv,
        other => {
            return Err(McpToolError::invalid_argument(format!(
                "unknown record_type `{other}`; expected one of a, aaaa, cname, mx, ns, txt, soa, ptr, caa, srv"
            )));
        }
    })
}

/// `200`, `200-299`, or `200,201,204`. The inverse of `expected_status_str`.
pub(super) fn parse_expected_status(spec: Option<&str>) -> Result<ExpectedStatus, McpToolError> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(ExpectedStatus::Range { min: 200, max: 299 });
    };
    let code = |s: &str| -> Result<u16, McpToolError> {
        s.trim().parse::<u16>().map_err(|_| {
            McpToolError::invalid_argument(format!(
                "expected_status `{spec}` is not a code, a range like 200-299, or a list like 200,201"
            ))
        })
    };
    if let Some((lo, hi)) = spec.split_once('-') {
        let (min, max) = (code(lo)?, code(hi)?);
        if min > max {
            return Err(McpToolError::invalid_argument(format!(
                "expected_status range `{spec}` starts above where it ends"
            )));
        }
        return Ok(ExpectedStatus::Range { min, max });
    }
    if spec.contains(',') {
        let codes = spec
            .split(',')
            .map(code)
            .collect::<Result<Vec<_>, McpToolError>>()?;
        return Ok(ExpectedStatus::OneOf(codes));
    }
    Ok(ExpectedStatus::Exact(code(spec)?))
}

/// Resolve a requested region against what the monitor is actually assigned to.
/// Naming the valid ids beats an empty answer, which reads as "healthy there".
pub(super) fn requested_region(
    requested: Option<&str>,
    assigned: &[String],
) -> Result<Option<String>, McpToolError> {
    let Some(region) = requested.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if !assigned.iter().any(|a| a == region) {
        return Err(McpToolError::invalid_argument(if assigned.is_empty() {
            "this monitor runs in no probe region, so it cannot be filtered by one".to_string()
        } else {
            format!(
                "monitor does not run in region `{}`; it runs in {}",
                sanitize_data(region),
                assigned.join(", ")
            )
        }));
    }
    Ok(Some(region.to_string()))
}

/// `open` (default) keeps only running incidents; `all` includes resolved ones.
pub(super) fn parse_incident_state_filter(state: Option<&str>) -> Result<bool, McpToolError> {
    match state {
        None | Some("open") => Ok(true),
        Some("all") => Ok(false),
        Some(other) => Err(McpToolError::invalid_argument(format!(
            "unknown state `{other}` (expected `open` or `all`)"
        ))),
    }
}

/// Resolve the caller's `from`/`to` into a bounded window: defaults to the
/// trailing [`DEFAULT_INCIDENT_WINDOW_DAYS`], and a span wider than
/// [`MAX_INCIDENT_WINDOW_DAYS`] is clamped by moving `from` forward.
pub(super) fn incident_window(
    from: Option<&str>,
    to: Option<&str>,
    now: DateTime<Utc>,
) -> Result<TimeRange, McpToolError> {
    let to = match to {
        Some(s) => parse_rfc3339(s, "to")?,
        None => now,
    };
    let from = match from {
        Some(s) => parse_rfc3339(s, "from")?,
        None => to - Duration::try_days(DEFAULT_INCIDENT_WINDOW_DAYS).unwrap_or_default(),
    };
    if from >= to {
        return Err(McpToolError::invalid_argument("`from` must be before `to`"));
    }
    let widest = Duration::try_days(MAX_INCIDENT_WINDOW_DAYS).unwrap_or_default();
    let from = from.max(to - widest);
    Ok(TimeRange { from, to })
}

pub(super) fn parse_rfc3339(value: &str, field: &str) -> Result<DateTime<Utc>, McpToolError> {
    DateTime::parse_from_rfc3339(value)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| {
            McpToolError::invalid_argument(format!("`{field}` must be an RFC 3339 timestamp"))
        })
}

pub(super) fn parse_uuid(s: &str, what: &str) -> Result<Uuid, McpToolError> {
    Uuid::parse_str(s).map_err(|_| McpToolError::invalid_argument(format!("invalid {what}")))
}

/// Window string → (span, latency bucket seconds). Bucket sizes target ~50-60
/// points across the window.
pub(super) fn parse_window(s: &str) -> Result<(Duration, u32), McpToolError> {
    let (hours, bucket) = match s {
        "1h" => (1, 60),
        "24h" => (24, 1_800),
        "7d" => (24 * 7, 10_800),
        "30d" => (24 * 30, 43_200),
        other => {
            return Err(McpToolError::invalid_argument(format!(
                "unknown window `{other}`; expected one of 1h, 24h, 7d, 30d"
            )));
        }
    };
    Ok((Duration::try_hours(hours).unwrap_or_default(), bucket))
}

/// Accepted monitor states for the `list_monitors` filter.
pub(super) fn parse_state(s: &str) -> Result<&'static str, McpToolError> {
    match s {
        "up" => Ok("up"),
        "down" => Ok("down"),
        "degraded" => Ok("degraded"),
        "error" => Ok("error"),
        "no_data" => Ok("no_data"),
        other => Err(McpToolError::invalid_argument(format!(
            "unknown state `{other}`; expected one of up, down, degraded, error, no_data"
        ))),
    }
}

/// Accepted incident phases for `post_incident_update`.
pub(super) fn parse_phase(s: &str) -> Result<IncidentStatusPhase, McpToolError> {
    match s {
        "investigating" => Ok(IncidentStatusPhase::Investigating),
        "identified" => Ok(IncidentStatusPhase::Identified),
        "monitoring" => Ok(IncidentStatusPhase::Monitoring),
        "resolved" => Ok(IncidentStatusPhase::Resolved),
        "postmortem" => Ok(IncidentStatusPhase::Postmortem),
        other => Err(McpToolError::invalid_argument(format!(
            "unknown phase `{other}`; expected one of investigating, identified, monitoring, resolved, postmortem"
        ))),
    }
}

/// Accepted monitor kinds for the `list_monitors` filter — derived from
/// `ALL_KINDS` so a new check kind is filterable without touching this file.
pub(super) fn parse_kind(s: &str) -> Result<&'static str, McpToolError> {
    crate::domain::CheckSpec::ALL_KINDS
        .into_iter()
        .find(|k| *k == s)
        .ok_or_else(|| {
            McpToolError::invalid_argument(format!(
                "unknown type `{s}`; expected one of {}",
                crate::domain::CheckSpec::ALL_KINDS.join(", ")
            ))
        })
}

fn bound_ids(alerts: &TargetAlerts) -> std::collections::BTreeSet<Uuid> {
    alerts.iter().map(|b| b.channel_id).collect()
}

/// Channel ids to bindings, refusing anything the org does not own. A duplicate
/// is an error rather than a silent collapse, matching the REST validator.
pub(super) fn resolve_bindings(
    ids: &[String],
    channels: &[NotificationChannel],
) -> Result<TargetAlerts, McpToolError> {
    let mut bindings: Vec<AlertBinding> = Vec::with_capacity(ids.len());
    for id in ids {
        let channel_id = parse_uuid(id, "channel id")?;
        // Absent from the org's own inventory covers both "does not exist" and
        // "belongs to someone else".
        if !channels.iter().any(|c| c.id == channel_id) {
            return Err(McpToolError::invalid_argument(format!(
                "no notification channel {channel_id} in this organization"
            )));
        }
        if bindings.iter().any(|b| b.channel_id == channel_id) {
            return Err(McpToolError::invalid_argument(format!(
                "notification channel {channel_id} is listed twice"
            )));
        }
        bindings.push(AlertBinding { channel_id });
    }
    Ok(TargetAlerts(bindings))
}

#[cfg(test)]
mod credential_body_tests {
    use super::pastes_credential;

    #[test]
    fn a_literal_secret_is_refused_and_a_reference_is_not() {
        assert!(pastes_credential("client_secret=hunter2", "client_secret"));
        assert!(!pastes_credential(
            "client_secret={{ oauth_secret }}",
            "client_secret"
        ));
        assert!(pastes_credential(
            r#"{"client_secret": "hunter2"}"#,
            "client_secret"
        ));
        assert!(!pastes_credential(
            r#"{"client_secret": "{{ oauth_secret }}"}"#,
            "client_secret"
        ));
    }

    #[test]
    fn only_an_assignment_counts_as_pasting_one() {
        // The OAuth password grant names the word twice: once as a value that
        // assigns nothing, once as the field, referenced properly.
        assert!(!pastes_credential(
            "grant_type=password&password={{ login_pw }}",
            "password"
        ));
        // ... and the same body with the real field pasted is still refused.
        assert!(pastes_credential(
            "grant_type=password&password=hunter2",
            "password"
        ));
        // A longer parameter and plain prose are not this field.
        assert!(!pastes_credential("passwordless=true", "password"));
        assert!(!pastes_credential(
            r#"{"msg":"forgot password"}"#,
            "password"
        ));
        // The left boundary still holds.
        assert!(!pastes_credential("login_password=hunter2", "password"));
    }
}
