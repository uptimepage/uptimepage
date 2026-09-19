pub mod alert_channel_stop;
pub mod auth;
pub mod billing;
pub mod billing_hook;
pub mod connect_oauth;
pub mod coverage;
pub mod dashboard;
pub mod delegate_connect;
pub mod discord_connect;
pub mod escalation;
pub mod heartbeat;
pub mod help;
pub mod incident_ack;
pub mod incidents;
pub mod invitations;
pub mod legal;
pub mod nav;
pub mod notification_channels;
pub mod on_call;
pub mod organizations;
pub mod pages;
pub mod public_status;
pub mod region_display;
pub mod resend_hook;
pub mod share;
pub mod slack_connect;
pub mod subscribe;
pub mod targets_detail;
pub mod targets_form;
pub mod targets_list;
pub mod team;
pub mod telegram;
pub mod variables;
pub mod verify_channel;
pub mod whatsapp;

use std::fmt;

use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use serde::Serialize;

use crate::domain::{CheckSpec, OrgId};
use crate::error::AppError;
use crate::request::CurrentOrg;
use crate::web::error::WebError;

/// Shared range tab descriptor — the per-page handler builds a `Vec`
/// from its allowed key set, marking exactly one entry `selected`. One
/// source so the Console / Detail / Incidents tabs render identical
/// markup and the active tab can never silently double-fire.
pub struct RangeOption {
    pub key: &'static str,
    pub selected: bool,
}

/// Page-size option in a list footer. `hx_get` switches the link from a
/// full navigation to an htmx swap of the list region.
pub struct PageSizeLink {
    pub n: usize,
    pub href: String,
    pub hx_get: Option<String>,
    pub active: bool,
}

/// Prev/next link in a list footer.
pub struct PagerLink {
    pub label: &'static str,
    pub href: String,
    pub hx_get: Option<String>,
}

pub(crate) fn build_range_options(active: &'static str, keys: &[&'static str]) -> Vec<RangeOption> {
    keys.iter()
        .map(|k| RangeOption {
            key: k,
            selected: *k == active,
        })
        .collect()
}

/// Returns the matching key from `keys` if `raw` is one of them, else
/// `default`. Tiny but used by every page that exposes a `?range=` tab
/// strip — centralised so adding a new preset is one edit.
pub(crate) fn resolve_range_key(
    raw: Option<&str>,
    keys: &[&'static str],
    default: &'static str,
) -> &'static str {
    raw.and_then(|s| keys.iter().copied().find(|k| *k == s))
        .unwrap_or(default)
}

/// Resolve the caller's tenant for a `/settings/*` page exactly as the API
/// does. An *unauthenticated* hit bounces to login (so a bookmarked settings
/// URL works after sign-in); a Forbidden / DB error surfaces as the HTML
/// error page, never a misleading login loop. Shared by every settings view.
pub(crate) fn resolve_org(
    org: Result<CurrentOrg, AppError>,
    redirect_to: &str,
) -> Result<OrgId, Box<Response>> {
    match org {
        Ok(CurrentOrg(o)) => Ok(o),
        Err(AppError::Unauthorized) => Err(Box::new(
            crate::request::auth::login_redirect(redirect_to).into_response(),
        )),
        Err(e) => Err(Box::new(WebError::from(e).into_response())),
    }
}

/// Pretty-print a string map for a "headers (JSON object)" form field,
/// falling back to an empty object so the textarea is never blank/invalid.
pub(crate) fn json_pretty<T: Serialize>(m: &T) -> String {
    serde_json::to_string_pretty(m).unwrap_or_else(|_| "{}".into())
}

/// Maps a `CheckSpec` to a UI-friendly `(kind, address)` pair.
/// Used by the list and detail views; centralized so adding a new
/// check variant updates both call-sites.
pub(crate) fn describe_check(spec: &CheckSpec) -> (&'static str, String) {
    match spec {
        CheckSpec::Http(h) => ("HTTP", h.url.to_string()),
        CheckSpec::Tcp(c) => ("TCP", format!("{}:{}", c.host, c.port)),
        CheckSpec::Ping(c) => ("PING", c.host.clone()),
        CheckSpec::Heartbeat(c) => (
            "HEARTBEAT",
            format!(
                "ping every {} (+{} grace)",
                exact_duration(c.period.as_secs()),
                exact_duration(c.grace.as_secs())
            ),
        ),
        CheckSpec::TlsCert(c) => ("TLS", format!("{}:{}", c.host, c.port)),
        CheckSpec::DomainExpiry(c) => ("DOMAIN", c.domain.clone()),
        CheckSpec::Dns(c) => ("DNS", format!("{} {}", c.record_type.as_str(), c.domain)),
        CheckSpec::Flow(c) => ("FLOW", c.start_url.to_string()),
    }
}

/// UI label for a transport. The Telegram pair swap names on purpose: the
/// one-tap kind is the plain "telegram" customers expect.
pub(crate) fn channel_kind_label(kind: crate::domain::ChannelKind) -> &'static str {
    use crate::domain::ChannelKind;
    match kind {
        ChannelKind::Telegram => "telegram bot",
        ChannelKind::TelegramApp => "telegram",
        ChannelKind::WhatsApp => "whatsapp api",
        ChannelKind::WhatsAppApp => "whatsapp",
        ChannelKind::MsTeams => "teams",
        ChannelKind::GoogleChat => "google chat",
        other => other.as_db_str(),
    }
}

