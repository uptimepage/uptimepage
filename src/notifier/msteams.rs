//! Teams' dialect of the alert card: an Adaptive Card with a themed headline,
//! a fact set and an open-url action. Timestamps go out as Teams' own date
//! templating, so each reader sees their local time.

use async_trait::async_trait;
use chrono::SecondsFormat;
use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::card::{AlertCard, CardField, CardTone, CardValue, escape_markdown};
use crate::notifier::transport::Notifier;
use crate::text::truncate_chars;

const HEADING_MAX: usize = 256;
const TEXT_MAX: usize = 2048;
const FACT_VALUE_MAX: usize = 512;
use crate::notifier::event::IncidentNotice;

pub struct MsTeamsNotifier {
    client: OutboundHttpClient,
    webhook_url: Url,
    ack_link: Option<String>,
    resolve_link: Option<String>,
}

#[derive(Serialize)]
struct TeamsMessage {
    #[serde(rename = "type")]
    kind: &'static str,
    attachments: [Attachment; 1],
}

#[derive(Serialize)]
struct Attachment {
    #[serde(rename = "contentType")]
    content_type: &'static str,
    content: AdaptiveCard,
}

#[derive(Serialize)]
struct AdaptiveCard {
    #[serde(rename = "type")]
    kind: &'static str,
    version: &'static str,
    body: Vec<Body>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<Action>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum Body {
    TextBlock {
        text: String,
        wrap: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        weight: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        color: Option<&'static str>,
        #[serde(rename = "fontType", skip_serializing_if = "Option::is_none")]
        font_type: Option<&'static str>,
        #[serde(rename = "isSubtle", skip_serializing_if = "std::ops::Not::not")]
        subtle: bool,
    },
    FactSet {
        facts: Vec<Fact>,
    },
}

impl Body {
    fn text(text: String) -> Self {
        Self::TextBlock {
            text,
            wrap: true,
            size: None,
            weight: None,
            color: None,
            font_type: None,
            subtle: false,
        }
    }
}

#[derive(Serialize)]
struct Fact {
    title: String,
    value: String,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum Action {
    #[serde(rename = "Action.OpenUrl")]
    OpenUrl {
        title: &'static str,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        style: Option<&'static str>,
    },
}

impl MsTeamsNotifier {
    pub fn new(client: OutboundHttpClient, webhook_url: Url) -> Self {
        Self {
            client,
            webhook_url,
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

    fn message(card: &AlertCard) -> TeamsMessage {
        let mut body = vec![
            Body::TextBlock {
                text: cap(&card.heading(), HEADING_MAX),
                wrap: true,
                size: Some("Large"),
                weight: Some("Bolder"),
                color: None,
                font_type: None,
                subtle: false,
            },
            Body::TextBlock {
                text: cap(&card.headline, TEXT_MAX),
                wrap: true,
                size: None,
                weight: Some("Bolder"),
                color: Some(theme(card.tone)),
                font_type: None,
                subtle: false,
            },
        ];
        if !card.fields.is_empty() {
            body.push(Body::FactSet {
                facts: card.fields.iter().map(fact).collect(),
            });
        }
        if let Some(error) = &card.error {
            body.push(Body::TextBlock {
                text: cap(error, TEXT_MAX),
                wrap: true,
                size: None,
                weight: None,
                color: None,
                font_type: Some("Monospace"),
                subtle: true,
            });
        }
        if let Some(note) = &card.note {
            body.push(Body::text(cap(note, TEXT_MAX)));
        }
        TeamsMessage {
            kind: "message",
            attachments: [Attachment {
                content_type: "application/vnd.microsoft.card.adaptive",
                content: AdaptiveCard {
                    kind: "AdaptiveCard",
                    version: "1.4",
                    body,
                    actions: actions(card),
                },
            }],
        }
    }
}

/// A Workflows card opens URLs and nothing else, so acknowledging and
/// resolving happen on the page the button opens, Acknowledge first and
/// highlighted.
fn actions(card: &AlertCard) -> Vec<Action> {
    let acknowledge = card.ack_link.iter().map(|url| Action::OpenUrl {
        title: "Acknowledge",
        url: url.clone(),
        style: Some("positive"),
    });
    let resolve = card.resolve_link.iter().map(|url| Action::OpenUrl {
        title: "Resolve",
        url: url.clone(),
        style: None,
    });
    let view = card.link.iter().map(|url| Action::OpenUrl {
        title: "View incident",
        url: url.clone(),
        style: None,
    });
    acknowledge.chain(resolve).chain(view).collect()
}

fn fact(f: &CardField) -> Fact {
    let value = match &f.value {
        CardValue::Text(text) => cap(text, FACT_VALUE_MAX),
        // Teams resolves this to the reader's own locale and timezone.
        CardValue::Time(at) => {
            let iso = at.to_rfc3339_opts(SecondsFormat::Secs, true);
            format!("{{{{DATE({iso}, SHORT)}}}} {{{{TIME({iso})}}}}")
        }
    };
    Fact {
        title: f.label.to_string(),
        value,
    }
}

fn theme(tone: CardTone) -> &'static str {
    match tone {
        CardTone::Critical | CardTone::Major => "Attention",
        CardTone::Minor | CardTone::Warning => "Warning",
        CardTone::Recovered | CardTone::Resumed => "Good",
    }
}

/// A text block renders a markdown subset, so customer text is escaped before
/// it can format itself. Braces are not escaped but broken, and `#` is left
/// alone: headers are not in the supported subset, and a backslash before a
/// character the subset does not know is consumed rather than kept.
/// Teams has no per-field limit but does cap the whole card, so every block is
/// bounded here.
fn cap(s: &str, max: usize) -> String {
    truncate_chars(
        &escape_markdown(&defuse_date_functions(s), &['\\', '`', '*', '_', '[', ']']),
        max,
    )
}

/// Teams evaluates `{{DATE(…)}}` wherever it appears in a text block. Escaping
/// the braces does not stop it — the backslash is eaten as a markdown escape
/// and the pair closes up again — so the pair is split by a space, which the
/// date syntax forbids between its braces, leaving the text to render as the
/// name it is.
fn defuse_date_functions(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '{' && out.ends_with('{') {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[async_trait]
impl Notifier for MsTeamsNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let card = AlertCard::for_notice(notice, self.ack_link.as_deref())
            .with_resolve(notice, self.resolve_link.as_deref());
        let message = Self::message(&card);
        post_json(&self.client, &self.webhook_url, &message).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NotificationReason;
    use crate::notifier::card::tests::notice;
    use serde_json::Value;

    fn card(n: &IncidentNotice) -> Value {
        let v = serde_json::to_value(MsTeamsNotifier::message(&AlertCard::for_notice(n, None)))
            .unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(
            v["attachments"][0]["contentType"],
            "application/vnd.microsoft.card.adaptive"
        );
        v["attachments"][0]["content"].clone()
    }

    #[test]
    fn an_open_incident_becomes_an_adaptive_card() {
        let v = card(&notice(NotificationReason::Opened));
        assert_eq!(v["type"], "AdaptiveCard");
        assert_eq!(v["body"][0]["text"], "🔴 api-prod");
        assert_eq!(v["body"][1]["text"], "major incident OPEN");
        assert_eq!(v["body"][1]["color"], "Attention");
        assert_eq!(v["body"][2]["facts"][0]["title"], "Started");
        assert_eq!(
            v["body"][2]["facts"][0]["value"],
            "{{DATE(2023-11-14T22:13:20Z, SHORT)}} {{TIME(2023-11-14T22:13:20Z)}}"
        );
        assert_eq!(v["body"][3]["text"], "HTTP 500");
        assert_eq!(v["actions"][0]["type"], "Action.OpenUrl");
        assert_eq!(v["actions"][0]["url"], "https://app.test/i/7");
    }

    #[test]
    fn a_resolved_incident_reads_as_recovery() {
        let mut n = notice(NotificationReason::Resolved);
        n.ended_at = Some(n.started_at + chrono::Duration::minutes(95));
        let v = card(&n);
        assert_eq!(v["body"][0]["text"], "✅ api-prod");
        assert_eq!(v["body"][1]["color"], "Good");
        assert_eq!(v["body"][2]["facts"][2]["value"], "1h 35m");
    }

    #[test]
    fn an_open_incident_offers_acknowledge_before_the_incident_link() {
        let ack = crate::notifier::card::tests::ack_page();
        let n = notice(NotificationReason::Opened);
        let v = serde_json::to_value(MsTeamsNotifier::message(&AlertCard::for_notice(
            &n,
            Some(&ack),
        )))
        .unwrap();
        let actions = &v["attachments"][0]["content"]["actions"];
        assert_eq!(actions[0]["type"], "Action.OpenUrl");
        assert_eq!(actions[0]["title"], "Acknowledge");
        assert_eq!(actions[0]["url"], ack);
        assert_eq!(actions[0]["style"], "positive");
        assert_eq!(actions[1]["title"], "View incident");
        assert!(actions[1].get("style").is_none(), "{actions}");
    }

    #[test]
    fn resolve_follows_acknowledge_and_takes_no_highlight() {
        let ack = crate::notifier::card::tests::ack_page();
        let resolve = crate::notifier::card::tests::resolve_page();
        let n = notice(NotificationReason::Opened);
        let card = AlertCard::for_notice(&n, Some(&ack)).with_resolve(&n, Some(&resolve));
        let v = serde_json::to_value(MsTeamsNotifier::message(&card)).unwrap();
        let actions = &v["attachments"][0]["content"]["actions"];
        assert_eq!(actions[1]["title"], "Resolve");
        assert_eq!(actions[1]["url"], resolve);
        assert!(actions[1].get("style").is_none(), "{actions}");
        assert_eq!(actions[2]["title"], "View incident");
    }

    /// An incident with no link at all must still deliver, without an action
    /// pointing nowhere.
    #[test]
    fn an_incident_without_a_link_carries_no_action() {
        let mut n = notice(NotificationReason::Opened);
        n.url = None;
        assert!(card(&n).get("actions").is_none());
    }

    #[test]
    fn customer_text_renders_literally() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("api *prod*".into());
        let v = card(&n);
        assert_eq!(v["body"][0]["text"], r"🔴 api \*prod\*");
    }

    /// Teams interpolates its date templating anywhere in a text block, so a
    /// monitor name must not be able to show the responder a made-up time.
    /// The brace pair has to be broken by something markdown will not eat:
    /// a backslash before a brace is consumed and the pair closes up again.
    #[test]
    fn a_monitor_name_cannot_fabricate_a_timestamp() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("{{DATE(2001-09-11T00:00:00Z, SHORT)}}".into());
        let heading = card(&n)["body"][0]["text"].as_str().unwrap().to_string();
        assert!(!heading.contains("{{"), "{heading}");
        assert!(
            !heading.contains("\\{"),
            "no backslash to be eaten: {heading}"
        );
        assert!(
            heading.contains("{ {DATE("),
            "the name still reads back: {heading}"
        );
        // Repeats must not close up into a fresh pair.
        n.monitor_name = Some("{{{{DATE(2001-09-11T00:00:00Z, SHORT)}}}}".into());
        let heading = card(&n)["body"][0]["text"].as_str().unwrap().to_string();
        assert!(!heading.contains("{{"), "{heading}");
    }

    /// Teams caps the whole card, and a monitor name has no length limit
    /// anywhere in the product.
    #[test]
    fn an_unbounded_name_cannot_cost_the_alert() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("x".repeat(40_000));
        n.note = Some("y".repeat(40_000));
        let v = card(&n);
        for block in v["body"].as_array().unwrap() {
            let text = block["text"].as_str().unwrap_or_default();
            assert!(text.chars().count() <= TEXT_MAX, "{}", text.len());
        }
    }
}
