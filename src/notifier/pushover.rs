use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::domain::IncidentUrgency;
use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, get_json, post_json, post_json_capture};
use crate::notifier::event::IncidentNotice;
use crate::notifier::transport::Notifier;
use crate::text::truncate_chars;

const MESSAGES_URL: &str = "https://api.pushover.net/1/messages.json";
// Pushover caps: message 1024, title 250, url 512 characters.
const MAX_MESSAGE_CHARS: usize = 1024;
const MAX_TITLE_CHARS: usize = 250;
const MAX_URL_CHARS: usize = 512;
// Emergency (priority 2): re-alert every RETRY seconds until acknowledged, for
// at most EXPIRE seconds. Pushover floors retry at 30 s and caps expire at 3 h.
const EMERGENCY_RETRY_SECS: u32 = 60;
const EMERGENCY_EXPIRE_SECS: u32 = 3600;

pub struct PushoverNotifier {
    client: OutboundHttpClient,
    token: String,
    user: String,
    device: Option<String>,
    emergency: bool,
    receipt: Mutex<Option<String>>,
}

#[derive(Serialize)]
struct Message<'a> {
    token: &'a str,
    user: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<&'a str>,
    title: String,
    message: String,
    priority: i8,
    timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expire: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_title: Option<&'static str>,
}

#[derive(Deserialize)]
struct SendResponse {
    receipt: Option<String>,
}

/// Emergency priority applies only to a live high-urgency page — not to a
/// resolve (which goes silent) or a low-urgency notice.
fn is_emergency(enabled: bool, notice: &IncidentNotice) -> bool {
    enabled && matches!(notice.urgency, IncidentUrgency::High) && !notice.reason.closes_incident()
}

impl PushoverNotifier {
    pub fn new(
        client: OutboundHttpClient,
        token: String,
        user: String,
        device: Option<String>,
        emergency: bool,
    ) -> Self {
        Self {
            client,
            token,
            user,
            device,
            emergency,
            receipt: Mutex::new(None),
        }
    }

    fn message<'a>(
        token: &'a str,
        user: &'a str,
        device: Option<&'a str>,
        emergency: bool,
        notice: &'a IncidentNotice,
    ) -> Message<'a> {
        let url = notice
            .url
            .as_deref()
            .map(|u| truncate_chars(u, MAX_URL_CHARS));
        let emergency = is_emergency(emergency, notice);
        Message {
            token,
            user,
            device,
            title: truncate_chars(notice.label(), MAX_TITLE_CHARS),
            message: truncate_chars(&notice.plain_text(), MAX_MESSAGE_CHARS),
            priority: if emergency {
                2
            } else {
                match (notice.reason.closes_incident(), notice.urgency) {
                    (true, _) => -1,
                    (false, IncidentUrgency::High) => 1,
                    (false, IncidentUrgency::Low) => 0,
                }
            },
            retry: emergency.then_some(EMERGENCY_RETRY_SECS),
            expire: emergency.then_some(EMERGENCY_EXPIRE_SECS),
            // Pushover renders the push at this time — a resolve is news
            // from the resolve moment, not the open.
            timestamp: if notice.reason.closes_incident() {
                notice.ended_at.unwrap_or(notice.started_at)
            } else {
                notice.started_at
            }
            .timestamp(),
            url_title: url.is_some().then_some("Open incident"),
            url,
        }
    }
}

#[async_trait]
impl Notifier for PushoverNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let url: Url = MESSAGES_URL.parse().expect("static messages URL parses");
        let msg = Self::message(
            &self.token,
            &self.user,
            self.device.as_deref(),
            self.emergency,
            notice,
        );
        if msg.priority == 2 {
            let resp: SendResponse = post_json_capture(&self.client, &url, &msg).await?;
            if resp.receipt.is_none() {
                tracing::warn!("pushover accepted an emergency page but returned no receipt");
            }
            *self.receipt.lock().expect("receipt mutex") = resp.receipt;
            Ok(())
        } else {
            post_json(&self.client, &url, &msg).await
        }
    }

    fn taken_receipt(&self) -> Option<String> {
        self.receipt.lock().expect("receipt mutex").take()
    }
}

const RECEIPTS_BASE: &str = "https://api.pushover.net/1/receipts";

