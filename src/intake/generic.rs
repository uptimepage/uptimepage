//! Our own shape, a subset of PagerDuty's Events API v2, for scripts and any
//! tool that can template its webhook body. Unknown keys are ignored.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::domain::IncidentSeverity;

use super::{
    AlertEvent, EventAction, MAX_LINKS, Unparsed, clean_labels, clean_link, clean_title, quoted,
    stored_key,
};

/// RFC 3339, or the colon-less offset PagerDuty's examples use.
fn sender_time(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    DateTime::parse_from_rfc3339(raw)
        .or_else(|_| DateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f%z"))
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[derive(Deserialize)]
struct Body {
    event_action: String,
    dedup_key: Option<String>,
    #[serde(default)]
    payload: Option<Payload>,
    #[serde(default)]
    links: Vec<Link>,
}

#[derive(Deserialize)]
struct Payload {
    summary: Option<String>,
    source: Option<String>,
    severity: Option<String>,
    timestamp: Option<String>,
    #[serde(default)]
    custom_details: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Link {
    href: String,
    #[serde(default)]
    text: String,
}

pub(super) fn parse(body: &[u8]) -> Result<Vec<AlertEvent>, Unparsed> {
    let body: Body = serde_json::from_slice(body)
        .map_err(|e| Unparsed::new(format!("not a generic alert: {e}")))?;
    let action = match body.event_action.as_str() {
        "trigger" => EventAction::Fire,
        "resolve" => EventAction::Resolve,
        // A sender written for PagerDuty also acknowledges. Nothing here acts
        // on it, and reading it as unreadable would page.
        "acknowledge" => return Ok(Vec::new()),
        other => {
            return Err(Unparsed::new(format!(
                "event_action must be trigger, resolve or acknowledge, not {}",
                quoted(other)
            )));
        }
    };
    let key = body
        .dedup_key
        .as_deref()
        .and_then(stored_key)
        .ok_or_else(|| Unparsed::new("dedup_key is required"))?;
    let payload = body.payload;
    let summary = payload.as_ref().and_then(|p| p.summary.as_deref());
    if action == EventAction::Fire && summary.is_none_or(|s| s.trim().is_empty()) {
        return Err(Unparsed::new("a trigger needs payload.summary"));
    }
    let severity = match payload
        .as_ref()
        .and_then(|p| p.severity.as_deref())
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("error") => IncidentSeverity::Major,
        Some("critical") => IncidentSeverity::Critical,
        Some("warning" | "info") => IncidentSeverity::Minor,
        Some(other) => {
            return Err(Unparsed::new(format!(
                "severity must be critical, error, warning or info, not {}",
                quoted(other)
            )));
        }
    };
    // A time we cannot read only costs the stale check, so it is dropped
    // rather than refusing the event. PagerDuty's own examples write the
    // offset without a colon.
    let at = payload
        .as_ref()
        .and_then(|p| p.timestamp.as_deref())
        .and_then(sender_time);
    let details: Vec<(String, String)> = payload
        .as_ref()
        .and_then(|p| p.custom_details.as_ref())
        .and_then(|d| d.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| match v {
                    serde_json::Value::String(s) => Some((k.clone(), s.clone())),
                    serde_json::Value::Number(n) => Some((k.clone(), n.to_string())),
                    serde_json::Value::Bool(b) => Some((k.clone(), b.to_string())),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let mut details = details;
    if let Some(source) = payload.as_ref().and_then(|p| p.source.as_deref())
        && !details.iter().any(|(k, _)| k == "source")
    {
        details.push(("source".into(), source.into()));
    }
    let labels = clean_labels(details.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    Ok(vec![AlertEvent {
        title: clean_title(summary.unwrap_or_default(), &key),
        detail: None,
        severity,
        firing: u32::from(action == EventAction::Fire),
        truncated: 0,
        at,
        labels,
        annotations: Default::default(),
        links: body
            .links
            .iter()
            .filter_map(|l| clean_link(&l.href, &l.text))
            .take(MAX_LINKS)
            .collect(),
        key,
        action,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(body: &str) -> AlertEvent {
        let mut events = parse(body.as_bytes()).expect("parses");
        assert_eq!(events.len(), 1);
        events.remove(0)
    }

    #[test]
    fn the_documented_example_reads_as_one_firing_event() {
        let e = one(include_str!(
            "../../tests/fixtures/alerts/generic_trigger.json"
        ));
        assert_eq!(e.key, "checkout-5xx");
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(e.title, "Checkout error rate above 5%");
        assert_eq!(e.severity, IncidentSeverity::Critical);
        assert_eq!(
            e.labels.get("region").map(String::as_str),
            Some("eu-west-1")
        );
        assert_eq!(
            e.labels.get("source").map(String::as_str),
            Some("billing-worker")
        );
        assert_eq!(e.links.len(), 1);
        assert!(e.at.is_some());
    }

    #[test]
    fn a_resolve_needs_only_its_key() {
        let e = one(r#"{"event_action":"resolve","dedup_key":"checkout-5xx"}"#);
        assert_eq!(e.action, EventAction::Resolve);
        assert_eq!(e.firing, 0);
    }

    #[test]
    fn an_acknowledge_is_heard_and_does_nothing() {
        let events = parse(br#"{"event_action":"acknowledge","dedup_key":"k"}"#).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn what_cannot_page_is_unparsed() {
        for body in [
            "not json",
            r#"{"event_action":"snooze","dedup_key":"k"}"#,
            r#"{"event_action":"trigger","dedup_key":"k"}"#,
            r#"{"event_action":"trigger","payload":{"summary":"s"}}"#,
            r#"{"event_action":"trigger","dedup_key":"k","payload":{"summary":"s","severity":"fatal"}}"#,
        ] {
            assert!(parse(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn a_time_is_read_leniently_and_dropped_when_unreadable() {
        let at = |ts: &str| {
            one(&format!(
                r#"{{"event_action":"trigger","dedup_key":"k","payload":{{"summary":"s","timestamp":"{ts}"}}}}"#
            ))
            .at
        };
        assert!(
            at("2015-07-17T08:42:58.315+0000").is_some(),
            "PagerDuty's own example"
        );
        assert!(at("2026-11-06T09:14:00Z").is_some());
        assert_eq!(at("yesterday"), None);
        let e = one(
            r#"{"event_action":"trigger","dedup_key":"k","payload":{"summary":"s","severity":" Critical "}}"#,
        );
        assert_eq!(e.severity, IncidentSeverity::Critical);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let e = one(
            r#"{"event_action":"trigger","dedup_key":"k","routing_key":"x","payload":{"summary":"s","class":"c"}}"#,
        );
        assert_eq!(e.severity, IncidentSeverity::Major, "error is the default");
    }
}