/// Exhaustive so a new kind cannot ship naming a symbol that does not exist.
pub(crate) fn channel_kind_icon(kind: crate::domain::ChannelKind) -> &'static str {
    use crate::domain::ChannelKind;
    match kind {
        ChannelKind::Slack => "slack",
        ChannelKind::Discord => "discord",
        ChannelKind::Email => "email",
        ChannelKind::Telegram | ChannelKind::TelegramApp => "telegram",
        ChannelKind::WhatsApp | ChannelKind::WhatsAppApp => "whatsapp",
        ChannelKind::MsTeams => "msteams",
        ChannelKind::GoogleChat => "google-chat",
        ChannelKind::PagerDuty => "pagerduty",
        ChannelKind::Pushover => "pushover",
        ChannelKind::Ntfy => "ntfy",
        ChannelKind::Gotify => "gotify",
        ChannelKind::Mattermost => "mattermost",
        ChannelKind::Sms => "sms",
        ChannelKind::Webhook => "webhook",
    }
}

pub(crate) fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Human-readable wall-clock UTC string, e.g. "2026-05-13 12:34 UTC".
/// Pair with `fmt_ts` (ISO 8601) for `<time datetime>` round-trips.
pub(crate) fn fmt_human(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Exact single-unit duration (`45s`, `5m`, `24h`, `30d`) for config values
/// that must round-trip — the lossy two-unit display lives in [`HumanDur`].
/// Days start at two, so a 24h interval stays hours.
pub(crate) fn exact_duration(secs: u64) -> String {
    if secs >= 172_800 && secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Two-unit duration string, e.g. `"45s"`, `"17m"`, `"2h 14m"`, `"1d 1h"`.
/// Negative durations clamp to zero.
pub(crate) fn humanize_duration(d: ChronoDuration) -> String {
    HumanDur(d.num_seconds()).to_string()
}

/// Display wrapper for [`humanize_duration`] that writes directly to a
/// `fmt::Formatter` instead of allocating an intermediate `String`. Cheap
/// to construct from the raw seconds the storage layer already returns.
pub struct HumanDur(pub i64);

impl fmt::Display for HumanDur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.0.max(0);
        if total < 60 {
            return write!(f, "{total}s");
        }
        let mins = total / 60;
        if mins < 60 {
            return write!(f, "{mins}m");
        }
        let hours = mins / 60;
        let rem_mins = mins % 60;
        if hours < 24 {
            if rem_mins == 0 {
                return write!(f, "{hours}h");
            }
            return write!(f, "{hours}h {rem_mins}m");
        }
        let days = hours / 24;
        let rem_hours = hours % 24;
        if rem_hours == 0 {
            write!(f, "{days}d")
        } else {
            write!(f, "{days}d {rem_hours}h")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The icon ids are plain strings the compiler cannot check against the
    /// sprite, so a kind pointing at a missing symbol has to fail here.
    #[test]
    fn every_channel_kind_has_a_symbol_in_the_sprite() {
        let sprite = include_str!("../../../templates/settings/_channel_icons.html");
        for kind in crate::domain::ChannelKind::ALL {
            let id = format!(r#"id="ci-{}""#, channel_kind_icon(*kind));
            assert!(
                sprite.contains(&id),
                "{kind:?} names a missing symbol: {id}"
            );
        }
    }

    #[test]
    fn humanize_duration_picks_largest_unit() {
        assert_eq!(humanize_duration(ChronoDuration::seconds(0)), "0s");
        assert_eq!(humanize_duration(ChronoDuration::seconds(45)), "45s");
        assert_eq!(humanize_duration(ChronoDuration::minutes(17)), "17m");
        assert_eq!(humanize_duration(ChronoDuration::minutes(134)), "2h 14m");
        assert_eq!(humanize_duration(ChronoDuration::hours(25)), "1d 1h");
        assert_eq!(humanize_duration(ChronoDuration::hours(48)), "2d");
        assert_eq!(humanize_duration(ChronoDuration::seconds(-5)), "0s");
    }

    #[test]
    fn exact_duration_reaches_days_without_moving_a_daily_interval() {
        assert_eq!(exact_duration(45), "45s");
        assert_eq!(exact_duration(300), "5m");
        assert_eq!(exact_duration(4_980), "83m");
        assert_eq!(exact_duration(86_400), "24h");
        assert_eq!(exact_duration(172_800), "2d");
        assert_eq!(exact_duration(2_592_000), "30d");
    }

    /// The form renders a stored duration with this, and `parseDuration` in
    /// check_form.js reads it back on submit. Anything outside the grammar that
    /// regex accepts would come back as a validation error on a field the
    /// customer never touched.
    #[test]
    fn every_rendered_duration_matches_the_grammar_the_form_parses() {
        for secs in [
            0, 1, 45, 59, 60, 90, 300, 4_980, 3_600, 43_200, 86_400, 172_800, 2_592_000,
        ] {
            let rendered = exact_duration(secs);
            let (digits, unit) = rendered.split_at(rendered.len() - 1);
            assert!(
                digits.chars().all(|c| c.is_ascii_digit()) && !digits.is_empty(),
                "{rendered} is not digits followed by a unit"
            );
            assert!(
                ["s", "m", "h", "d"].contains(&unit),
                "{rendered} ends in an unparseable unit"
            );
        }
    }
}