/// State of an emergency receipt at poll time.
pub struct ReceiptState {
    pub acknowledged: bool,
    pub expired: bool,
    /// User key of whoever acknowledged, which on a group key is one person
    /// in the group.
    pub acknowledged_by: Option<String>,
    /// Name of the device they acknowledged on.
    pub acknowledged_by_device: Option<String>,
}

#[derive(Deserialize)]
struct ReceiptResponse {
    acknowledged: i32,
    expired: i32,
    #[serde(default)]
    acknowledged_by: Option<String>,
    #[serde(default)]
    acknowledged_by_device: Option<String>,
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

/// Offer to link the Pushover account that just acknowledged. Quiet: they are
/// holding the phone already.
#[derive(Serialize)]
struct LinkOffer<'a> {
    token: &'a str,
    user: &'a str,
    title: &'static str,
    message: &'static str,
    url: &'a str,
    url_title: &'static str,
    priority: i8,
}

#[derive(Serialize)]
struct CancelBody<'a> {
    token: &'a str,
}

#[derive(Deserialize)]
struct CancelResponse {}

/// What the channel's application token does outside a page: poll and cancel
/// emergency receipts, and offer the account that acknowledged one a link.
/// Built per operation from the stored Pushover config.
pub struct PushoverReceipts {
    client: OutboundHttpClient,
    token: String,
}

impl PushoverReceipts {
    pub fn new(client: OutboundHttpClient, token: String) -> Self {
        Self { client, token }
    }

    fn url(&self, path: String) -> Result<Url> {
        path.parse()
            .map_err(|e| AppError::Other(anyhow::anyhow!("receipt url: {e}")))
    }

    /// Whether the recipient has acknowledged the page, and whether Pushover has
    /// stopped retrying it (acknowledged, expired, or cancelled).
    pub async fn poll(&self, receipt: &str) -> Result<ReceiptState> {
        // The token rides in the query string; scrub it from any error before it
        // reaches a log line.
        let url = self.url(format!(
            "{RECEIPTS_BASE}/{receipt}.json?token={}",
            self.token
        ))?;
        let r: ReceiptResponse = get_json(&self.client, &url).await.map_err(|e| {
            AppError::Other(anyhow::anyhow!(
                "pushover receipt poll: {}",
                e.to_string().replace(self.token.as_str(), "***")
            ))
        })?;
        Ok(ReceiptState {
            acknowledged: r.acknowledged == 1,
            expired: r.expired == 1,
            acknowledged_by: non_empty(r.acknowledged_by),
            acknowledged_by_device: non_empty(r.acknowledged_by_device),
        })
    }

    /// Send `user`, and only them, the link that ties their Pushover account
    /// to whoever opens it and signs in.
    pub async fn offer_link(&self, user: &str, link: &str) -> Result<()> {
        let url: Url = MESSAGES_URL.parse().expect("static messages URL parses");
        post_json(
            &self.client,
            &url,
            &LinkOffer {
                token: &self.token,
                user,
                title: "Put your name on it",
                message: "You acknowledged an incident, but this Pushover account is not \
                          linked to anyone in Uptimepage, so it went on record without a \
                          name. Open the link and sign in to link it.",
                url: link,
                url_title: "Link this Pushover account",
                priority: -1,
            },
        )
        .await
        .map_err(|e| {
            AppError::Other(anyhow::anyhow!(
                "pushover link offer: {}",
                e.to_string().replace(self.token.as_str(), "***")
            ))
        })
    }

