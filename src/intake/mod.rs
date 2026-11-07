//! Alerts other tools send in: the event each payload comes down to, and the
//! rules every source kind shares for its key, severity and text. A parser is a
//! pure function of the body, so its fixtures are the whole of its contract.

mod alertmanager;
mod datadog;
mod generic;
pub mod signature;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::{AlertGrouping, AlertSourceKind, IncidentSeverity};
use crate::security::sha256_hex;
use crate::text::{single_line, truncate_chars};

/// Longer keys are stored as their SHA-256 hex.
pub const MAX_KEY_LEN: usize = 255;
/// Keys the platform keeps for the incidents it opens about a source itself.
pub const RESERVED_KEY_PREFIX: &str = "uptimepage:";
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_DETAIL_CHARS: usize = 2_000;
pub const MAX_LABELS: usize = 50;
pub const MAX_LABEL_KEY_CHARS: usize = 100;
pub const MAX_LABEL_VALUE_CHARS: usize = 500;
pub const MAX_LINKS: usize = 5;
/// `incident_alert_state.firing_count` is a Postgres INTEGER.
pub const MAX_FIRING: u32 = i32::MAX as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAction {
    Fire,
    Resolve,
}

/// A link the sender attached. Only `http` and `https` survive parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AlertLink {
    pub href: String,
    pub text: String,
}

/// One thing a tool said about one alert key.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertEvent {
    pub key: String,
    pub action: EventAction,
    /// One line, capped, as stored on the incident.
    pub title: String,
    pub detail: Option<String>,
    pub severity: IncidentSeverity,
    /// Alerts firing under this key, cut ones included. Capped at what the
    /// database column holds.
    pub firing: u32,
    /// Alerts the sender cut from the payload. A resolve with any cut is not
    /// trusted to mean everything recovered.
    pub truncated: u64,
    /// The sender's own time: the latest start among firing alerts for a
    /// fire, the latest end for a resolve. `None` when it sent none.
    pub at: Option<DateTime<Utc>>,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub links: Vec<AlertLink>,
}

/// A body that does not read as the source's format. With a valid token it
/// still opens the source's `unparsed` incident, so a tool that tried to page
/// someone is never dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unparsed {
    pub reason: String,
}

impl Unparsed {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// A value the sender chose, quoted for a reason we store: one line, short, so
/// a hostile body cannot carry its size into the unparsed incident.
pub(crate) fn quoted(raw: &str) -> String {
    format!("{:?}", truncate_chars(&single_line(raw), 60))
}

/// Every event in `body` for a source of `kind`. Empty for a notification the
/// source sends that says nothing about an alert, such as a Datadog
/// transition we do not act on.
pub fn parse(
    kind: AlertSourceKind,
    grouping: AlertGrouping,
    body: &[u8],
) -> Result<Vec<AlertEvent>, Unparsed> {
    match kind {
        AlertSourceKind::Generic => generic::parse(body),
        AlertSourceKind::Alertmanager | AlertSourceKind::Grafana => {
            alertmanager::parse(grouping, body)
        }
        AlertSourceKind::Datadog => datadog::parse(body),
    }
}

/// The stored form of a sender's key: as sent when it fits and is not one the
/// platform keeps for itself, else its SHA-256 hex. `None` for a blank key.
pub fn stored_key(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.len() > MAX_KEY_LEN || raw.starts_with(RESERVED_KEY_PREFIX) {
        return Some(sha256_hex(raw));
    }
    Some(raw.to_string())
}

/// A title as stored: one line, capped, never empty.
pub(crate) fn clean_title(raw: &str, fallback: &str) -> String {
    let line = single_line(raw);
    let line = if line.is_empty() {
        single_line(fallback)
    } else {
        line
    };
    truncate_chars(&line, MAX_TITLE_CHARS)
}

/// Free text as stored: control characters other than line breaks and tabs
/// removed, capped. `None` when nothing is left.
pub(crate) fn clean_block(raw: &str, max_chars: usize) -> Option<String> {
    let kept: String = raw
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let kept = kept.trim();
    (!kept.is_empty()).then(|| truncate_chars(kept, max_chars))
}

/// At most [`MAX_LABELS`] pairs, each side one line and capped, empty keys
/// dropped. Keys keep their sorted order, so a cut is the same every time.
pub(crate) fn clean_labels<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in pairs {
        let k = truncate_chars(&single_line(k), MAX_LABEL_KEY_CHARS);
        if k.is_empty() {
            continue;
        }
        out.insert(k, truncate_chars(&single_line(v), MAX_LABEL_VALUE_CHARS));
        // Trimmed as it goes, so the map never grows past the ones kept.
        if out.len() > MAX_LABELS {
            out.pop_last();
        }
    }
    out
}

