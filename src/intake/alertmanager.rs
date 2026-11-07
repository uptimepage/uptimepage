//! Alertmanager's webhook body, version 4, which Grafana Alerting also sends
//! with its own fields added. One group per request: in group mode it is one
//! event under the `groupKey`, in alert mode one per alert under its
//! `fingerprint`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::domain::{AlertGrouping, IncidentSeverity};

use super::{
    AlertEvent, AlertLink, EventAction, MAX_DETAIL_CHARS, MAX_FIRING, MAX_LINKS, Unparsed,
    clean_annotations, clean_block, clean_labels, clean_link, clean_title, label_severity, quoted,
    stored_key,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Body {
    group_key: String,
    status: String,
    #[serde(default)]
    truncated_alerts: u64,
    #[serde(default)]
    common_labels: BTreeMap<String, String>,
    #[serde(default)]
    common_annotations: BTreeMap<String, String>,
    #[serde(default)]
    external_url: Option<String>,
    /// Grafana only.
    #[serde(default)]
    message: Option<String>,
    alerts: Vec<Alert>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Alert {
    status: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(default)]
    starts_at: Option<DateTime<Utc>>,
    #[serde(default)]
    ends_at: Option<DateTime<Utc>>,
    #[serde(default, rename = "generatorURL")]
    generator_url: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
    /// Grafana only.
    #[serde(default, rename = "dashboardURL")]
    dashboard_url: Option<String>,
    #[serde(default, rename = "panelURL")]
    panel_url: Option<String>,
}

impl Alert {
    fn firing(&self) -> bool {
        self.status == "firing"
    }

    /// The zero time both senders write for "not yet" reads as none.
    fn started(&self) -> Option<DateTime<Utc>> {
        self.starts_at.filter(|t| t.timestamp() > 0)
    }

    fn ended(&self) -> Option<DateTime<Utc>> {
        self.ends_at.filter(|t| t.timestamp() > 0)
    }

    fn links(&self) -> impl Iterator<Item = AlertLink> + '_ {
        [
            (self.generator_url.as_deref(), "Source"),
            (self.dashboard_url.as_deref(), "Dashboard"),
            (self.panel_url.as_deref(), "Panel"),
        ]
        .into_iter()
        .filter_map(|(href, text)| href.and_then(|h| clean_link(h, text)))
    }
}

fn status(raw: &str) -> Result<EventAction, Unparsed> {
    match raw {
        "firing" => Ok(EventAction::Fire),
        "resolved" => Ok(EventAction::Resolve),
        other => Err(Unparsed::new(format!(
            "status must be firing or resolved, not {}",
            quoted(other)
        ))),
    }
}

/// Summary annotation, else the alert name, else a generic fallback.
fn title_from(labels: &BTreeMap<String, String>, annotations: &BTreeMap<String, String>) -> String {
    let named = annotations
        .get("summary")
        .or_else(|| labels.get("alertname"))
        .map(String::as_str)
        .unwrap_or_default();
    clean_title(named, "Alert")
}

fn pairs(map: &BTreeMap<String, String>) -> impl Iterator<Item = (&str, &str)> {
    map.iter().map(|(k, v)| (k.as_str(), v.as_str()))
}

pub(super) fn parse(grouping: AlertGrouping, body: &[u8]) -> Result<Vec<AlertEvent>, Unparsed> {
    let body: Body = serde_json::from_slice(body)
        .map_err(|e| Unparsed::new(format!("not an Alertmanager or Grafana alert: {e}")))?;
    let group_status = status(&body.status)?;
    if body.alerts.is_empty() {
        return Err(Unparsed::new("a notification with no alerts"));
    }
    for a in &body.alerts {
        status(&a.status)?;
    }
    match grouping {
        AlertGrouping::Group => group_event(body, group_status).map(|e| vec![e]),
        AlertGrouping::Alert => body
            .alerts
            .iter()
            .map(|a| alert_event(a, body.external_url.as_deref(), body.truncated_alerts))
            .collect(),
    }
}

