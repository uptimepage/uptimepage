//! The payload template we publish for Datadog's webhook integration. Every
//! value arrives as a string Datadog filled in, so a variable it could not
//! fill reads as missing.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::domain::IncidentSeverity;

use super::{
    AlertEvent, EventAction, MAX_DETAIL_CHARS, Unparsed, clean_block, clean_labels, clean_link,
    clean_title, stored_key,
};

#[derive(Deserialize)]
struct Body {
    transition: Option<String>,
    cycle_key: Option<String>,
    monitor_id: Option<String>,
    scope: Option<String>,
    title: Option<String>,
    message: Option<String>,
    priority: Option<String>,
    link: Option<String>,
    tags: Option<String>,
    date: Option<String>,
}

/// A value Datadog filled in: blank, or a variable it left as `$NAME`, reads
/// as missing.
fn filled(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| {
        let placeholder = s.strip_prefix('$').is_some_and(|name| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        });
        !s.is_empty() && !placeholder
    })
}

/// `$EVENT_TITLE` leads with the transition and sometimes the priority,
/// `[P1] [Triggered on {host:a}] CPU high`; the incident keeps what follows.
fn without_transition(title: &str) -> &str {
    const TRANSITIONS: [&str; 8] = [
        "Triggered",
        "Re-Triggered",
        "Recovered",
        "Warn",
        "Re-Warn",
        "No Data",
        "Re-No Data",
        "Renotify",
    ];
    const PRIORITIES: [&str; 5] = ["P1", "P2", "P3", "P4", "P5"];
    let mut t = title.trim_start();
    while let Some(rest) = t.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        let word = &rest[..end];
        let ours = PRIORITIES.contains(&word)
            || TRANSITIONS
                .iter()
                .any(|w| word == *w || word.starts_with(&format!("{w} ")));
        if !ours {
            break;
        }
        t = rest[end + 1..].trim_start();
    }
    t
}

