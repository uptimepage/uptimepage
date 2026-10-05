use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::card::alert_link;
use crate::notifier::event::IncidentNotice;
use crate::notifier::transport::Notifier;
use crate::text::truncate_chars;

/// Google Chat caps message text at 4096 characters.
const MAX_TEXT_CHARS: usize = 4096;

pub struct GoogleChatNotifier {
    client: OutboundHttpClient,
    webhook_url: Url,
    ack_link: Option<String>,
    resolve_link: Option<String>,
}

/// The card renders below the text, which stays the whole alert on its own.
#[derive(Serialize)]
struct GoogleChatPayload<'a> {
    text: &'a str,
    #[serde(rename = "cardsV2", skip_serializing_if = "Vec::is_empty")]
    cards: Vec<CardWithId>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CardWithId {
    card_id: &'static str,
    card: Card,
}

#[derive(Serialize)]
struct Card {
    sections: [Section; 1],
}

#[derive(Serialize)]
struct Section {
    widgets: [Widget; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Widget {
    button_list: ButtonList,
}

#[derive(Serialize)]
struct ButtonList {
    buttons: Vec<Button>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Button {
    text: &'static str,
    on_click: OnClick,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OnClick {
    open_link: OpenLink,
}

#[derive(Serialize)]
struct OpenLink {
    url: String,
}

impl CardWithId {
    /// A webhook message cannot carry an action that reaches back to us, only
    /// a link, so acknowledging and resolving happen on the page it opens.
    /// `None` when there is no link to open.
    fn controls(links: [(&'static str, Option<String>); 2]) -> Option<Self> {
        let buttons: Vec<Button> = links
            .into_iter()
            .filter_map(|(text, url)| {
                url.map(|url| Button {
                    text,
                    on_click: OnClick {
                        open_link: OpenLink { url },
                    },
                })
            })
            .collect();
        (!buttons.is_empty()).then_some(Self {
            card_id: "alert_actions",
            card: Card {
                sections: [Section {
                    widgets: [Widget {
                        button_list: ButtonList { buttons },
                    }],
                }],
            },
        })
    }
}

impl GoogleChatNotifier {
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

    fn payload<'a>(&self, notice: &IncidentNotice, text: &'a str) -> GoogleChatPayload<'a> {
        GoogleChatPayload {
            text,
            cards: CardWithId::controls([
                ("Acknowledge", alert_link(notice, self.ack_link.as_deref())),
                ("Resolve", alert_link(notice, self.resolve_link.as_deref())),
            ])
            .into_iter()
            .collect(),
        }
    }
}

#[async_trait]
impl Notifier for GoogleChatNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let text = truncate_chars(&notice.plain_text(), MAX_TEXT_CHARS);
        post_json(
            &self.client,
            &self.webhook_url,
            &self.payload(notice, &text),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::NotificationReason;
    use crate::notifier::card::tests::{ack_page, notice, resolve_page};

    fn notifier() -> GoogleChatNotifier {
        GoogleChatNotifier::new(
            crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
            ),
            "https://chat.googleapis.com/v1/spaces/S/messages"
                .parse()
                .unwrap(),
        )
        .with_ack_link(Some(ack_page()))
    }

    #[test]
    fn an_all_clear_is_plain_text() {
        let n = notice(NotificationReason::Resolved);
        let v =
            serde_json::to_value(notifier().payload(&n, "api-prod — incident RESOLVED")).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "text": "api-prod — incident RESOLVED" })
        );
    }

    #[test]
    fn an_open_incident_carries_an_acknowledge_button_under_the_text() {
        let n = notice(NotificationReason::Opened);
        let v =
            serde_json::to_value(notifier().payload(&n, "api-prod — major incident OPEN")).unwrap();
        assert_eq!(v["text"], "api-prod — major incident OPEN");
        assert_eq!(
            v["cardsV2"],
            serde_json::json!([{
                "cardId": "alert_actions",
                "card": {"sections": [{"widgets": [{"buttonList": {"buttons": [{
                    "text": "Acknowledge",
                    "onClick": {"openLink": {"url": ack_page()}}
                }]}}]}]}
            }])
        );
    }

    #[test]
    fn resolve_follows_acknowledge_in_the_same_button_list() {
        let resolve = resolve_page();
        let sender = notifier().with_resolve_link(Some(resolve.clone()));
        let v = serde_json::to_value(sender.payload(
            &notice(NotificationReason::Opened),
            "api-prod — major incident OPEN",
        ))
        .unwrap();
        let buttons =
            &v["cardsV2"][0]["card"]["sections"][0]["widgets"][0]["buttonList"]["buttons"];
        assert_eq!(buttons[0]["text"], "Acknowledge");
        assert_eq!(buttons[1]["text"], "Resolve");
        assert_eq!(buttons[1]["onClick"]["openLink"]["url"], resolve);
    }
}
