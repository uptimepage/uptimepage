use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::domain::IncidentUrgency;
use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json_with_headers};
use crate::notifier::event::IncidentNotice;
use crate::notifier::transport::Notifier;
use crate::text::truncate_chars;

// Gotify stores the message and imposes no documented cap; this one keeps a
// long error sample from filling the notification drawer.
const MAX_MESSAGE_CHARS: usize = 4096;

pub struct GotifyNotifier {
    client: OutboundHttpClient,
    /// `{server}/message`; the token picks the application it posts as.
    publish_url: Url,
    token: String,
}

#[derive(Serialize)]
struct Publish<'a> {
    title: &'a str,
    message: String,
    /// Gotify's 0-10 scale, as the Android client reads it: 8-10 sounds and
    /// vibrates, 4-7 sounds, 1-3 is the notification bar only.
    priority: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    extras: Option<Extras<'a>>,
}

#[derive(Serialize)]
struct Extras<'a> {
    #[serde(rename = "client::notification")]
    notification: Notification<'a>,
}

#[derive(Serialize)]
struct Notification<'a> {
    click: Click<'a>,
}

#[derive(Serialize)]
struct Click<'a> {
    url: &'a str,
}

impl GotifyNotifier {
    pub fn new(client: OutboundHttpClient, publish_url: Url, token: String) -> Self {
        Self {
            client,
            publish_url,
            token,
        }
    }

    fn publish(notice: &IncidentNotice) -> Publish<'_> {
        let resolved = notice.reason.closes_incident();
        Publish {
            title: notice.label(),
            message: truncate_chars(&notice.plain_text(), MAX_MESSAGE_CHARS),
            priority: match (resolved, notice.urgency) {
                // A resolve is news, not a page: bar icon, no sound.
                (true, _) => 3,
                (false, IncidentUrgency::High) => 8,
                (false, IncidentUrgency::Low) => 5,
            },
            extras: notice.url.as_deref().map(|url| Extras {
                notification: Notification {
                    click: Click { url },
                },
            }),
        }
    }
}

#[async_trait]
impl Notifier for GotifyNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        // The query-string form of the token would land in the server's
        // access log; the header does not.
        let headers = BTreeMap::from([("X-Gotify-Key".to_string(), self.token.clone())]);
        post_json_with_headers(
            &self.client,
            &self.publish_url,
            &Self::publish(notice),
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
    use crate::domain::{IncidentOrigin, IncidentSeverity, NotificationReason};

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
        let v = serde_json::to_value(GotifyNotifier::publish(&n)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "title": "api-prod",
                "message": "api-prod — major incident OPEN\nhttps://app.uptimepage.dev/i/7",
                "priority": 8,
                "extras": {
                    "client::notification": {
                        "click": {"url": "https://app.uptimepage.dev/i/7"}
                    }
                }
            })
        );
    }

    #[test]
    fn priorities_follow_lifecycle_and_a_urlless_notice_carries_no_extras() {
        let low = notice(NotificationReason::Opened, IncidentUrgency::Low);
        assert_eq!(GotifyNotifier::publish(&low).priority, 5);

        let resolved = notice(NotificationReason::Resolved, IncidentUrgency::High);
        assert_eq!(GotifyNotifier::publish(&resolved).priority, 3);

        let mut no_url = notice(NotificationReason::Opened, IncidentUrgency::High);
        no_url.url = None;
        let v = serde_json::to_value(GotifyNotifier::publish(&no_url)).unwrap();
        assert!(v.get("extras").is_none());
    }
}
