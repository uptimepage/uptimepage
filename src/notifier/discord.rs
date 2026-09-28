//! Discord's dialect of the alert card: an embed with a coloured bar, inline
//! fields and Discord's own timestamp markup. Limits are applied per field,
//! because Discord refuses the whole message rather than trimming an embed.

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::domain::DiscordMention;
use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::Notifier;
use crate::notifier::card::{AlertCard, CardField, CardTone, CardValue, escape_markdown};
use crate::notifier::event::IncidentNotice;
use crate::notifier::truncate_chars;

const TITLE_MAX: usize = 256;
const DESCRIPTION_MAX: usize = 2048;
const FIELD_NAME_MAX: usize = 32;
const FIELD_VALUE_MAX: usize = 512;
const MAX_FIELDS: usize = 6;
/// Discord counts the whole embed, not each field, and refuses the message over
/// it. The per-field caps above are only safe while their worst case fits here.
const EMBED_MAX: usize = 6000;
const _: () = assert!(
    TITLE_MAX + DESCRIPTION_MAX + MAX_FIELDS * (FIELD_NAME_MAX + FIELD_VALUE_MAX) <= EMBED_MAX
);
/// The description is trimmed after its code fence is closed, so a headline,
/// an error and a note have to fit whole. Over the cap the cut lands inside
/// the fence and the unterminated backticks swallow the rest of the embed.
const _: () = assert!(
    crate::notifier::card::MAX_ERROR_CHARS + crate::notifier::card::MAX_NOTE_CHARS + 256
        <= DESCRIPTION_MAX
);

pub struct DiscordNotifier {
    client: OutboundHttpClient,
    webhook_url: Url,
    ping: Option<Ping>,
    ack_link: Option<String>,
}

struct Ping {
    content: String,
    allowed: AllowedMentions,
}

#[derive(Serialize)]
struct DiscordPayload<'a> {
    /// Discord resolves no mention inside an embed, so the ping rides here.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    embeds: [Embed; 1],
    /// Sent even when empty: without it Discord resolves whatever looks like a
    /// mention anywhere in the message, monitor names included.
    allowed_mentions: &'a AllowedMentions,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    components: Vec<ActionRow>,
}

/// A row of link buttons: the one kind a webhook we do not own may send, and
/// the press opens a browser rather than reaching any app.
#[derive(Serialize)]
struct ActionRow {
    #[serde(rename = "type")]
    kind: u8,
    components: [LinkButton; 1],
}

#[derive(Serialize)]
struct LinkButton {
    #[serde(rename = "type")]
    kind: u8,
    style: u8,
    label: &'static str,
    url: String,
}

impl ActionRow {
    fn acknowledge(url: &str) -> Self {
        Self {
            kind: 1,
            components: [LinkButton {
                kind: 2,
                style: 5,
                label: "Acknowledge",
                url: url.to_string(),
            }],
        }
    }
}

static SILENT: AllowedMentions = AllowedMentions {
    parse: Vec::new(),
    roles: Vec::new(),
    users: Vec::new(),
};

#[derive(Serialize)]
struct AllowedMentions {
    parse: Vec<&'static str>,
    roles: Vec<String>,
    users: Vec<String>,
}

#[derive(Serialize)]
struct Embed {
    title: String,
    description: String,
    color: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fields: Vec<EmbedField>,
}

#[derive(Serialize)]
struct EmbedField {
    name: String,
    value: String,
    inline: bool,
}

impl DiscordNotifier {
    pub fn new(
        client: OutboundHttpClient,
        mut webhook_url: Url,
        mention: Option<DiscordMention>,
    ) -> Self {
        // Without `wait=true` Discord answers 204 before delivering, hiding
        // failures from the retry loop, and without `with_components=true` a
        // webhook we do not own drops the buttons. Replaced, not appended: a
        // pasted URL already carrying either would leave two, and which one
        // Discord honours is not specified.
        let kept: Vec<(String, String)> = webhook_url
            .query_pairs()
            .filter(|(k, _)| k != "wait" && k != "with_components")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        webhook_url
            .query_pairs_mut()
            .clear()
            .extend_pairs(kept)
            .append_pair("wait", "true")
            .append_pair("with_components", "true");
        Self {
            client,
            webhook_url,
            ping: mention.map(|m| Ping {
                content: m.markup.clone(),
                allowed: allowed(&m),
            }),
            ack_link: None,
        }
    }

    pub fn with_ack_link(mut self, ack_link: Option<String>) -> Self {
        self.ack_link = ack_link;
        self
    }

