pub mod account_deletion;
pub mod account_restored;
pub mod billing;
pub mod channel_failing;
pub mod channel_verification;
pub mod heartbeat_never_pinged;
pub mod identity_linked;
pub mod identity_unlinked;
pub mod incident_alert;
pub mod invitation;
pub mod layout;
pub mod magic_link;
pub mod subscriber_confirm;
pub mod subscriber_incident;
pub mod subscriber_maintenance;
pub mod support_request;

/// Header safety for subjects; the same rule every other channel applies to a
/// one-line value.
pub(crate) use crate::text::single_line;

/// HTML-escape the five entities that matter in element text and double- or
/// single-quoted attribute values. Single owner for every transactional
/// template — the escape set is a security invariant and must not drift
/// between copies.
pub(crate) fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Attribute-context escaping. Same rule as [`html_escape`] today (the quote
/// entities cover `href="…"`); a distinct name keeps call sites self-documenting.
pub(crate) fn attr_escape(input: &str) -> String {
    html_escape(input)
}

/// Two-unit duration for mail prose. Mail owns its wording so a view change
/// cannot silently reword an email.
pub(crate) fn duration_words(secs: i64) -> String {
    let minutes = (secs / 60).max(0);
    match (minutes / 1440, (minutes % 1440) / 60, minutes % 60) {
        (0, 0, 0) => "under a minute".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Wall-clock stamp for mail. Always UTC and always says so — unlike the app,
/// an inbox carries no viewer timezone to render in.
pub(crate) fn utc_stamp(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%-d %b %Y %H:%M UTC").to_string()
}