/// The whole group as one event. A resolved group with alerts cut from the
/// payload is not trusted to mean they all recovered, so it reads as a repeat.
fn group_event(body: Body, group_status: EventAction) -> Result<AlertEvent, Unparsed> {
    let key = stored_key(&body.group_key).ok_or_else(|| Unparsed::new("groupKey is empty"))?;
    let firing: Vec<&Alert> = body.alerts.iter().filter(|a| a.firing()).collect();
    let distrusted = group_status == EventAction::Resolve && body.truncated_alerts > 0;
    let action = match distrusted {
        true => EventAction::Fire,
        false => group_status,
    };
    let at = match action {
        EventAction::Fire => firing.iter().filter_map(|a| a.started()).max(),
        EventAction::Resolve => body.alerts.iter().filter_map(|a| a.ended()).max(),
    };
    // The worst firing alert wins; a group with none firing is rated whole.
    let rated: Vec<&Alert> = if firing.is_empty() {
        body.alerts.iter().collect()
    } else {
        firing.clone()
    };
    let severity = rated
        .iter()
        .map(|a| label_severity(a.labels.get("severity").map(String::as_str)))
        .max()
        .unwrap_or(IncidentSeverity::Major);
    let first = &body.alerts[0];
    let labels = if body.common_labels.is_empty() {
        &first.labels
    } else {
        &body.common_labels
    };
    let annotations = if body.common_annotations.is_empty() {
        &first.annotations
    } else {
        &body.common_annotations
    };
    let detail = annotations
        .get("description")
        .or(body.message.as_ref())
        .and_then(|d| clean_block(d, MAX_DETAIL_CHARS));
    let mut links: Vec<AlertLink> = Vec::new();
    for link in body.alerts.iter().flat_map(Alert::links) {
        if links.len() == MAX_LINKS {
            break;
        }
        if !links.contains(&link) {
            links.push(link);
        }
    }
    // A cut alert may have resolved already; counting it as firing is the
    // safe reading, since nothing says otherwise.
    let firing_count = u64::try_from(firing.len())
        .unwrap_or(u64::MAX)
        .saturating_add(body.truncated_alerts)
        .min(u64::from(MAX_FIRING)) as u32;
    Ok(AlertEvent {
        title: title_from(labels, annotations),
        detail,
        severity,
        firing: firing_count,
        truncated: body.truncated_alerts,
        may_open: !distrusted,
        at,
        labels: clean_labels(pairs(labels)),
        annotations: clean_annotations(pairs(annotations)),
        links,
        key,
        action,
    })
}

