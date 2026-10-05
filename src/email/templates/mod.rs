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

use crate::i18n::Tr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedEmail {
    pub subject: String,
    pub text_body: String,
    pub html_body: String,
}

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

/// Placeholder for markup inside a catalogue message, see [`fill_slot`].
pub(crate) const MARKUP_SLOT: &str = "\u{E000}";

/// Escapes a resolved message and puts `markup` where its [`MARKUP_SLOT`] was.
pub(crate) fn fill_slot(text: &str, markup: &str) -> String {
    html_escape(text).replacen(MARKUP_SLOT, markup, 1)
}

pub(crate) fn subscriber_footnote(tr: Tr, page_name: &str, unsubscribe_url: &str) -> String {
    layout::fine_print(&fill_slot(
        &tr.t_args(
            "email-footnote",
            [
                ("page", page_name.replace(MARKUP_SLOT, "").into()),
                ("unsubscribe", MARKUP_SLOT.into()),
            ],
        ),
        &layout::quiet_link(unsubscribe_url, &tr.t("email-unsubscribe")),
    ))
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

/// Wall-clock stamp for English mail; [`Tr::utc_stamp`] owns the format.
pub(crate) fn utc_stamp(ts: chrono::DateTime<chrono::Utc>) -> String {
    Tr::default().utc_stamp(ts)
}