pub(super) fn parse(body: &[u8]) -> Result<Vec<AlertEvent>, Unparsed> {
    let body: Body = serde_json::from_slice(body)
        .map_err(|e| Unparsed::new(format!("not the Datadog template: {e}")))?;
    let transition =
        filled(&body.transition).ok_or_else(|| Unparsed::new("transition is missing"))?;
    let (action, warning) = match transition {
        "Triggered" | "Re-Triggered" | "Renotify" => (EventAction::Fire, false),
        "Warn" | "Re-Warn" | "No Data" | "Re-No Data" => (EventAction::Fire, true),
        "Recovered" => (EventAction::Resolve, false),
        _ => return Ok(Vec::new()),
    };
    let key = match (filled(&body.cycle_key), filled(&body.monitor_id)) {
        (Some(cycle), _) => stored_key(cycle),
        (None, Some(monitor)) => {
            stored_key(&format!("{monitor}:{}", filled(&body.scope).unwrap_or("*")))
        }
        (None, None) => None,
    }
    .ok_or_else(|| Unparsed::new("neither cycle_key nor monitor_id is set"))?;
    // No priority set reads as major, as an unset severity does elsewhere.
    let severity = match (warning, filled(&body.priority)) {
        (true, _) => IncidentSeverity::Minor,
        (false, None) => IncidentSeverity::Major,
        (false, Some(p)) if p.eq_ignore_ascii_case("P1") => IncidentSeverity::Critical,
        (false, Some(p)) if p.eq_ignore_ascii_case("P2") => IncidentSeverity::Major,
        (false, Some(_)) => IncidentSeverity::Minor,
    };
    let at = filled(&body.date)
        .and_then(|d| d.parse::<i64>().ok())
        .and_then(DateTime::<Utc>::from_timestamp_millis);
    let tags: Vec<(String, String)> = filled(&body.tags)
        .into_iter()
        .chain(filled(&body.scope))
        .flat_map(|list| list.split(','))
        .filter_map(|tag| {
            let tag = tag.trim();
            match tag.split_once(':') {
                Some((k, v)) => Some((k.to_string(), v.to_string())),
                None if !tag.is_empty() => Some((tag.to_string(), String::new())),
                None => None,
            }
        })
        .collect();
    let title = filled(&body.title)
        .map(without_transition)
        .unwrap_or_default();
    Ok(vec![AlertEvent {
        title: clean_title(title, "Datadog monitor"),
        detail: filled(&body.message).and_then(|m| clean_block(m, MAX_DETAIL_CHARS)),
        severity,
        firing: u32::from(action == EventAction::Fire),
        truncated: 0,
        may_open: true,
        at,
        labels: clean_labels(tags.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
        annotations: Default::default(),
        links: filled(&body.link)
            .and_then(|l| clean_link(l, "Datadog"))
            .into_iter()
            .collect(),
        key,
        action,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(transition: &str) -> String {
        serde_json::json!({
            "transition": transition,
            "cycle_key": "cycle-7",
            "monitor_id": "1234",
            "scope": "host:web-1",
            "title": "[Triggered on {host:web-1}] CPU above 90%",
            "message": "CPU has been above 90% for 5 minutes",
            "priority": "P1",
            "link": "https://app.datadoghq.com/event/jump_to?event_id=1",
            "tags": "env:prod, team:core",
            "date": "1406662672000"
        })
        .to_string()
    }

    #[test]
    fn a_triggered_monitor_fires_under_its_cycle_key() {
        let e = &parse(body("Triggered").as_bytes()).unwrap()[0];
        assert_eq!(e.key, "cycle-7");
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(e.title, "CPU above 90%");
        assert_eq!(e.severity, IncidentSeverity::Critical);
        assert_eq!(e.labels.get("env").map(String::as_str), Some("prod"));
        assert_eq!(
            e.at,
            DateTime::<Utc>::from_timestamp_millis(1_406_662_672_000)
        );
        assert_eq!(e.links.len(), 1);
    }

    #[test]
    fn transitions_map_to_fire_resolve_or_nothing() {
        let action = |t: &str| parse(body(t).as_bytes()).unwrap().first().map(|e| e.action);
        assert_eq!(action("Recovered"), Some(EventAction::Resolve));
        assert_eq!(action("Renotify"), Some(EventAction::Fire));
        assert_eq!(action("Muted"), None, "logged and ignored");
        let warn = &parse(body("Warn").as_bytes()).unwrap()[0];
        assert_eq!(warn.severity, IncidentSeverity::Minor);
    }

    #[test]
    fn an_unfilled_cycle_key_falls_back_to_monitor_and_scope() {
        let mut v: serde_json::Value = serde_json::from_str(&body("Triggered")).unwrap();
        v["cycle_key"] = "$ALERT_CYCLE_KEY".into();
        let e = &parse(v.to_string().as_bytes()).unwrap()[0];
        assert_eq!(e.key, "1234:host:web-1");
    }

    #[test]
    fn the_transition_and_priority_prefix_come_off_the_title_and_nothing_else() {
        assert_eq!(
            without_transition("[P1] [Triggered on {host:web-1}] CPU above 90%"),
            "CPU above 90%"
        );
        assert_eq!(without_transition("[P10-team] Disk"), "[P10-team] Disk");
        assert_eq!(without_transition("[Recovered] Disk"), "Disk");
    }

    #[test]
    fn no_priority_reads_as_major_and_a_dollar_title_survives() {
        let mut v: serde_json::Value = serde_json::from_str(&body("Triggered")).unwrap();
        v["priority"] = "$ALERT_PRIORITY".into();
        v["title"] = "$5 off checkout is failing".into();
        let e = &parse(v.to_string().as_bytes()).unwrap()[0];
        assert_eq!(e.severity, IncidentSeverity::Major);
        assert_eq!(e.title, "$5 off checkout is failing");
    }

    #[test]
    fn a_body_broken_by_an_unescaped_quote_is_unparsed() {
        let broken = r#"{"transition":"Triggered","message":"said "hi"","cycle_key":"c"}"#;
        assert!(parse(broken.as_bytes()).is_err());
    }
}