/// Annotations kept whole while their text fits [`MAX_DETAIL_CHARS`] in all;
/// the rest are dropped rather than cut mid-value.
pub(crate) fn clean_annotations<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut budget = MAX_DETAIL_CHARS;
    for (k, v) in pairs {
        let k = truncate_chars(&single_line(k), MAX_LABEL_KEY_CHARS);
        let Some(v) = clean_block(v, MAX_DETAIL_CHARS) else {
            continue;
        };
        let cost = k.chars().count() + v.chars().count();
        if k.is_empty() || cost > budget {
            continue;
        }
        budget -= cost;
        out.insert(k, v);
    }
    out
}

/// A link kept only when it is `http` or `https`; its text one line, capped.
pub(crate) fn clean_link(href: &str, text: &str) -> Option<AlertLink> {
    let url = url::Url::parse(href.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.as_str().chars().count() > 2_000 {
        return None;
    }
    let text = truncate_chars(&single_line(text), MAX_TITLE_CHARS);
    Some(AlertLink {
        href: url.as_str().to_string(),
        text: if text.is_empty() { "Link".into() } else { text },
    })
}

/// How bad an Alertmanager or Grafana `severity` label reads: unset is major,
/// a value we do not know is minor.
pub(crate) fn label_severity(label: Option<&str>) -> IncidentSeverity {
    match label.map(|s| s.trim().to_ascii_lowercase()) {
        None => IncidentSeverity::Major,
        Some(s) if s.is_empty() => IncidentSeverity::Major,
        Some(s) => match s.as_str() {
            "critical" | "page" | "p1" => IncidentSeverity::Critical,
            "error" | "high" | "major" | "p2" => IncidentSeverity::Major,
            _ => IncidentSeverity::Minor,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_kept_unless_too_long_or_reserved() {
        assert_eq!(
            stored_key(" checkout-5xx ").as_deref(),
            Some("checkout-5xx")
        );
        assert_eq!(stored_key("  "), None);
        let long = "k".repeat(MAX_KEY_LEN + 1);
        assert_eq!(stored_key(&long), Some(sha256_hex(&long)));
        assert_eq!(
            stored_key("uptimepage:overflow"),
            Some(sha256_hex("uptimepage:overflow")),
            "a sender cannot reach the platform's own keys"
        );
    }

    #[test]
    fn text_from_outside_is_flattened_and_capped() {
        assert_eq!(clean_title("a\nb\u{0}c", "x"), "a b c");
        assert_eq!(clean_title("   ", "fallback"), "fallback");
        assert_eq!(
            clean_title(&"t".repeat(500), "x").chars().count(),
            MAX_TITLE_CHARS
        );
        assert_eq!(
            clean_block("line one\nline two\u{7}", 100).as_deref(),
            Some("line one\nline two")
        );
        assert_eq!(clean_block(" \u{1b} ", 100), None);
    }

    #[test]
    fn only_web_links_are_kept() {
        assert!(clean_link("https://grafana.example.com/d/abc", "Dashboard").is_some());
        assert!(clean_link("javascript:alert(1)", "x").is_none());
        assert!(clean_link("file:///etc/passwd", "x").is_none());
        assert_eq!(clean_link("http://x.test/", " ").unwrap().text, "Link");
        assert_eq!(
            clean_link("https://x.test/a\tb", "x").unwrap().href,
            "https://x.test/ab",
            "stored as the parser normalised it"
        );
    }

    #[test]
    fn labels_are_capped_in_count_and_length() {
        let many: Vec<(String, String)> = (0..80)
            .map(|i| (format!("k{i:02}"), "v".repeat(900)))
            .collect();
        let labels = clean_labels(many.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        assert_eq!(labels.len(), MAX_LABELS);
        let reversed = clean_labels(many.iter().rev().map(|(k, v)| (k.as_str(), v.as_str())));
        assert_eq!(labels, reversed, "the same labels are kept in any order");
        assert_eq!(labels.keys().last().map(String::as_str), Some("k49"));
        assert!(
            labels
                .values()
                .all(|v| v.chars().count() == MAX_LABEL_VALUE_CHARS)
        );
    }

    #[test]
    fn severity_reads_the_common_label_values() {
        use IncidentSeverity::*;
        assert_eq!(label_severity(Some("Critical")), Critical);
        assert_eq!(label_severity(Some("p2")), Major);
        assert_eq!(label_severity(Some("warning")), Minor);
        assert_eq!(label_severity(None), Major);
        assert!(Critical > Major && Major > Minor, "worse is greater");
    }
}