    fn payload(&self, card: &AlertCard) -> DiscordPayload<'_> {
        let ping = card.ping(self.ping.as_ref());
        DiscordPayload {
            content: ping.map(|p| p.content.as_str()),
            embeds: [Self::embed(card)],
            allowed_mentions: ping.map_or(&SILENT, |p| &p.allowed),
            // The embed title already opens the incident.
            components: card
                .ack_link
                .as_deref()
                .map(ActionRow::acknowledge)
                .into_iter()
                .collect(),
        }
    }

    fn embed(card: &AlertCard) -> Embed {
        let mut description = format!("**{}**", escape(&card.headline));
        if let Some(error) = &card.error {
            description.push_str(&format!("\n```{}```", fenced(error)));
        }
        if let Some(note) = &card.note {
            description.push_str(&format!("\n{}", escape(note)));
        }
        Embed {
            // Discord renders no markdown in an embed title, so escaping it
            // would only print the backslashes.
            title: truncate_chars(&card.heading(), TITLE_MAX),
            description: truncate_chars(&description, DESCRIPTION_MAX),
            color: color(card.tone),
            url: card.link.clone(),
            fields: card.fields.iter().take(MAX_FIELDS).map(field).collect(),
        }
    }
}

fn field(f: &CardField) -> EmbedField {
    let value = match &f.value {
        CardValue::Text(text) => escape(text),
        // Rendered in the reader's own timezone by their client.
        CardValue::Time(at) => format!("<t:{}:f>", at.timestamp()),
    };
    EmbedField {
        name: truncate_chars(f.label, FIELD_NAME_MAX),
        value: truncate_chars(&value, FIELD_VALUE_MAX),
        inline: true,
    }
}

/// Loudest first: the bar has to descend with severity, or the quieter
/// incident is the one that catches the eye.
fn color(tone: CardTone) -> u32 {
    match tone {
        CardTone::Critical => 0xED_42_45,
        CardTone::Major => 0xE6_7E_22,
        CardTone::Minor => 0xF1_C4_0F,
        CardTone::Warning => 0x95_A5_A6,
        CardTone::Recovered | CardTone::Resumed => 0x2E_CC_71,
    }
}

/// Customer text renders literally rather than as emphasis or a live link.
/// Mentions need no handling: Discord does not resolve them inside an embed.
fn escape(s: &str) -> String {
    escape_markdown(s, &['\\', '`', '*', '_', '~', '|', '[', ']', '<', '>'])
}

/// Error text for a code fence. Backticks in the payload would close the fence
/// early and hand the rest of the error to the markdown parser.
fn fenced(error: &str) -> String {
    error.replace('`', "'")
}

/// Exactly what the operator configured, so a ping cannot widen on the way out.
fn allowed(mention: &DiscordMention) -> AllowedMentions {
    AllowedMentions {
        parse: if mention.everyone {
            vec!["everyone"]
        } else {
            Vec::new()
        },
        roles: mention.roles.clone(),
        users: mention.users.clone(),
    }
}