/// One alert as its own event, under its fingerprint. Its own status is true;
/// a cut alert sends nothing at all, so each event carries the count cut from
/// its payload for the delivery log to say so.
fn alert_event(
    alert: &Alert,
    external_url: Option<&str>,
    truncated: u64,
) -> Result<AlertEvent, Unparsed> {
    let key = alert
        .fingerprint
        .as_deref()
        .and_then(stored_key)
        .ok_or_else(|| Unparsed::new("an alert has no fingerprint"))?;
    let action = status(&alert.status)?;
    let mut links: Vec<AlertLink> = alert.links().collect();
    if links.is_empty()
        && let Some(link) = external_url.and_then(|u| clean_link(u, "Alertmanager"))
    {
        links.push(link);
    }
    links.truncate(MAX_LINKS);
    Ok(AlertEvent {
        title: title_from(&alert.labels, &alert.annotations),
        detail: alert
            .annotations
            .get("description")
            .and_then(|d| clean_block(d, MAX_DETAIL_CHARS)),
        severity: label_severity(alert.labels.get("severity").map(String::as_str)),
        firing: u32::from(action == EventAction::Fire),
        truncated,
        may_open: true,
        at: match action {
            EventAction::Fire => alert.started(),
            EventAction::Resolve => alert.ended(),
        },
        labels: clean_labels(pairs(&alert.labels)),
        annotations: clean_annotations(pairs(&alert.annotations)),
        links,
        key,
        action,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! fixture {
        ($name:literal) => {
            include_bytes!(concat!("../../tests/fixtures/alerts/", $name))
        };
    }

    fn group(body: &[u8]) -> AlertEvent {
        let mut events = parse(AlertGrouping::Group, body).expect("parses");
        assert_eq!(events.len(), 1, "one event per group");
        events.remove(0)
    }

    #[test]
    fn a_firing_group_is_one_event_under_its_group_key() {
        let e = group(fixture!("alertmanager_firing.json"));
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(e.key, "{}/{}:{alertname=\"KubePodCrashLooping\"}");
        assert_eq!(e.title, "Pod is crash looping");
        assert_eq!(e.firing, 3);
        assert_eq!(
            e.severity,
            IncidentSeverity::Critical,
            "the worst firing alert wins"
        );
        assert!(e.at.is_some());
        assert_eq!(
            e.labels.get("alertname").map(String::as_str),
            Some("KubePodCrashLooping")
        );
        assert_eq!(e.links.len(), 1, "one generator link, deduplicated");
    }

    #[test]
    fn a_partial_recovery_still_fires_with_the_lower_count() {
        let e = group(fixture!("alertmanager_partial.json"));
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(e.firing, 2);
    }

    #[test]
    fn a_resolved_group_resolves() {
        let e = group(fixture!("alertmanager_resolved.json"));
        assert_eq!(e.action, EventAction::Resolve);
        assert_eq!(e.firing, 0);
        assert!(e.at.is_some(), "the latest end");
    }

    /// Alertmanager computes the status after cutting the list, so this
    /// payload reads resolved while the cut alert still fires.
    #[test]
    fn a_truncated_resolved_group_is_a_repeat_not_a_close() {
        let e = group(fixture!("alertmanager_truncated_resolved.json"));
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(e.truncated, 1);
        assert_eq!(e.firing, 1, "the cut alert counts as firing");
        assert!(
            !e.may_open,
            "it never opens an incident for a recovered group"
        );
    }

    #[test]
    fn a_truncated_firing_group_counts_what_was_cut() {
        let e = group(fixture!("alertmanager_truncated_firing.json"));
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(e.firing, 3);
    }

    #[test]
    fn alert_mode_is_one_event_per_fingerprint() {
        let events = parse(AlertGrouping::Alert, fixture!("alertmanager_partial.json")).unwrap();
        assert_eq!(events.len(), 3);
        let resolved = events
            .iter()
            .filter(|e| e.action == EventAction::Resolve)
            .count();
        assert_eq!(resolved, 1);
        let keys: std::collections::BTreeSet<&str> =
            events.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys.len(), 3, "each alert has its own key");
        let cut = parse(
            AlertGrouping::Alert,
            fixture!("alertmanager_truncated_firing.json"),
        )
        .unwrap();
        assert!(
            cut.iter().all(|e| e.truncated == 1),
            "a cut is visible on what was kept"
        );
        let warning = events
            .iter()
            .find(|e| e.labels.get("pod").map(String::as_str) == Some("api-2"))
            .unwrap();
        assert_eq!(warning.severity, IncidentSeverity::Minor);
    }

    #[test]
    fn grafana_sends_the_same_shape() {
        let e = group(fixture!("grafana_firing.json"));
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(e.title, "Checkout errors above 5%");
        assert_eq!(e.firing, 2);
        assert_eq!(e.severity, IncidentSeverity::Critical);
        assert_eq!(
            e.detail.as_deref(),
            Some("Error rate has been above 5% for 1 minute")
        );
        let e = group(fixture!("grafana_resolved.json"));
        assert_eq!(e.action, EventAction::Resolve);
    }

    /// A rule whose query fails still reaches the on-call, under its own name.
    #[test]
    fn a_grafana_datasource_error_fires_as_its_own_alert() {
        let e = group(fixture!("grafana_datasource_error.json"));
        assert_eq!(e.action, EventAction::Fire);
        assert_eq!(
            e.labels.get("alertname").map(String::as_str),
            Some("DatasourceError")
        );
        assert!(
            e.links.iter().all(|l| l.href.starts_with("http")),
            "empty URLs are dropped"
        );
    }

    #[test]
    fn what_is_not_a_notification_is_unparsed() {
        for body in [
            &b"{}"[..],
            br#"{"groupKey":"g","status":"firing","alerts":[]}"#,
            br#"{"groupKey":"g","status":"pending","alerts":[{"status":"firing"}]}"#,
            br#"{"groupKey":" ","status":"firing","alerts":[{"status":"firing"}]}"#,
        ] {
            assert!(parse(AlertGrouping::Group, body).is_err());
        }
        assert!(
            parse(
                AlertGrouping::Alert,
                br#"{"groupKey":"g","status":"firing","alerts":[{"status":"firing"}]}"#
            )
            .is_err(),
            "alert mode needs a fingerprint"
        );
    }
}
