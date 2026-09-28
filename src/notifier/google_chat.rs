use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::Notifier;
use crate::notifier::card::acknowledge_link;
use crate::notifier::event::IncidentNotice;
use crate::notifier::truncate_chars;

/// Google Chat caps message text at 4096 characters.
const MAX_TEXT_CHARS: usize = 4096;

pub struct GoogleChatNotifier {
    client: OutboundHttpClient,
    webhook_url: Url,
    ack_link: Option<String>,
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
    buttons: [Button; 1],
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
    /// a link, so acknowledging happens on the page it opens.
    fn acknowledge(url: String) -> Self {
        Self {
            card_id: "acknowledge",
            card: Card {
                sections: [Section {
                    widgets: [Widget {
                        button_list: ButtonList {
                            buttons: [Button {
                                text: "Acknowledge",
                                on_click: OnClick {
                                    open_link: OpenLink { url },
                                },
                            }],
                        },
                    }],
                }],
            },
        }
    }
}

impl GoogleChatNotifier {
    pub fn new(client: OutboundHttpClient, webhook_url: Url) -> Self {
        Self {
            client,
            webhook_url,
            ack_link: None,
        }
    }

    pub fn with_ack_link(mut self, ack_link: Option<String>) -> Self {
        self.ack_link = ack_link;
        self
    }

    fn payload<'a>(&self, notice: &IncidentNotice, text: &'a str) -> GoogleChatPayload<'a> {
        GoogleChatPayload {
            text,
            cards: acknowledge_link(notice, self.ack_link.as_deref())
                .map(CardWithId::acknowledge)
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
    use crate::notifier::card::tests::notice;

    const ACK: &str = "https://app.test/incidents/7/acknowledge?org=1&episode=0";

    fn notifier() -> GoogleChatNotifier {
        GoogleChatNotifier::new(
            crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::relaxed_for_tests(),
            ),
            "https://chat.googleapis.com/v1/spaces/S/messages"
                .parse()
                .unwrap(),
        )
        .with_ack_link(Some(ACK.into()))
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
                "cardId": "acknowledge",
                "card": {"sections": [{"widgets": [{"buttonList": {"buttons": [{
                    "text": "Acknowledge",
                    "onClick": {"openLink": {"url": ACK}}
                }]}}]}]}
            }])
        );
    }
}
