use uuid::Uuid;

use crate::domain::{IncidentState, NotificationReason, UserId};

use super::PageTarget;

/// Strip the path/query/userinfo from any URL in a delivery error before it is
/// persisted to `incident_notifications.error`. A Slack webhook secret lives in
/// the path (`hooks.slack.com/services/T…/B…/<secret>`) and a Telegram bot
/// token in `…/bot<token>/…`; storing the raw transport error would leak them
/// at rest. Each whitespace token that parses as a URL is reduced to
/// `scheme://host[:port]`; everything else is kept verbatim.
pub(super) fn redact_secrets(msg: &str) -> String {
    msg.split_whitespace()
        .map(|tok| {
            if !tok.contains("://") {
                return tok.to_string();
            }
            let trimmed = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != ':' && c != '/');
            match url::Url::parse(trimmed) {
                Ok(u) if u.host_str().is_some() => {
                    let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
                    format!("{}://{}{}", u.scheme(), u.host_str().unwrap_or(""), port)
                }
                // Contains "://" but does not cleanly parse to a host — never
                // echo it verbatim (the secret-bearing path may survive); drop
                // the whole token.
                _ => "[redacted-url]".to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Delivery errors echo transport response bodies (up to the outbound read
/// cap). The DB error column stays org-scoped, but the shared log stream must
/// not carry tenant-endpoint-controlled bulk or recipient addresses, so the
/// logged copy is address-masked and clipped.
pub(super) fn log_error_snippet(error: &str) -> String {
    const MAX_CHARS: usize = 256;
    let masked = error
        .split_whitespace()
        .map(|tok| match tok.find('@') {
            Some(at) if at > 0 && tok[at + 1..].contains('.') => "[redacted-address]",
            _ => tok,
        })
        .collect::<Vec<_>>()
        .join(" ");
    if masked.chars().count() > MAX_CHARS {
        let clipped: String = masked.chars().take(MAX_CHARS).collect();
        format!("{clipped}…")
    } else {
        masked
    }
}

/// Exponential retry backoff in seconds after `attempt` just failed:
/// `base * 2^(attempt-1)` capped at one hour. `None` once `attempt` reaches
/// `max_attempts` — the row is dead-lettered, not rescheduled.
pub(super) fn retry_delay_secs(
    attempt: i32,
    base_secs: u64,
    cap_secs: u64,
    max_attempts: u32,
) -> Option<u64> {
    if attempt >= max_attempts as i32 {
        return None;
    }
    let base = base_secs.max(1);
    let shift = (attempt.max(1) - 1).min(16) as u32;
    Some(base.saturating_mul(1u64 << shift).min(cap_secs.max(base)))
}

/// `"retry_after":N` seconds from a delivery error (Telegram and Discord 429
/// bodies); string-scanned because the error is already flattened, capped so a
/// hostile body can't park a retry for days.
pub(super) fn retry_after_hint(error: Option<&str>) -> Option<chrono::Duration> {
    const MAX_HINT_SECS: i64 = 3600;
    // Discord's sub-second hints floor to zero, which costs nothing: the value
    // only ever raises a backoff that has a floor of its own.
    crate::notifier::json_int_field(error?, "retry_after")
        .filter(|secs| *secs >= 0)
        .map(|secs| chrono::Duration::seconds(secs.min(MAX_HINT_SECS)))
}

/// Wrap bare channel ids (the no-policy fallback + resolution paths) as page
/// targets with no attributed responder.
pub(super) fn channel_targets(channels: Vec<Uuid>) -> Vec<PageTarget> {
    channels
        .into_iter()
        .map(|channel_id| PageTarget {
            channel_id,
            user_id: None,
        })
        .collect()
}

/// Append a page target, deduped by channel so one channel is paged at most
/// once per rung. The first occurrence wins (it may carry a responder).
pub(super) fn push_target(out: &mut Vec<PageTarget>, channel_id: Uuid, user_id: Option<UserId>) {
    if out.iter().any(|t| t.channel_id == channel_id) {
        return;
    }
    out.push(PageTarget {
        channel_id,
        user_id,
    });
}

/// What one round of paging produced. `recorded` counts rows written, which a
/// failed delivery also produces and the retry sweep owns; `delivered` counts
/// the ones that actually reached a channel.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Paged {
    pub(super) recorded: u32,
    pub(super) delivered: u32,
}

/// Bookkeeping rows the engine writes to itself. None is a delivery.
pub fn is_damper_marker(transport: &str) -> bool {
    matches!(
        transport,
        DAMPED_TRANSPORT | RELEASED_TRANSPORT | UNREACHABLE_TRANSPORT | MAINTENANCE_TRANSPORT
    )
}

pub const DAMPED_TRANSPORT: &str = "damped";
pub const RELEASED_TRANSPORT: &str = "released";
pub const UNREACHABLE_TRANSPORT: &str = "unreachable";
pub const MAINTENANCE_TRANSPORT: &str = "maintenance";

/// Whether the damper runs on this path. A release already waited out the
/// hold, so re-damping it would silence the outage the hold exists to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Damper {
    Apply,
    Skip,
}

/// What the flap damper does with one monitor's opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlapState {
    Steady,
    /// Still pages: the operator learns of the flapping from the alert stream.
    Crossing,
    /// Recorded, not delivered.
    Damped,
}

/// `opens` counts the monitor's incident opens inside the flap window,
/// including the one being decided. `max` of 0 disables damping.
pub(super) fn flap_state(opens: u32, max: u32) -> FlapState {
    match max {
        0 => FlapState::Steady,
        _ if opens < max => FlapState::Steady,
        _ if opens == max => FlapState::Crossing,
        _ => FlapState::Damped,
    }
}

/// Is an outage already being paged? True when the most recent open-side page
/// (opened/reopened/escalated) is newer than the most recent resolution page —
/// i.e. we are inside an unresolved paging episode. Used to absorb duplicate
/// open signals without silencing a genuine reopen (which posts a Resolved row
/// first, ending the prior episode).
pub(super) fn open_episode_active(rows: &[crate::domain::IncidentNotification]) -> bool {
    let last_open = rows
        .iter()
        // A held open is not an episode in progress: counting it would make
        // the release look like a duplicate signal. Keyed on the marker, not
        // `channel_id` — deleting a channel NULLs that on real delivered rows.
        .filter(|n| !is_damper_marker(&n.transport))
        .filter(|n| {
            matches!(
                n.reason,
                NotificationReason::Opened
                    | NotificationReason::Reopened
                    | NotificationReason::Escalated
            )
        })
        .map(|n| n.created_at)
        .max();
    let last_resolved = rows
        .iter()
        .filter(|n| n.reason == NotificationReason::Resolved)
        .map(|n| n.created_at)
        .max();
    match (last_open, last_resolved) {
        (Some(o), Some(r)) => o > r,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Channels to send the all-clear to: every channel paged this episode that
/// has not already been sent a resolution newer than its last open-side page.
pub(super) fn resolvable_channels(rows: &[crate::domain::IncidentNotification]) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = Vec::new();
    let mut seen: Vec<Uuid> = Vec::new();
    for cid in rows.iter().filter_map(|n| n.channel_id) {
        if seen.contains(&cid) {
            continue;
        }
        seen.push(cid);
        let last_open = rows
            .iter()
            .filter(|n| {
                n.channel_id == Some(cid)
                    && matches!(
                        n.reason,
                        NotificationReason::Opened
                            | NotificationReason::Reopened
                            | NotificationReason::Escalated
                    )
            })
            .map(|n| n.created_at)
            .max();
        let Some(open_at) = last_open else { continue };
        let resolved_after = rows.iter().any(|n| {
            n.channel_id == Some(cid)
                && n.reason == NotificationReason::Resolved
                && n.created_at >= open_at
        });
        if !resolved_after {
            out.push(cid);
        }
    }
    out
}

/// A queued page becomes stale if the incident moved past the state it
/// describes before delivery succeeded: an outage notice once the incident is
/// resolved, or a recovery notice once it has reopened.
pub(super) fn reason_is_stale(reason: NotificationReason, state: IncidentState) -> bool {
    match reason {
        NotificationReason::Opened
        | NotificationReason::Reopened
        | NotificationReason::Escalated => state == IncidentState::Resolved,
        // A reminder only says "still unacknowledged", which an ack ends.
        NotificationReason::Reminder => state != IncidentState::Triggered,
        NotificationReason::Resolved => state != IncidentState::Resolved,
        NotificationReason::NoData | NotificationReason::DataResumed => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_queued_reminder_is_dropped_once_someone_acknowledges() {
        use IncidentState::{Acknowledged, Resolved, Triggered};
        assert!(!reason_is_stale(NotificationReason::Reminder, Triggered));
        assert!(reason_is_stale(NotificationReason::Reminder, Acknowledged));
        assert!(reason_is_stale(NotificationReason::Reminder, Resolved));
        // An opening page still lands on an acknowledged incident: the
        // responder took it, they did not learn about it twice.
        assert!(!reason_is_stale(NotificationReason::Opened, Acknowledged));
    }
}
