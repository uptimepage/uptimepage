use async_trait::async_trait;
use serde::Serialize;
use url::Url;
use uuid::Uuid;

use crate::domain::{IncidentSeverity, NotificationReason};
use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::event::IncidentNotice;
use crate::notifier::transport::Notifier;
use crate::text::truncate_chars;

const ENQUEUE_URL: &str = "https://events.pagerduty.com/v2/enqueue";
// Events API v2 caps payload.summary at 1024 characters.
const MAX_SUMMARY_CHARS: usize = 1024;

pub struct PagerDutyNotifier {
    client: OutboundHttpClient,
    routing_key: String,
}

#[derive(Serialize)]
struct Event<'a> {
    routing_key: &'a str,
    event_action: &'static str,
    dedup_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<Payload<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    links: Vec<Link<'a>>,
}

#[derive(Serialize)]
struct Payload<'a> {
    summary: String,
    source: &'a str,
    severity: &'static str,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_details: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct Link<'a> {
    href: &'a str,
    text: &'static str,
}

impl PagerDutyNotifier {
    pub fn new(client: OutboundHttpClient, routing_key: String) -> Self {
        Self {
            client,
            routing_key,
        }
    }

    fn severity(notice: &IncidentNotice) -> &'static str {
        match notice.severity {
            IncidentSeverity::Critical => "critical",
            IncidentSeverity::Major => "error",
            IncidentSeverity::Minor => "warning",
        }
    }

    fn trigger<'a>(
        routing_key: &'a str,
        notice: &'a IncidentNotice,
        dedup_key: String,
        severity: &'static str,
    ) -> Event<'a> {
        let mut details = serde_json::Map::new();
        if !notice.regions_down.is_empty() {
            details.insert("regions_down".into(), notice.regions_down.clone().into());
        }
        if !notice.regions_up.is_empty() {
            details.insert("regions_up".into(), notice.regions_up.clone().into());
        }
        if let Some(err) = &notice.error_sample {
            details.insert("error".into(), err.clone().into());
        }
        // The summary is the first line only, so the note would be cut there.
        if let Some(note) = &notice.note {
            details.insert("note".into(), note.clone().into());
        }
        Event {
            routing_key,
            event_action: "trigger",
            dedup_key,
            payload: Some(Payload {
                summary: truncate_chars(&notice.summary(), MAX_SUMMARY_CHARS),
                source: notice.label(),
                severity,
                timestamp: notice.started_at.to_rfc3339(),
                custom_details: (!details.is_empty()).then(|| details.into()),
            }),
            links: notice
                .url
                .as_deref()
                .map(|href| {
                    vec![Link {
                        href,
                        text: "Open incident",
                    }]
                })
                .unwrap_or_default(),
        }
    }

    fn resolve(routing_key: &str, dedup_key: String) -> Event<'_> {
        Event {
            routing_key,
            event_action: "resolve",
            dedup_key,
            payload: None,
            links: Vec::new(),
        }
    }

    /// Every open-side reason shares one `dedup_key`, so PagerDuty folds a
    /// reminder into the incident it already has instead of paging again.
    fn closes_the_incident(reason: NotificationReason) -> bool {
        match reason {
            NotificationReason::Resolved | NotificationReason::DataResumed => true,
            NotificationReason::Opened
            | NotificationReason::Reopened
            | NotificationReason::Escalated
            | NotificationReason::Reminder
            | NotificationReason::NoData => false,
        }
    }

    async fn send(&self, event: &Event<'_>) -> Result<()> {
        let url: Url = ENQUEUE_URL.parse().expect("static enqueue URL parses");
        post_json(&self.client, &url, event).await
    }
}

#[async_trait]
impl Notifier for PagerDutyNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        // The test notice (nil incident id) must not leave an open PD
        // incident: trigger at `info` on a throwaway key, then resolve it.
        // Enqueue is async on PD's side and a resolve that beats its trigger
        // is dropped, leaving the test incident open — give the trigger a
        // head start.
        if notice.incident_id.is_nil() {
            let dedup = format!("uptimepage-test-{}", Uuid::now_v7());
            self.send(&Self::trigger(
                &self.routing_key,
                notice,
                dedup.clone(),
                "info",
            ))
            .await?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            return self.send(&Self::resolve(&self.routing_key, dedup)).await;
        }
        let dedup = notice.incident_id.to_string();
        if Self::closes_the_incident(notice.reason) {
            return self.send(&Self::resolve(&self.routing_key, dedup)).await;
        }
        self.send(&Self::trigger(
            &self.routing_key,
            notice,
            dedup,
            Self::severity(notice),
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::domain::{IncidentOrigin, IncidentUrgency};

    fn notice(reason: NotificationReason) -> IncidentNotice {
        IncidentNotice {
            incident_id: Uuid::from_u128(7),
            reason,
            monitor_name: Some("api-prod".into()),
            title: None,
            severity: IncidentSeverity::Major,
            urgency: IncidentUrgency::High,
            origin: IncidentOrigin::Monitor,
            started_at: Utc.with_ymd_and_hms(2026, 6, 12, 8, 0, 0).unwrap(),
            ended_at: None,
            error_sample: Some("timeout after 10s".into()),
            regions_down: vec!["eu-west".into()],
            regions_up: vec!["us-east".into()],
            url: Some("https://app.uptimepage.dev/i/7".into()),
            note: None,
        }
    }

    #[test]
    fn trigger_matches_events_v2_wire_shape() {
        let n = notice(NotificationReason::Opened);
        let v = serde_json::to_value(PagerDutyNotifier::trigger(
            "rk",
            &n,
            n.incident_id.to_string(),
            "error",
        ))
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "routing_key": "rk",
                "event_action": "trigger",
                "dedup_key": "00000000-0000-0000-0000-000000000007",
                "payload": {
                    "summary": "api-prod — major incident OPEN: timeout after 10s",
                    "source": "api-prod",
                    "severity": "error",
                    "timestamp": "2026-06-12T08:00:00+00:00",
                    "custom_details": {
                        "regions_down": ["eu-west"],
                        "regions_up": ["us-east"],
                        "error": "timeout after 10s"
                    }
                },
                "links": [{ "href": "https://app.uptimepage.dev/i/7", "text": "Open incident" }]
            })
        );
    }

    #[test]
    fn resolve_omits_payload_and_links() {
        let v = serde_json::to_value(PagerDutyNotifier::resolve("rk", "d".into())).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "routing_key": "rk",
                "event_action": "resolve",
                "dedup_key": "d"
            })
        );
    }

    #[test]
    fn only_a_recovery_closes_the_pagerduty_incident() {
        for reason in [
            NotificationReason::Resolved,
            NotificationReason::DataResumed,
        ] {
            assert!(PagerDutyNotifier::closes_the_incident(reason), "{reason:?}");
        }
        for reason in [
            NotificationReason::Opened,
            NotificationReason::Reopened,
            NotificationReason::Escalated,
            NotificationReason::Reminder,
            NotificationReason::NoData,
        ] {
            assert!(
                !PagerDutyNotifier::closes_the_incident(reason),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn severity_maps_to_pagerduty_levels() {
        let mut n = notice(NotificationReason::Opened);
        n.severity = IncidentSeverity::Critical;
        assert_eq!(PagerDutyNotifier::severity(&n), "critical");
        n.severity = IncidentSeverity::Major;
        assert_eq!(PagerDutyNotifier::severity(&n), "error");
        n.severity = IncidentSeverity::Minor;
        assert_eq!(PagerDutyNotifier::severity(&n), "warning");
    }
}
