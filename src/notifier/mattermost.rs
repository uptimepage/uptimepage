//! The server scans an attachment's title, body, pretext and fields for
//! mentions. Its fallback, and the inside of a code fence, are never scanned.

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::card::{AlertCard, CardField, CardTone, CardValue};
use crate::notifier::event::IncidentNotice;
use crate::notifier::transport::Notifier;
use crate::text::truncate_chars;

const FALLBACK_MAX: usize = 1000;
const TITLE_MAX: usize = 256;
const TEXT_MAX: usize = 4096;
const FIELD_VALUE_MAX: usize = 512;
const MAX_FIELDS: usize = 6;
/// Mattermost splits a post over this into several.
const POST_MAX: usize = 16_383;
const _: () = assert!(
    FALLBACK_MAX + TITLE_MAX + TEXT_MAX + MAX_FIELDS * (64 + FIELD_VALUE_MAX) + 256 <= POST_MAX
);
/// A cut inside the fence would leave it unterminated, and one at the end
/// would take the acknowledge and resolve links.
const _: () = assert!(
    crate::notifier::card::MAX_ERROR_CHARS
        + crate::notifier::card::MAX_NOTE_CHARS
        + 2 * crate::notifier::card::MAX_ACK_LINK_CHARS
        + 256
        <= TEXT_MAX
);

pub struct MattermostNotifier {
    client: OutboundHttpClient,
    webhook_url: Url,
    mention: Option<String>,
    ack_link: Option<String>,
    resolve_link: Option<String>,
}

/// No top-level message: it is scanned for mentions. A push notification
/// reads the attachment's fallback when the message is empty.
#[derive(Serialize)]
struct MattermostPayload {
    attachments: [Attachment; 1],
}

#[derive(Serialize)]
struct Attachment {
    fallback: String,
    color: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pretext: Option<String>,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title_link: Option<String>,
    text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fields: Vec<Field>,
}

#[derive(Serialize)]
struct Field {
    title: &'static str,
    value: String,
    short: bool,
}

impl MattermostNotifier {
    pub fn new(client: OutboundHttpClient, webhook_url: Url, mention: Option<String>) -> Self {
        Self {
            client,
            webhook_url,
            mention,
            ack_link: None,
            resolve_link: None,
        }
    }

    pub fn with_ack_link(mut self, ack_link: Option<String>) -> Self {
        self.ack_link = ack_link;
        self
    }

    pub fn with_resolve_link(mut self, resolve_link: Option<String>) -> Self {
        self.resolve_link = resolve_link;
        self
    }

    fn payload(&self, notice: &IncidentNotice) -> MattermostPayload {
        let card = AlertCard::for_notice(notice, self.ack_link.as_deref())
            .with_resolve(notice, self.resolve_link.as_deref());
        let ping = card.ping(self.mention.as_deref());
        MattermostPayload {
            attachments: [Self::attachment(&card, ping, notice)],
        }
    }

    fn attachment(card: &AlertCard, ping: Option<&str>, n: &IncidentNotice) -> Attachment {
        let mut text = format!("**{}**", card.headline);
        if let Some(error) = &card.error {
            text.push_str(&format!("\n```\n{}\n```", fenced(error)));
        }
        if let Some(note) = &card.note {
            text.push_str(&format!("\n{note}"));
        }
        // An attachment button posts to an integration server-side instead of
        // opening a browser, so the pages ride as links.
        let links: Vec<String> = [
            ("Acknowledge", &card.ack_link),
            ("Resolve", &card.resolve_link),
        ]
        .into_iter()
        .filter_map(|(label, url)| url.as_ref().map(|url| format!("[{label}]({url})")))
        .collect();
        if !links.is_empty() {
            text.push_str(&format!("\n{}", links.join(" · ")));
        }
        Attachment {
            fallback: truncate_chars(&n.summary(), FALLBACK_MAX),
            color: color(card.tone),
            pretext: ping.map(str::to_string),
            // Truncate before defusing: a cut must not split an @ from its joiner.
            title: defuse_mentions(&truncate_chars(&card.heading(), TITLE_MAX)),
            title_link: card.link.clone(),
            text: truncate_chars(&text, TEXT_MAX),
            fields: card.fields.iter().take(MAX_FIELDS).map(field).collect(),
        }
    }
}

fn field(f: &CardField) -> Field {
    let value = match &f.value {
        CardValue::Text(text) => text.clone(),
        // Mattermost has no client-side timestamp markup.
        CardValue::Time(at) => at.format("%Y-%m-%d %H:%M UTC").to_string(),
    };
    Field {
        title: f.label,
        value: truncate_chars(&value, FIELD_VALUE_MAX),
        short: true,
    }
}

/// An `@` is read as a mention only when it opens a word; a zero-width space
/// closes the word without printing anything.
fn defuse_mentions(s: &str) -> String {
    s.replace('@', "@\u{200b}")
}