    /// Stop the repeat loop for a receipt — the incident resolved before it was
    /// acknowledged.
    pub async fn cancel(&self, receipt: &str) -> Result<()> {
        let url = self.url(format!("{RECEIPTS_BASE}/{receipt}/cancel.json"))?;
        let _: CancelResponse =
            post_json_capture(&self.client, &url, &CancelBody { token: &self.token }).await?;
        Ok(())
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
    fn message_matches_wire_shape() {
        let n = notice(NotificationReason::Opened, IncidentUrgency::High);
        let v = serde_json::to_value(PushoverNotifier::message(
            "apptoken",
            "userkey",
            Some("droid2"),
            false,
            &n,
        ))
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "token": "apptoken",
                "user": "userkey",
                "device": "droid2",
                "title": "api-prod",
                "message": "api-prod — major incident OPEN\nhttps://app.uptimepage.dev/i/7",
                "priority": 1,
                "timestamp": 1781251200_i64,
                "url": "https://app.uptimepage.dev/i/7",
                "url_title": "Open incident"
            })
        );
    }

    #[test]
    fn priority_follows_urgency_and_resolve() {
        let msg = |n: &IncidentNotice| {
            serde_json::to_value(PushoverNotifier::message("t", "u", None, false, n)).unwrap()
        };
        let low = notice(NotificationReason::Opened, IncidentUrgency::Low);
        assert_eq!(msg(&low)["priority"], 0);
        let mut resolved = notice(NotificationReason::Resolved, IncidentUrgency::High);
        resolved.ended_at = Some(Utc.with_ymd_and_hms(2026, 6, 12, 15, 0, 0).unwrap());
        let rv = msg(&resolved);
        assert_eq!(rv["priority"], -1);
        // The resolve push carries the resolve moment, not the open time.
        assert_eq!(rv["timestamp"], resolved.ended_at.unwrap().timestamp());
        let mut closed = notice(NotificationReason::MonitorDeleted, IncidentUrgency::High);
        closed.ended_at = resolved.ended_at;
        let emergency =
            serde_json::to_value(PushoverNotifier::message("t", "u", None, true, &closed)).unwrap();
        assert_eq!(emergency["priority"], -1, "a closing notice never rings");
        let mut no_url = notice(NotificationReason::Opened, IncidentUrgency::High);
        no_url.url = None;
        let v = msg(&no_url);
        assert!(v.get("url").is_none());
        assert!(v.get("url_title").is_none());
    }

    #[test]
    fn emergency_only_arms_live_high_urgency_pages() {
        let msg = |n: &IncidentNotice| {
            serde_json::to_value(PushoverNotifier::message("t", "u", None, true, n)).unwrap()
        };
        // High-urgency open with emergency on: priority 2 + retry/expire.
        let v = msg(&notice(NotificationReason::Opened, IncidentUrgency::High));
        assert_eq!(v["priority"], 2);
        assert_eq!(v["retry"], EMERGENCY_RETRY_SECS);
        assert_eq!(v["expire"], EMERGENCY_EXPIRE_SECS);
        // Low urgency stays at 0 — emergency never escalates a non-critical page.
        let low = msg(&notice(NotificationReason::Opened, IncidentUrgency::Low));
        assert_eq!(low["priority"], 0);
        assert!(low.get("retry").is_none());
        // Resolves stay silent even with emergency on.
        let resolved = msg(&notice(NotificationReason::Resolved, IncidentUrgency::High));
        assert_eq!(resolved["priority"], -1);
        assert!(resolved.get("retry").is_none());
        // A reminder still rings until acknowledged; the backoff is what thins
        // them out, not a quieter priority.
        let reminder = msg(&notice(NotificationReason::Reminder, IncidentUrgency::High));
        assert_eq!(reminder["priority"], 2);
        assert_eq!(reminder["retry"], EMERGENCY_RETRY_SECS);
    }

    /// Pushover's receipt reply, trimmed to the fields read. An unacknowledged
    /// receipt carries the acknowledger fields empty rather than absent.
    #[test]
    fn a_receipt_names_the_user_key_that_acknowledged() {
        let acked: ReceiptResponse = serde_json::from_str(
            r#"{"status":1,"acknowledged":1,"acknowledged_at":1790000000,
                "acknowledged_by":"uQiRzpo4DXghDmr9QzzfQu27cmVRsG",
                "acknowledged_by_device":"iphone","expired":0}"#,
        )
        .unwrap();
        assert_eq!(
            non_empty(acked.acknowledged_by).as_deref(),
            Some("uQiRzpo4DXghDmr9QzzfQu27cmVRsG")
        );
        assert_eq!(
            non_empty(acked.acknowledged_by_device).as_deref(),
            Some("iphone")
        );

        let waiting: ReceiptResponse = serde_json::from_str(
            r#"{"status":1,"acknowledged":0,"acknowledged_by":"","acknowledged_by_device":"","expired":0}"#,
        )
        .unwrap();
        assert_eq!(non_empty(waiting.acknowledged_by), None);
        let bare: ReceiptResponse =
            serde_json::from_str(r#"{"acknowledged":0,"expired":1}"#).unwrap();
        assert_eq!(non_empty(bare.acknowledged_by), None);
    }
}
