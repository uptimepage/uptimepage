//! Slack's dialect of the alert card: Block Kit JSON, mrkdwn escaping, and
//! Slack's own date markup. Its limits are applied at construction, because an
//! over-long block is refused as `invalid_blocks`, which loses the whole alert
//! rather than a corner of it.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::notifier::card::{AlertCard, CardField, CardValue, MAX_ERROR_CHARS, Presses};
use crate::text::{single_line, truncate_chars};

const HEADER_MAX: usize = 150;
const SECTION_MAX: usize = 3000;
const FIELD_MAX: usize = 2000;
const MAX_FIELDS: usize = 10;

/// Escaping can quintuple the text (`&` becomes `&amp;`), so the shared error
/// cap holds against Slack's only while the worst case plus the fence still fits.
const _: () = assert!(MAX_ERROR_CHARS * 5 + 32 <= SECTION_MAX);

/// Name the presses on the buttons our own app receives, the only presses it
/// acts on.
pub const ACKNOWLEDGE_ACTION: &str = "acknowledge";
pub const RESOLVE_ACTION: &str = "resolve";

/// The card as Slack blocks. Who gets woken is the card's decision, so no
/// future call site can page a channel with an all-clear. A press is the
/// signed value of a button whose press reaches our app; it takes the place of
/// the card's link to the same page.
pub fn render(card: &AlertCard, mention: Option<&str>, presses: Presses<'_>) -> Vec<Block> {
    let mention = card.ping(mention);
    let headline = match mention {
        Some(m) => format!("{m} *{}*", escape(&card.headline)),
        None => format!("*{}*", escape(&card.headline)),
    };
    let mut blocks = vec![
        Block::header(&plain_label(&card.heading())),
        Block::section(headline),
    ];
    blocks.extend(Block::fields(card.fields.iter().map(field).collect()));
    if let Some(error) = &card.error {
        blocks.push(Block::section(format!("*Error*\n```{}```", fenced(error))));
    }
    if let Some(note) = &card.note {
        blocks.push(Block::context(escape(note)));
    }
    blocks.extend(Block::links(card, presses));
    blocks
}

fn field(f: &CardField) -> Text {
    let value = match &f.value {
        CardValue::Text(text) => escape(text),
        CardValue::Time(at) => stamp(*at),
    };
    Text::field(f.label, &value)
}

/// Slack renders this in each reader's own timezone, so an on-call in another
/// country does not convert UTC by hand at 3am.
fn stamp(at: DateTime<Utc>) -> String {
    format!(
        "<!date^{epoch}^{{date_short_pretty}} {{time}}|{fallback}>",
        epoch = at.timestamp(),
        fallback = at.format("%Y-%m-%d %H:%M UTC"),
    )
}

/// Error text for a code fence. Backticks in the payload would close the fence
/// early and hand the rest of the error to the mrkdwn parser.
fn fenced(error: &str) -> String {
    escape(error).replace('`', "'")
}

/// Escape the three characters Slack mrkdwn treats specially, so customer text
/// renders literally rather than as a live link or a mention.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Angle brackets carry every mrkdwn control sequence, and `plain_text` shows
/// entities verbatim, so a header cannot use [`escape`]: it drops them instead.
fn plain_label(s: &str) -> String {
    s.replace(['<', '>'], "")
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Header {
        text: Text,
    },
    Section {
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<Text>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        fields: Vec<Text>,
    },
    Context {
        elements: Vec<Text>,
    },
    Actions {
        elements: Vec<Element>,
    },
}

impl Block {
    fn header(text: &str) -> Self {
        Self::Header {
            text: Text::plain(text),
        }
    }

    fn section(markdown: String) -> Self {
        Self::Section {
            text: Some(Text::mrkdwn(markdown, SECTION_MAX)),
            fields: Vec::new(),
        }
    }

    /// Two-column layout, capped at the ten Slack accepts. `None` for an empty
    /// set, because a section with neither text nor fields is refused.
    fn fields(mut fields: Vec<Text>) -> Option<Self> {
        if fields.is_empty() {
            return None;
        }
        fields.truncate(MAX_FIELDS);
        Some(Self::Section { text: None, fields })
    }

