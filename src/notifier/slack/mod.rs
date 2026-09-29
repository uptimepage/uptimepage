mod blocks;

pub use blocks::{ACKNOWLEDGE_ACTION, escape};

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::domain::NotificationReason;
use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::Notifier;
use crate::notifier::event::IncidentNotice;

use crate::notifier::card::AlertCard;

use blocks::{Block, render};

pub struct SlackNotifier {
    client: OutboundHttpClient,
    webhook_url: Url,
    /// Already rendered to markup; the raw token would post as plain text.
    mention: Option<String>,
    ack_link: Option<String>,
    /// Signed value of an Acknowledge button our own app receives. Only on a
    /// channel connected through it: a press on a pasted webhook's message
    /// goes to whichever app owns that webhook.
    ack_press: Option<String>,
}

/// `text` is what a push notification and a client that cannot render blocks
/// show, so it carries the whole alert on its own.
#[derive(Serialize)]
struct SlackPayload<'a> {
    text: &'a str,
    blocks: Vec<Block>,
}

impl SlackNotifier {
    pub fn new(client: OutboundHttpClient, webhook_url: Url, mention: Option<String>) -> Self {
        Self {
            client,
            webhook_url,
            mention,
            ack_link: None,
            ack_press: None,
        }
    }

    pub fn with_ack_link(mut self, ack_link: Option<String>) -> Self {
        self.ack_link = ack_link;
        self
    }

    pub fn with_ack_press(mut self, value: Option<String>) -> Self {
        self.ack_press = value;
        self
    }

    /// Both halves of the message, from one card: the blocks Slack renders and
    /// the line a push notification shows.
    fn compose(
        mention: Option<&str>,
        ack_link: Option<&str>,
        ack_press: Option<&str>,
        n: &IncidentNotice,
    ) -> (String, Vec<Block>) {
        let card = AlertCard::for_notice(n, ack_link);
        let text = Self::render_incident(card.ping(mention), &card, n);
        let ack_press = ack_press.filter(|_| n.reason.awaits_acknowledgement());
        (text, render(&card, mention, ack_press))
    }

    /// From the card's values, not the notice's: they are bounded there, and
    /// Slack refuses an over-long payload rather than trimming it.
    fn render_incident(mention: Option<&str>, card: &AlertCard, n: &IncidentNotice) -> String {
        let body = match &card.note {
            Some(note) => format!("{}\n{}", Self::incident_line(card, n), escape(note)),
            None => Self::incident_line(card, n),
        };
        match mention {
            Some(m) => format!("{m} {body}"),
            None => body,
        }
    }

    /// One line for a push notification, so it stays terser than the card
    /// rather than reusing it. A field added to the card does not reach here.
    fn incident_line(card: &AlertCard, n: &IncidentNotice) -> String {
        // The card's link, not the raw one: a base URL without a scheme has no
        // button on the card, so it must not have a dead link here either.
        let link = card
            .link
            .as_deref()
            .map(|u| format!(" <{u}|view incident>"))
            .unwrap_or_default();
        // Customer-supplied monitor name + error text must not inject Slack
        // markup (live `<url|text>` links, `@channel` mentions) into the
        // responders' channel.
        let label = escape(&card.title);
        match n.reason {
            NotificationReason::Opened
            | NotificationReason::Escalated
            | NotificationReason::Reopened
            | NotificationReason::Reminder => format!(
                "*{label}* — {sev} incident {state}{err}{regions}{link}",
                sev = n.severity.as_db_str(),
                state = n.open_state(),
                err = card
                    .error
                    .as_deref()
                    .map(|e| format!(": {}", escape(e)))
                    .unwrap_or_default(),
                regions = region_line(n),
            ),
            NotificationReason::Resolved => {
                let dur = n
                    .duration_minutes()
                    .map(|m| format!(" after {m}m"))
                    .unwrap_or_default();
                format!("*{label}* — incident RESOLVED{dur}{link}")
            }
            NotificationReason::NoData => {
                format!("*{label}* — NO DATA: monitoring interrupted{link}")
            }
            NotificationReason::DataResumed => format!("*{label}* — monitoring RESUMED{link}"),
        }
    }
}

/// Per-region breakdown line, escaped for mrkdwn; empty for single-region.
fn region_line(n: &IncidentNotice) -> String {
    n.region_summary(escape, " • ")
        .map(|s| format!("\n• {s}"))
        .unwrap_or_default()
}

#[async_trait]
impl Notifier for SlackNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let (text, blocks) = Self::compose(
            self.mention.as_deref(),
            self.ack_link.as_deref(),
            self.ack_press.as_deref(),
            notice,
        );
        post_json(
            &self.client,
            &self.webhook_url,
            &SlackPayload {
                text: &text,
                blocks,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::card::tests::notice;

    /// Slack refuses an over-long payload, so an unbounded fallback line
    /// loses the whole alert while every block around it renders fine.
    #[test]
    fn a_huge_monitor_name_cannot_bloat_the_fallback_line() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("A".repeat(60_000));
        n.note = Some("N".repeat(60_000));
        let (text, _) = SlackNotifier::compose(None, None, None, &n);
        assert!(
            text.chars().count() < 2_000,
            "fallback text ran to {} chars",
            text.chars().count()
        );
        assert!(
            text.contains('…'),
            "the name is trimmed, not dropped: {text}"
        );
    }

    /// Slack does not use the shared body renderer, so a note added there
    /// reaches it only because this transport appends it too.
    #[test]
    fn a_note_reaches_slack_even_though_it_renders_its_own_body() {
        let mut n = notice(NotificationReason::Opened);
        n.note = Some("Flapping: alerts held".into());
        let (text, _) = SlackNotifier::compose(None, None, None, &n);
        assert!(text.contains("Flapping: alerts held"), "{text}");
    }

    /// One ping decision serves both halves; a fallback line that shouted while
    /// the card stayed quiet would wake the room anyway.
    #[test]
    fn the_text_and_the_card_ping_the_same_events() {
        for reason in [
            NotificationReason::Opened,
            NotificationReason::Reopened,
            NotificationReason::Escalated,
            NotificationReason::NoData,
            NotificationReason::Resolved,
            NotificationReason::DataResumed,
            NotificationReason::Reminder,
        ] {
            let (text, blocks) =
                SlackNotifier::compose(Some("<!here>"), None, None, &notice(reason));
            let card = serde_json::to_string(&blocks).unwrap();
            assert_eq!(
                text.contains("<!here>"),
                card.contains("<!here>"),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn a_mention_leads_the_alert_but_stays_off_the_all_clear() {
        let (opened, _) = SlackNotifier::compose(
            Some("<!here>"),
            None,
            None,
            &notice(NotificationReason::Opened),
        );
        assert!(opened.starts_with("<!here> *api-prod*"), "{opened}");
        let (resolved, _) = SlackNotifier::compose(
            Some("<!here>"),
            None,
            None,
            &notice(NotificationReason::Resolved),
        );
        assert!(!resolved.contains("<!here>"), "{resolved}");
    }

    /// A button minted for an incident still to be taken must not ride along
    /// on the all-clear.
    #[test]
    fn only_an_incident_still_to_be_taken_carries_the_press() {
        let press = |reason| {
            let (_, blocks) = SlackNotifier::compose(None, None, Some("a-signed"), &notice(reason));
            serde_json::to_string(&blocks).unwrap().contains("a-signed")
        };
        assert!(press(NotificationReason::Opened));
        assert!(!press(NotificationReason::Resolved));
    }
}
