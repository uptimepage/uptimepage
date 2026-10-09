use super::MAX_INCIDENT_MESSAGE_LEN;
use crate::storage::LifecycleOutcome;

use rmcp::handler::server::wrapper::Json;

use crate::domain::target::{NewTarget, RegionIncidentPolicy};
use crate::domain::text::is_invisible;
use crate::domain::{CheckSpec, humanize_check_error};

use crate::mcp::error::McpToolError;
use crate::mcp::schema::{FieldChange, IncidentActionResult, ProbeOutcome};

/// Prompt-facing names; `changes` reports the machine names.
fn field_label(field: &str) -> &str {
    match field {
        "interval_secs" => "check interval (seconds)",
        "alert_confirmations" => "failing checks before alerting",
        "notify_recovery" => "announce recovery",
        "renotify_interval_secs" => "reminder interval (seconds)",
        "recovery_period_secs" => "recovery hold before closing (seconds)",
        "group_name" => "group",
        "alerts" => "notification channels",
        "region_policy" => "opens an incident on",
        "starts_at" => "start",
        "ends_at" => "end",
        "monitor_ids" => "monitors",
        "suppress_alerts" => "paging held",
        other => other,
    }
}

pub(super) fn change_lines(changes: &[FieldChange]) -> String {
    changes
        .iter()
        .map(|c| {
            format!(
                "{}: {} → {}",
                field_label(&c.field),
                sanitize_prompt(&c.from),
                sanitize_prompt(&c.to)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every setting the monitor would be created with. What the prompt leaves out
/// is approved unread, so nothing the caller chose is omitted here.
pub(super) fn create_prompt_lines(
    new: &NewTarget,
    regions: &[String],
    probe: Option<(&str, &ProbeOutcome)>,
    channel_summary: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![format!("checked every {}s", new.interval.as_secs())];
    // Part of what the check watches, so it is approved rather than discovered.
    if !new.check.is_passive() {
        lines.push(format!("probed from: {}", regions.join(", ")));
    }
    lines.push(match probe {
        // Named: the trial answers from one region, the monitor may hold several.
        Some((region, p)) => format!(
            "trial run from {}: {}",
            sanitize_prompt(region),
            sanitize_prompt(&probe_line(p))
        ),
        None if matches!(new.check, CheckSpec::Manual(_)) => {
            "nothing to probe: it starts up and stays so until someone sets its state".to_string()
        }
        None => "nothing to probe: it reports nothing and alerts nobody until the job's \
                 first ping"
            .to_string(),
    });
    if !new.tags.is_empty() {
        lines.push(format!("tags: {}", sanitize_prompt(&tag_list(&new.tags))));
    }
    if let Some(group) = &new.group_name {
        lines.push(format!("group: {}", sanitize_prompt(group)));
    }
    lines.push(match new.check {
        CheckSpec::Manual(_) => "alerts as soon as it is set down or degraded".to_string(),
        _ => format!("alerts after {} failing checks", new.alert_confirmations),
    });
    if !new.notify_recovery {
        lines.push("recovery is not announced".to_string());
    }
    lines.push(match new.renotify_interval_secs {
        0 => "no reminders while an outage is open".to_string(),
        secs => format!("first reminder after {secs}s, then doubling while unacknowledged"),
    });
    // A manual monitor closes on the state it is set to, said by its alert line.
    if !matches!(new.check, CheckSpec::Manual(_)) {
        lines.push(match new.recovery_period() {
            0 => "closes its incident once checks pass again".to_string(),
            secs => format!("closes its incident once checks have passed for {secs}s"),
        });
    }
    if let Some(policy) = new.region_policy {
        // A quorum wider than the assignment is clamped, so the label alone would
        // promise a patience the monitor will not have.
        lines.push(match new.check.is_passive() {
            true => format!("opens an incident on {}", region_policy_str(policy)),
            false => format!(
                "opens an incident on {} ({} of {} assigned regions)",
                region_policy_str(policy),
                policy.required(regions.len()),
                regions.len()
            ),
        });
    }
    lines.push(match channel_summary {
        Some(s) => format!("notification channels: {}", sanitize_prompt(s)),
        // A channel tag rule can still cover it, but naming one costs the
        // channel inventory this call did not ask for.
        None => "notification channels: none bound, so it alerts nobody unless a channel's \
                 tag rule covers its tags"
            .to_string(),
    });
    lines
}

pub(super) fn tag_list(tags: &[String]) -> String {
    if tags.is_empty() {
        "none".to_string()
    } else {
        tags.iter()
            .map(|t| sanitize_data(t))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(super) fn region_policy_str(policy: RegionIncidentPolicy) -> String {
    match policy {
        RegionIncidentPolicy::Any => "any region down".to_string(),
        RegionIncidentPolicy::Majority => "a majority of regions down".to_string(),
        RegionIncidentPolicy::All => "every region down".to_string(),
        RegionIncidentPolicy::Count(n) => format!("{n} regions down"),
    }
}

/// One line a human can judge the trial run by.
pub(super) fn probe_line(p: &ProbeOutcome) -> String {
    let head = match (p.state.as_str(), p.http_status) {
        ("up", Some(code)) => format!("passed, HTTP {code}"),
        ("up", None) => "passed".to_string(),
        (state, Some(code)) => format!("{state}, HTTP {code}"),
        (state, None) => state.to_string(),
    };
    let result = match &p.error {
        Some(err) => format!("{head} in {}ms — {err}", p.duration_ms),
        None => format!("{head} in {}ms", p.duration_ms),
    };
    match &p.diagnostic {
        Some(diagnostic) => format!("{result}; {}", diagnostic.summary),
        None => result,
    }
}

/// Cap on any one untrusted value in a confirmation prompt.
const PROMPT_CAP: usize = 200;

/// Neutralise untrusted text (customer monitor names, operator messages)
/// interpolated into a human confirmation prompt: drop what could spoof the
/// approval dialog and cap the length. The prompt's own structure (quotes,
/// newlines) is added around the sanitized value.
pub(super) fn sanitize_prompt(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .take(PROMPT_CAP + 1)
        .collect();
    // Silent truncation would hide, say, the tags a replacement is dropping.
    if out.chars().count() > PROMPT_CAP {
        out = out.chars().take(PROMPT_CAP).collect();
        out.push_str("... (truncated)");
    }
    out
}

/// Neutralise customer-supplied text returned to the model: drop characters that
/// could smuggle hidden instructions (tab and newline stay, they are legitimate
/// in error text) and cap length. The server instructions already label this as
/// data, not commands — this is belt-and-suspenders.
pub(super) fn sanitize_data(s: &str) -> String {
    s.chars()
        .filter(|c| (!c.is_control() && !is_invisible(*c)) || *c == '\n' || *c == '\t')
        .take(4000)
        .collect()
}

/// Humanize, then scrub — order matters so the scrub can't mangle our own copy.
pub(super) fn present_error(raw: &str) -> String {
    sanitize_data(&humanize_check_error(raw))
}

pub(super) fn clean_public_text(
    value: Option<&str>,
    field: &'static str,
    max: usize,
) -> Result<Option<String>, McpToolError> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) if v.chars().count() > max => Err(McpToolError::invalid_argument(format!(
            "{field} must be at most {max} characters"
        ))),
        Some(v) => Ok(Some(v.to_string())),
    }
}

/// Trim a blank incident note to `None`; reject one over the message cap.
pub(super) fn clean_incident_note(note: Option<&str>) -> Result<Option<String>, McpToolError> {
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        None => Ok(None),
        Some(n) if n.chars().count() > MAX_INCIDENT_MESSAGE_LEN => {
            Err(McpToolError::invalid_argument(format!(
                "note must be at most {MAX_INCIDENT_MESSAGE_LEN} characters"
            )))
        }
        Some(n) => Ok(Some(n.to_string())),
    }
}

/// Map a lifecycle store outcome onto the MCP action result.
pub(super) fn incident_action_result(
    id: uuid::Uuid,
    outcome: LifecycleOutcome,
) -> Result<Json<IncidentActionResult>, McpToolError> {
    match outcome {
        LifecycleOutcome::Updated(inc) => Ok(Json(IncidentActionResult {
            incident_id: id.to_string(),
            state: inc.state.as_db_str().to_string(),
            acknowledged_at: inc.acknowledged_at.map(|t| t.to_rfc3339()),
            resolved_at: inc.ended_at.map(|t| t.to_rfc3339()),
        })),
        LifecycleOutcome::NotFound => Err(McpToolError::not_found("incident not found")),
        LifecycleOutcome::IllegalTransition(err) => {
            Err(McpToolError::invalid_argument(err.to_string()))
        }
        LifecycleOutcome::Stale => Err(McpToolError::invalid_argument(
            "this incident has reopened since the action was prepared",
        )),
    }
}
