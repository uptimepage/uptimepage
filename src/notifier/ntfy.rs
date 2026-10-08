use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::domain::{IncidentUrgency, NotificationReason};
use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json_with_headers};
use crate::notifier::event::IncidentNotice;
use crate::notifier::transport::Notifier;
use crate::text::truncate_bytes;

// ntfy's default server-side cap is 4096 BYTES; an over-limit publish is
// silently converted to a .txt attachment (or rejected when attachments
// are off), so the cap must count bytes, not chars.
const MAX_MESSAGE_BYTES: usize = 4096;

/// Acknowledge action for a push notification. Its own type so the factory
/// call site reads, the way [`crate::notifier::email::EmailAlert`] does for mail.
#[derive(Clone)]
pub struct PushAck {
    pub url: String,
}

pub struct NtfyNotifier {
    client: OutboundHttpClient,
    /// Server root; JSON publishes POST here with the topic in the body.
    server_url: Url,
    topic: String,
    access_token: Option<String>,
    ack: Option<PushAck>,
}

#[derive(Serialize)]
struct Publish<'a> {
    topic: &'a str,
    title: &'a str,
    message: String,
    priority: u8,
    tags: [&'static str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    click: Option<&'a str>,
    /// ntfy allows at most three; we spend one. Tapping the notification body
    /// already opens the incident, so a second button would only repeat it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<Action<'a>>,
}

/// One tap from the notification, no browser. `clear` is safe here because on
/// an `http` action ntfy only clears once the request succeeds — a refused
/// acknowledgement leaves the page on screen rather than telling the responder
/// they took an outage they did not.
#[derive(Serialize)]
struct Action<'a> {
    action: &'static str,
    label: &'static str,
    url: &'a str,
    method: &'static str,
    clear: bool,
}

impl NtfyNotifier {
    pub fn new(
        client: OutboundHttpClient,
        server_url: Url,
        topic: String,
        access_token: Option<String>,
        ack: Option<PushAck>,
    ) -> Self {
        Self {
            client,
            server_url,
            topic,
            access_token,
            ack,
        }
    }

    fn publish<'a>(
        topic: &'a str,
        notice: &'a IncidentNotice,
        ack: Option<&'a PushAck>,
    ) -> Publish<'a> {
        let resolved = notice.reason.closes_incident();
        Publish {
            topic,
            title: notice.label(),
            message: truncate_bytes(&notice.plain_text(), MAX_MESSAGE_BYTES),
            priority: match (resolved, notice.urgency) {
                (true, _) => 3,
                (false, IncidentUrgency::High) => 4,
                (false, IncidentUrgency::Low) => 3,
            },
            tags: match notice.reason {
                NotificationReason::Resolved => ["white_check_mark"],
                NotificationReason::MonitorDeleted => ["white_circle"],
                _ => ["rotating_light"],
            },
            click: notice.url.as_deref(),
            actions: match ack.filter(|_| !resolved) {
                Some(a) => vec![Action {
                    action: "http",
                    label: "Acknowledge",
                    url: &a.url,
                    method: "POST",
                    clear: true,
                }],
                None => Vec::new(),
            },
        }
    }
}

#[async_trait]
impl Notifier for NtfyNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let mut headers = BTreeMap::new();
        if let Some(token) = &self.access_token {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        }
        post_json_with_headers(
            &self.client,
            &self.server_url,
            &Self::publish(&self.topic, notice, self.ack.as_ref()),
            &headers,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::domain::{IncidentOrigin, IncidentSeverity};

    fn notice(reason: NotificationReason, urgency: IncidentUrgency) -> IncidentNotice {
        IncidentNotice {
            incident_id: Uuid::from_u128(7),
            reason,
            monitor_name: Some("api-prod".into()),
            title: None,
            severity: IncidentSeverity::Major,
            urgency,
            origin: IncidentOrigin::Monitor,
            started_at: Utc.with_ymd_and_hms(2026, 6, 12, 8, 0, 0).unwrap(),
            ended_at: None,
            error_sample: None,
            regions_down: Vec::new(),
            regions_up: Vec::new(),
            url: Some("https://app.uptimepage.dev/i/7".into()),
            note: None,
        }
    }

    #[test]
    fn open_publish_matches_wire_shape() {
        let n = notice(NotificationReason::Opened, IncidentUrgency::High);
        let v = serde_json::to_value(NtfyNotifier::publish("ops", &n, None)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "topic": "ops",
                "title": "api-prod",
                "message": "api-prod — major incident OPEN\nhttps://app.uptimepage.dev/i/7",
                "priority": 4,
                "tags": ["rotating_light"],
                "click": "https://app.uptimepage.dev/i/7"
            })
        );
    }

    #[test]
    fn ack_action_rides_an_open_page_and_not_a_resolve() {
        let ack = PushAck {
            url: "https://app.uptimepage.dev/incident/ack?o=1&i=7&c=2&e=99&t=ab".into(),
        };

        let open = notice(NotificationReason::Opened, IncidentUrgency::High);
        let v = serde_json::to_value(NtfyNotifier::publish("ops", &open, Some(&ack))).unwrap();
        assert_eq!(
            v["actions"],
            serde_json::json!([{
                "action": "http",
                "label": "Acknowledge",
                "url": "https://app.uptimepage.dev/incident/ack?o=1&i=7&c=2&e=99&t=ab",
                "method": "POST",
                "clear": true
            }])
        );

        // The key is absent rather than an empty array, so the body of a page
        // without one is unchanged.
        let done = notice(NotificationReason::Resolved, IncidentUrgency::High);
        let v = serde_json::to_value(NtfyNotifier::publish("ops", &done, Some(&ack))).unwrap();
        assert!(v.get("actions").is_none());

        let v = serde_json::to_value(NtfyNotifier::publish("ops", &open, None)).unwrap();
        assert!(v.get("actions").is_none());
    }

    #[test]
    fn priorities_and_tags_follow_lifecycle() {
        let low = notice(NotificationReason::Opened, IncidentUrgency::Low);
        assert_eq!(NtfyNotifier::publish("t", &low, None).priority, 3);

        let resolved = notice(NotificationReason::Resolved, IncidentUrgency::High);
        let p = NtfyNotifier::publish("t", &resolved, None);
        assert_eq!(p.priority, 3);
        assert_eq!(p.tags, ["white_check_mark"]);

        let mut no_url = notice(NotificationReason::Opened, IncidentUrgency::High);
        no_url.url = None;
        let v = serde_json::to_value(NtfyNotifier::publish("t", &no_url, None)).unwrap();
        assert!(v.get("click").is_none());
    }
}