#[async_trait]
impl Notifier for DiscordNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let payload = self.payload(&AlertCard::for_notice(notice, self.ack_link.as_deref()));
        post_json(&self.client, &self.webhook_url, &payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NotificationReason;
    use crate::notifier::card::tests::notice;

    fn notifier(url: &str) -> DiscordNotifier {
        DiscordNotifier::new(
            crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
            ),
            url.parse().unwrap(),
            None,
        )
    }

    fn payload(mention: Option<&str>, n: &IncidentNotice) -> serde_json::Value {
        let cfg = crate::domain::DiscordConfig {
            webhook_url: "https://discord.com/api/webhooks/123/tok".into(),
            mention: mention.map(str::to_string),
        };
        let sender = DiscordNotifier::new(
            crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
            ),
            "https://discord.com/api/webhooks/123/tok".parse().unwrap(),
            cfg.mention_targets(),
        );
        serde_json::to_value(sender.payload(&AlertCard::for_notice(n, None))).unwrap()
    }

    /// A second `wait` pair leaves the retry loop blind to a failed send.
    #[test]
    fn an_existing_wait_pair_is_replaced_not_doubled() {
        let sender = notifier(
            "https://discord.com/api/webhooks/1/tok?wait=false&thread_id=7&with_components=false",
        );
        let pairs: Vec<(String, String)> = sender
            .webhook_url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("thread_id".to_string(), "7".to_string()),
                ("wait".to_string(), "true".to_string()),
                ("with_components".to_string(), "true".to_string())
            ]
        );
    }

    fn embed(n: &IncidentNotice) -> serde_json::Value {
        serde_json::to_value(DiscordNotifier::embed(&AlertCard::for_notice(n, None))).unwrap()
    }

    #[test]
    fn send_url_carries_wait_for_synchronous_errors() {
        let n = notifier("https://discord.com/api/webhooks/123/tok");
        assert_eq!(
            n.webhook_url.as_str(),
            "https://discord.com/api/webhooks/123/tok?wait=true&with_components=true"
        );
        let threaded = notifier("https://discord.com/api/webhooks/123/tok?thread_id=42");
        assert_eq!(
            threaded.webhook_url.as_str(),
            "https://discord.com/api/webhooks/123/tok?thread_id=42&wait=true&with_components=true"
        );
    }

    #[test]
    fn an_open_incident_becomes_a_coloured_embed() {
        let v = embed(&notice(NotificationReason::Opened));
        assert_eq!(v["title"], "🔴 api-prod");
        assert_eq!(v["color"], 0xE6_7E_22);
        assert_eq!(v["url"], "https://app.test/i/7");
        assert!(
            v["description"]
                .as_str()
                .unwrap()
                .starts_with("**major incident OPEN**"),
            "{v}"
        );
        assert_eq!(v["fields"][0]["name"], "Started");
        assert_eq!(v["fields"][0]["value"], "<t:1700000000:f>");
    }

    #[test]
    fn a_resolved_incident_turns_the_bar_green() {
        let mut n = notice(NotificationReason::Resolved);
        n.ended_at = Some(n.started_at + chrono::Duration::minutes(95));
        let v = embed(&n);
        assert_eq!(v["color"], 0x2E_CC_71);
        assert_eq!(v["fields"][2]["value"], "1h 35m");
    }

    /// Customer text must not turn into emphasis, a link, or a broken fence.
    #[test]
    fn customer_text_renders_literally() {
        let mut n = notice(NotificationReason::Opened);
        n.error_sample = Some("``` **boom** ```".into());
        let v = embed(&n);
        let description = v["description"].as_str().unwrap();
        assert_eq!(description.matches("```").count(), 2, "{description}");
        assert!(description.contains("'''"), "{description}");
    }

    /// A monitor whose name is a page of text would push the embed past
    /// Discord's caps, and Discord refuses the message outright.
    #[test]
    fn an_over_long_field_is_cut_to_discords_cap() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("x".repeat(5_000));
        n.note = Some("y".repeat(5_000));
        let v = embed(&n);
        assert!(v["title"].as_str().unwrap().chars().count() <= TITLE_MAX);
        assert!(v["description"].as_str().unwrap().chars().count() <= DESCRIPTION_MAX);
    }

    #[test]
    fn a_ping_rides_the_content_line_under_an_allow_list() {
        let v = payload(
            Some("@here &123456789012345678"),
            &notice(NotificationReason::Opened),
        );
        assert_eq!(v["content"], "@here <@&123456789012345678>");
        assert_eq!(v["allowed_mentions"]["parse"][0], "everyone");
        assert_eq!(v["allowed_mentions"]["roles"][0], "123456789012345678");
    }

    #[test]
    fn recovery_carries_no_ping_and_still_forbids_every_mention() {
        let v = payload(Some("@everyone"), &notice(NotificationReason::Resolved));
        assert!(v.get("content").is_none(), "{v}");
        assert_eq!(v["allowed_mentions"]["parse"].as_array().unwrap().len(), 0);
        assert_eq!(v["allowed_mentions"]["roles"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_monitor_name_cannot_ping_the_server() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("@everyone".into());
        let v = payload(None, &n);
        assert!(v.get("content").is_none(), "{v}");
        assert_eq!(v["allowed_mentions"]["parse"].as_array().unwrap().len(), 0);
        assert!(
            v["embeds"][0]["title"]
                .as_str()
                .unwrap()
                .contains("everyone"),
            "the name is still reported: {v}"
        );
    }

    /// A pasted webhook is not ours, so only a link button survives it, and
    /// Discord wants that one without a `custom_id`.
    #[test]
    fn an_open_incident_carries_an_acknowledge_link_button() {
        let ack = crate::notifier::card::tests::ack_page();
        let card = AlertCard::for_notice(&notice(NotificationReason::Opened), Some(&ack));
        let v =
            serde_json::to_value(notifier("https://discord.com/api/webhooks/1/tok").payload(&card))
                .unwrap();
        assert_eq!(
            v["components"],
            serde_json::json!([{
                "type": 1,
                "components": [{"type": 2, "style": 5, "label": "Acknowledge", "url": ack}]
            }])
        );
        assert!(
            payload(None, &notice(NotificationReason::Resolved))
                .get("components")
                .is_none()
        );
    }

    /// Discord renders no markdown in an embed title, so escaping one only
    /// prints the backslashes — and `_` is ordinary in a monitor name.
    #[test]
    fn a_monitor_name_reaches_the_title_unescaped() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("api_prod **PROD** ~~down~~".into());
        assert_eq!(embed(&n)["title"], "🔴 api_prod **PROD** ~~down~~");
    }
}