fn color(tone: CardTone) -> &'static str {
    match tone {
        CardTone::Critical => "#ED4245",
        CardTone::Major => "#E67E22",
        CardTone::Minor => "#F1C40F",
        CardTone::Warning => "#95A5A6",
        CardTone::Recovered | CardTone::Resumed => "#2ECC71",
    }
}

/// A backtick in the error would close the fence early.
fn fenced(error: &str) -> String {
    error.replace('`', "'")
}

#[async_trait]
impl Notifier for MattermostNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        post_json(&self.client, &self.webhook_url, &self.payload(notice)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NotificationReason;
    use crate::notifier::card::tests::notice;

    fn notifier(mention: Option<&str>) -> MattermostNotifier {
        let cfg = crate::domain::MattermostConfig {
            webhook_url: "https://mm.example.com/hooks/abc123".into(),
            mention: mention.map(str::to_string),
        };
        MattermostNotifier::new(
            crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
            ),
            cfg.webhook_url.parse().unwrap(),
            cfg.mention_markup(),
        )
    }

    #[test]
    fn an_open_incident_matches_the_wire_shape() {
        let v = serde_json::to_value(
            notifier(Some("here")).payload(&notice(NotificationReason::Opened)),
        )
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "attachments": [{
                    "fallback": "api-prod — major incident OPEN: HTTP 500",
                    "color": "#E67E22",
                    "pretext": "@here",
                    "title": "🔴 api-prod",
                    "title_link": "https://app.test/i/7",
                    "text": "**major incident OPEN**\n```\nHTTP 500\n```",
                    "fields": [
                        {"title": "Started", "value": "2023-11-14 22:13 UTC", "short": true}
                    ]
                }]
            })
        );
    }

    #[test]
    fn an_open_incident_ends_with_the_acknowledge_link() {
        let ack = crate::notifier::card::tests::ack_page();
        let mut n = notice(NotificationReason::Opened);
        n.note = Some("N".repeat(60_000));
        n.error_sample = Some("E".repeat(60_000));
        let v = serde_json::to_value(notifier(None).with_ack_link(Some(ack.clone())).payload(&n))
            .unwrap();
        let text = v["attachments"][0]["text"].as_str().unwrap();
        assert!(text.ends_with(&format!("\n[Acknowledge]({ack})")), "{text}");
    }

    #[test]
    fn resolve_follows_acknowledge_on_the_same_line_and_survives_the_cap() {
        let ack = crate::notifier::card::tests::ack_page();
        let resolve = crate::notifier::card::tests::resolve_page();
        let mut n = notice(NotificationReason::Opened);
        n.note = Some("N".repeat(60_000));
        n.error_sample = Some("E".repeat(60_000));
        let sender = notifier(None)
            .with_ack_link(Some(ack.clone()))
            .with_resolve_link(Some(resolve.clone()));
        let v = serde_json::to_value(sender.payload(&n)).unwrap();
        let text = v["attachments"][0]["text"].as_str().unwrap();
        assert!(
            text.ends_with(&format!("\n[Acknowledge]({ack}) · [Resolve]({resolve})")),
            "{text}"
        );
    }

    #[test]
    fn an_all_clear_carries_no_ping() {
        let v = serde_json::to_value(
            notifier(Some("here")).payload(&notice(NotificationReason::Resolved)),
        )
        .unwrap();
        assert!(v["attachments"][0].get("pretext").is_none());
        assert_eq!(v["attachments"][0]["color"], "#2ECC71");
    }

    #[test]
    fn an_error_body_cannot_break_out_of_its_fence() {
        let mut n = notice(NotificationReason::Opened);
        n.error_sample = Some("boom ``` @channel".into());
        let v = serde_json::to_value(notifier(None).payload(&n)).unwrap();
        let text = v["attachments"][0]["text"].as_str().unwrap();
        assert_eq!(text.matches("```").count(), 2);
        assert!(text.ends_with("boom ''' @channel\n```"));
    }

    #[test]
    fn a_monitor_name_cannot_ping_the_channel_from_the_title() {
        let mut n = notice(NotificationReason::Resolved);
        n.monitor_name = Some("@channel @here checkout-api".into());
        let v = serde_json::to_value(notifier(None).payload(&n)).unwrap();
        let title = v["attachments"][0]["title"].as_str().unwrap();
        assert_eq!(
            title.replace('\u{200b}', ""),
            "✅ @channel @here checkout-api"
        );
        for word in title.split_whitespace() {
            assert!(
                !matches!(word, "@channel" | "@here" | "@all"),
                "live broadcast mention in title: {title:?}"
            );
        }
        assert!(
            !v["attachments"][0]["fallback"]
                .as_str()
                .unwrap()
                .contains('\u{200b}')
        );
    }

    #[test]
    fn a_huge_monitor_name_and_error_stay_inside_the_post_budget() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("A".repeat(60_000));
        n.error_sample = Some("E".repeat(60_000));
        n.note = Some("N".repeat(60_000));
        let json = serde_json::to_string(&notifier(None).payload(&n)).unwrap();
        assert!(json.chars().count() < POST_MAX, "{}", json.chars().count());
    }
}