    fn context(markdown: String) -> Self {
        Self::Context {
            elements: vec![Text::mrkdwn(markdown, SECTION_MAX)],
        }
    }

    /// Acknowledge first and highlighted, as the one thing the page asks for,
    /// then Resolve. `None` when there is nothing to open, because an empty
    /// actions block is refused.
    fn links(card: &AlertCard, presses: Presses<'_>) -> Option<Self> {
        let acknowledge = Element::control(
            "Acknowledge",
            (ACKNOWLEDGE_ACTION, "acknowledge_page"),
            Some("primary"),
            presses.acknowledge,
            card.ack_link.as_deref(),
        );
        let resolve = Element::control(
            "Resolve",
            (RESOLVE_ACTION, "resolve_page"),
            None,
            presses.resolve,
            card.resolve_link.as_deref(),
        );
        let view = card.link.as_deref().map(|url| Element::Button {
            text: Text::plain("View incident"),
            action_id: "view_incident",
            url: Some(url.to_string()),
            value: None,
            style: None,
        });
        let elements: Vec<Element> = acknowledge.into_iter().chain(resolve).chain(view).collect();
        (!elements.is_empty()).then_some(Self::Actions { elements })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Text {
    PlainText { text: String, emoji: bool },
    Mrkdwn { text: String },
}

impl Text {
    fn plain(text: &str) -> Self {
        Self::PlainText {
            text: truncate_chars(&single_line(text), HEADER_MAX),
            emoji: true,
        }
    }

    fn mrkdwn(text: String, max: usize) -> Self {
        Self::Mrkdwn {
            text: truncate_chars(&text, max),
        }
    }

    fn field(label: &str, value: &str) -> Self {
        Self::mrkdwn(format!("*{label}*\n{value}"), FIELD_MAX)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Element {
    Button {
        text: Text,
        /// Slack wants these unique within a block.
        action_id: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Handed back to our app when pressed.
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        style: Option<&'static str>,
    },
}

impl Element {
    /// A press our app receives when there is one, else a link to the page.
    /// `ids` are the action ids of the two, which Slack wants unique.
    fn control(
        label: &str,
        ids: (&'static str, &'static str),
        style: Option<&'static str>,
        press: Option<&str>,
        page: Option<&str>,
    ) -> Option<Self> {
        let (action_id, url, value) = match (press, page) {
            (Some(value), _) => (ids.0, None, Some(value.to_string())),
            (None, Some(url)) => (ids.1, Some(url.to_string()), None),
            (None, None) => return None,
        };
        Some(Self::Button {
            text: Text::plain(label),
            action_id,
            url,
            value,
            style,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NotificationReason;
    use crate::notifier::card::tests::notice;
    use serde_json::Value;

    fn json(reason: NotificationReason, mention: Option<&str>) -> String {
        let card = AlertCard::for_notice(&notice(reason), None);
        serde_json::to_string(&render(&card, mention, Presses::default())).unwrap()
    }

    #[test]
    fn an_open_incident_leads_with_the_ping_and_links_the_incident() {
        let blocks = json(NotificationReason::Opened, Some("<!here>"));
        assert!(blocks.contains(r#""text":"🔴 api-prod""#), "{blocks}");
        assert!(blocks.contains("<!here> *major incident OPEN*"), "{blocks}");
        assert!(blocks.contains("*Error*\\n```HTTP 500```"), "{blocks}");
        assert!(
            blocks.contains(r#""url":"https://app.test/i/7""#),
            "{blocks}"
        );
        assert!(blocks.contains("<!date^1700000000^"), "{blocks}");
    }

    /// An all-clear that pinged would wake the room the alert spared.
    #[test]
    fn only_the_events_that_need_a_human_carry_the_ping() {
        for reason in [
            NotificationReason::Opened,
            NotificationReason::Reopened,
            NotificationReason::Escalated,
            NotificationReason::NoData,
        ] {
            assert!(
                json(reason, Some("<!here>")).contains("<!here>"),
                "{reason:?}"
            );
        }
        for reason in [
            NotificationReason::Resolved,
            NotificationReason::DataResumed,
            NotificationReason::Reminder,
        ] {
            assert!(
                !json(reason, Some("<!here>")).contains("<!here>"),
                "{reason:?}"
            );
        }
    }

    /// The customer names the monitor and the error, and both land in a channel
    /// where `<!channel>` would wake everybody.
    #[test]
    fn customer_text_cannot_smuggle_a_ping_into_the_message() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("<!channel> api".into());
        n.error_sample = Some("<!here> fix me".into());
        let blocks = serde_json::to_string(&render(
            &AlertCard::for_notice(&n, None),
            None,
            Presses::default(),
        ))
        .unwrap();
        assert!(!blocks.contains("<!channel>"), "{blocks}");
        assert!(!blocks.contains("<!here>"), "{blocks}");
    }

    /// Backticks in the error would close the fence early and hand the rest of
    /// the customer's text to the mrkdwn parser.
    #[test]
    fn an_error_cannot_break_out_of_its_code_fence() {
        let mut n = notice(NotificationReason::Opened);
        n.error_sample = Some("``` *not bold* ```".into());
        let blocks = serde_json::to_string(&render(
            &AlertCard::for_notice(&n, None),
            None,
            Presses::default(),
        ))
        .unwrap();
        assert_eq!(blocks.matches("```").count(), 2, "{blocks}");
    }

    /// `plain_text` shows entities verbatim, so escaping the header would put a
    /// literal `&amp;` in front of the responders.
    #[test]
    fn a_header_keeps_an_ampersand_but_never_slack_markup() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("search & index <!channel>".into());
        let v: Value = serde_json::from_str(
            &serde_json::to_string(&render(
                &AlertCard::for_notice(&n, None),
                None,
                Presses::default(),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(v[0]["text"]["text"], "🔴 search & index !channel");
    }

    /// The block names are Slack's contract: a renamed variant or a mangled
    /// serde attribute still compiles and still reads right, and Slack refuses
    /// every alert with `invalid_blocks`.
    #[test]
    fn blocks_serialize_to_slacks_wire_names() {
        let v = serde_json::to_value(render(
            &AlertCard::for_notice(&notice(NotificationReason::Opened), None),
            None,
            Presses::default(),
        ))
        .unwrap();
        assert_eq!(v[0]["type"], "header");
        assert_eq!(v[0]["text"]["type"], "plain_text");
        assert_eq!(v[0]["text"]["emoji"], true);
        assert_eq!(v[1]["type"], "section");
        assert_eq!(v[1]["text"]["type"], "mrkdwn");
        assert_eq!(v[2]["type"], "section");
        assert_eq!(v[2]["fields"][0]["type"], "mrkdwn");
        assert_eq!(v[4]["type"], "actions");
        assert_eq!(v[4]["elements"][0]["type"], "button");
        assert_eq!(v[4]["elements"][0]["text"]["type"], "plain_text");
    }

    /// The press opens the page in the reader's browser, where they sign in,
    /// so the button is a plain link even on an incoming webhook.
    #[test]
    fn an_open_incident_offers_acknowledge_before_the_incident_link() {
        let ack = crate::notifier::card::tests::ack_page();
        let card = AlertCard::for_notice(&notice(NotificationReason::Opened), Some(&ack));
        let v = serde_json::to_value(render(&card, None, Presses::default())).unwrap();
        let buttons = &v[4]["elements"];
        assert_eq!(buttons[0]["text"]["text"], "Acknowledge");
        assert_eq!(buttons[0]["url"], ack);
        assert_eq!(buttons[0]["style"], "primary");
        assert_eq!(buttons[1]["text"]["text"], "View incident");
        assert!(buttons[1].get("style").is_none(), "{v}");
        assert_ne!(buttons[0]["action_id"], buttons[1]["action_id"]);
    }

    /// A press on our own app's button comes back to us with its value, so it
    /// opens nothing and needs no page.
    #[test]
    fn a_button_our_app_receives_carries_its_value_instead_of_a_link() {
        let ack = crate::notifier::card::tests::ack_page();
        let card = AlertCard::for_notice(&notice(NotificationReason::Opened), Some(&ack));
        let v = serde_json::to_value(render(
            &card,
            None,
            Presses {
                acknowledge: Some("a-signed"),
                resolve: None,
            },
        ))
        .unwrap();
        let buttons = &v[4]["elements"];
        assert_eq!(buttons[0]["text"]["text"], "Acknowledge");
        assert_eq!(buttons[0]["action_id"], ACKNOWLEDGE_ACTION);
        assert_eq!(buttons[0]["value"], "a-signed");
        assert!(buttons[0].get("url").is_none(), "{v}");
        assert!(buttons[1].get("value").is_none(), "{v}");
    }

    /// Resolve is the quieter of the two, so it follows Acknowledge and takes
    /// no highlight.
    #[test]
    fn resolve_follows_acknowledge_and_comes_before_the_incident_link() {
        let ack = crate::notifier::card::tests::ack_page();
        let resolve = crate::notifier::card::tests::resolve_page();
        let notice = notice(NotificationReason::Opened);
        let card = AlertCard::for_notice(&notice, Some(&ack)).with_resolve(&notice, Some(&resolve));
        let v = serde_json::to_value(render(&card, None, Presses::default())).unwrap();
        let buttons = &v[4]["elements"];
        assert_eq!(buttons[1]["text"]["text"], "Resolve");
        assert_eq!(buttons[1]["url"], resolve);
        assert!(buttons[1].get("style").is_none(), "{v}");
        assert_eq!(buttons[2]["text"]["text"], "View incident");
        let ids: std::collections::HashSet<_> = buttons
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["action_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 3, "Slack wants action ids unique in a block");

        let without = AlertCard::for_notice(&notice, Some(&ack));
        let v = serde_json::to_value(render(&without, None, Presses::default())).unwrap();
        assert_eq!(v[4]["elements"][1]["text"]["text"], "View incident", "{v}");
    }

    #[test]
    fn a_resolve_press_our_app_receives_carries_its_value_under_its_own_action() {
        let notice = notice(NotificationReason::Opened);
        let card = AlertCard::for_notice(&notice, None);
        let presses = Presses {
            acknowledge: Some("a-signed"),
            resolve: Some("r-signed"),
        };
        let v = serde_json::to_value(render(&card, None, presses)).unwrap();
        let buttons = &v[4]["elements"];
        assert_eq!(buttons[1]["action_id"], RESOLVE_ACTION);
        assert_eq!(buttons[1]["value"], "r-signed");
        assert!(buttons[1].get("url").is_none(), "{v}");
    }

    /// A base URL set without a scheme is not a link Slack accepts, and Slack
    /// refuses the message rather than the button.
    #[test]
    fn an_unusable_link_costs_the_button_not_the_alert() {
        let mut n = notice(NotificationReason::Opened);
        n.url = Some("app.example.test/incidents/7".into());
        let blocks = serde_json::to_string(&render(
            &AlertCard::for_notice(&n, None),
            None,
            Presses::default(),
        ))
        .unwrap();
        assert!(!blocks.contains("actions"), "{blocks}");
        assert!(blocks.contains("major incident OPEN"), "{blocks}");
    }

    /// A page of HTML in the error field would push its section past Slack's
    /// cap, and Slack refuses the whole message rather than trimming it.
    #[test]
    fn no_block_can_exceed_slacks_cap() {
        let mut n = notice(NotificationReason::Opened);
        n.error_sample = Some("&".repeat(20_000));
        let v: Value = serde_json::to_value(render(
            &AlertCard::for_notice(&n, None),
            None,
            Presses::default(),
        ))
        .unwrap();

        fn worst(v: &Value, out: &mut usize) {
            match v {
                Value::Object(map) => {
                    if let Some(Value::String(s)) = map.get("text") {
                        *out = (*out).max(s.chars().count());
                    }
                    map.values().for_each(|v| worst(v, out));
                }
                Value::Array(items) => items.iter().for_each(|v| worst(v, out)),
                _ => {}
            }
        }
        let mut longest = 0;
        worst(&v, &mut longest);
        assert!(longest <= SECTION_MAX, "{longest}");
    }
}
