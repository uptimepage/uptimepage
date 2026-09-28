//! The alert as facts, before any vendor spells it: what belongs on the card
//! and how it reads is decided once here, and Slack, Discord and Teams each
//! render it into their own JSON. Customer text stays raw, timestamps stay
//! typed, and no markup of any dialect appears, because the escaping and the
//! date syntax differ per vendor.

use chrono::{DateTime, Utc};

use crate::domain::{IncidentOrigin, IncidentSeverity, NotificationReason};
use crate::notifier::event::IncidentNotice;
use crate::text::{single_line, truncate_chars};

/// Error text is customer-controlled and unbounded in storage, and every
/// vendor refuses an over-long payload outright rather than trimming it.
pub const MAX_ERROR_CHARS: usize = 500;

/// The note is ours rather than the customer's, but it shares the vendors'
/// budget, so it is bounded here instead of at each renderer.
pub const MAX_NOTE_CHARS: usize = 300;

/// A monitor name has no length limit anywhere in the product, and a vendor
/// refuses an over-long payload rather than trimming it.
pub const MAX_TITLE_CHARS: usize = 200;

/// Discord's cap on a link button's URL, the tightest of the vendors'. A base
/// URL is operator config, so an acknowledge link past it is dropped rather
/// than costing the message.
pub const MAX_ACK_LINK_CHARS: usize = 512;

/// How loud the card should look. Each transport maps this to what it has:
/// an emoji, a colour bar, a themed text block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardTone {
    Critical,
    Major,
    Minor,
    Warning,
    Recovered,
    Resumed,
}

impl CardTone {
    pub fn icon(self) -> &'static str {
        match self {
            Self::Critical => "🚨",
            Self::Major => "🔴",
            Self::Minor => "🟠",
            Self::Warning => "⚠️",
            Self::Recovered => "✅",
            Self::Resumed => "🟢",
        }
    }
}

/// A timestamp stays typed so each transport can hand it to its own
/// client-side formatter and show it in the reader's timezone.
#[derive(Debug, Clone)]
pub enum CardValue {
    Text(String),
    Time(DateTime<Utc>),
}

#[derive(Debug, Clone)]
pub struct CardField {
    pub label: &'static str,
    pub value: CardValue,
}

impl CardField {
    fn text(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: CardValue::Text(value.into()),
        }
    }

    fn time(label: &'static str, at: DateTime<Utc>) -> Self {
        Self {
            label,
            value: CardValue::Time(at),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AlertCard {
    pub tone: CardTone,
    /// Monitor name. Customer text, unescaped.
    pub title: String,
    /// The state in one line, no emphasis markup.
    pub headline: String,
    pub fields: Vec<CardField>,
    /// What the check saw. Customer text, capped, unescaped.
    pub error: Option<String>,
    pub note: Option<String>,
    /// Deep link to the incident, only when it is one a client will accept.
    pub link: Option<String>,
    /// The page where a signed-in member acknowledges the episode this alert
    /// is about. Only on an incident still to be taken.
    pub ack_link: Option<String>,
    /// Whether the event needs a human. Transports that carry a ping apply
    /// their own here; the rest ignore it.
    pub pings: bool,
}

/// The predefined layouts, one per kind of news. The reason picks one, so a new
/// kind of event adds a variant instead of another branch inside a renderer.
#[derive(Debug, Clone, Copy)]
pub enum CardTemplate {
    Incident,
    Resolved,
    NoData,
    DataResumed,
}

impl CardTemplate {
    pub fn for_reason(reason: NotificationReason) -> Self {
        match reason {
            NotificationReason::Opened
            | NotificationReason::Reopened
            | NotificationReason::Escalated
            | NotificationReason::Reminder => Self::Incident,
            NotificationReason::Resolved => Self::Resolved,
            NotificationReason::NoData => Self::NoData,
            NotificationReason::DataResumed => Self::DataResumed,
        }
    }

    pub fn card(self, n: &IncidentNotice, ack_link: Option<&str>) -> AlertCard {
        let mut card = AlertCard {
            tone: self.tone(n),
            title: truncate_chars(n.label(), MAX_TITLE_CHARS),
            headline: self.headline(n),
            fields: Vec::new(),
            error: None,
            note: n.note.as_deref().map(|s| truncate_chars(s, MAX_NOTE_CHARS)),
            link: n.url.as_deref().filter(|u| is_web_link(u)).map(Into::into),
            ack_link: acknowledge_link(n, ack_link),
            pings: matches!(
                n.reason,
                NotificationReason::Opened
                    | NotificationReason::Reopened
                    | NotificationReason::Escalated
                    | NotificationReason::NoData
            ),
        };
        match self {
            Self::Incident => {
                card.fields.push(CardField::time("Started", n.started_at));
                if let Some(regions) = n.region_summary(str::to_string, " · ") {
                    card.fields.push(CardField::text("Regions", regions));
                }
                // No failing check behind a declared incident, so the card must
                // not read as a detection the product cannot support.
                if n.origin == IncidentOrigin::Manual {
                    card.fields.push(CardField::text(
                        "Origin",
                        "declared by hand, no monitor detection",
                    ));
                }
                card.error = n
                    .error_sample
                    .as_deref()
                    .map(|e| truncate_chars(e, MAX_ERROR_CHARS));
            }
            Self::Resolved => {
                card.fields.push(CardField::time("Started", n.started_at));
                if let Some(end) = n.ended_at {
                    card.fields.push(CardField::time("Resolved", end));
                }
                if let Some(minutes) = n.duration_minutes() {
                    card.fields
                        .push(CardField::text("Duration", spell_duration(minutes)));
                }
            }
            Self::NoData => card.fields.push(CardField::time("Since", n.started_at)),
            Self::DataResumed => {}
        }
        card
    }

    fn tone(self, n: &IncidentNotice) -> CardTone {
        match self {
            Self::Incident => match n.severity {
                IncidentSeverity::Critical => CardTone::Critical,
                IncidentSeverity::Major => CardTone::Major,
                IncidentSeverity::Minor => CardTone::Minor,
            },
            Self::NoData => CardTone::Warning,
            Self::Resolved => CardTone::Recovered,
            // An outage that ended and a monitor that started reporting again
            // ask for different follow-up, so they do not look alike.
            Self::DataResumed => CardTone::Resumed,
        }
    }

    fn headline(self, n: &IncidentNotice) -> String {
        match self {
            Self::Incident => format!(
                "{sev} incident {state}",
                sev = n.severity.as_db_str(),
                state = n.open_state(),
            ),
            Self::Resolved => "Incident resolved".into(),
            Self::NoData => "No data: monitoring interrupted, no check results received".into(),
            Self::DataResumed => "Monitoring resumed, receiving check results again".into(),
        }
    }
}

impl AlertCard {
    pub fn for_notice(n: &IncidentNotice, ack_link: Option<&str>) -> Self {
        CardTemplate::for_reason(n.reason).card(n, ack_link)
    }

    /// Title with its tone in front, the one line every transport leads with.
    /// Flattened, because a monitor name is customer text and a newline in a
    /// card heading pushes the state below the fold.
    pub fn heading(&self) -> String {
        single_line(&format!("{} {}", self.tone.icon(), self.title))
    }

    /// The one owner of who gets woken: transports that carry a ping ask here
    /// rather than deciding for themselves.
    pub fn ping<'a, T: ?Sized>(&self, mention: Option<&'a T>) -> Option<&'a T> {
        mention.filter(|_| self.pings)
    }
}

/// The acknowledge page a page about `n` may link to. Only an incident still
/// to be taken has anything to acknowledge, and a link a client would refuse
/// costs the link rather than the message.
pub fn acknowledge_link(n: &IncidentNotice, link: Option<&str>) -> Option<String> {
    link.filter(|u| {
        n.reason.awaits_acknowledgement()
            && is_web_link(u)
            && u.chars().count() <= MAX_ACK_LINK_CHARS
    })
    .map(Into::into)
}

/// A link a client refuses can cost the whole message, so a base URL set
/// without a scheme costs the link and nothing else.
fn is_web_link(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Backslash-escape a vendor's markdown characters, so customer text renders
/// literally rather than as emphasis or a link. Slack escapes by entity and
/// keeps its own; Discord and Teams differ only in which characters bite.
pub fn escape_markdown(s: &str, specials: &[char]) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if specials.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn spell_duration(minutes: i64) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::domain::IncidentUrgency;
    use uuid::Uuid;

    /// Shared by the transport renderers, so every vendor is exercised against
    /// the same incident.
    pub(crate) fn notice(reason: NotificationReason) -> IncidentNotice {
        IncidentNotice {
            incident_id: Uuid::from_u128(7),
            reason,
            monitor_name: Some("api-prod".into()),
            title: None,
            severity: IncidentSeverity::Major,
            urgency: IncidentUrgency::High,
            origin: IncidentOrigin::Monitor,
            started_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            ended_at: None,
            error_sample: Some("HTTP 500".into()),
            regions_down: Vec::new(),
            regions_up: Vec::new(),
            url: Some("https://app.test/i/7".into()),
            note: None,
        }
    }

    fn labels(card: &AlertCard) -> Vec<&str> {
        card.fields.iter().map(|f| f.label).collect()
    }

    #[test]
    fn an_open_incident_carries_what_a_responder_needs() {
        let card = AlertCard::for_notice(&notice(NotificationReason::Opened), None);
        assert_eq!(card.heading(), "🔴 api-prod");
        assert_eq!(card.headline, "major incident OPEN");
        assert_eq!(labels(&card), ["Started"]);
        assert_eq!(card.error.as_deref(), Some("HTTP 500"));
        assert!(card.pings);
    }

    #[test]
    fn a_resolved_incident_reports_how_long_it_ran() {
        let mut n = notice(NotificationReason::Resolved);
        n.ended_at = Some(n.started_at + chrono::Duration::minutes(95));
        let card = AlertCard::for_notice(&n, None);
        assert_eq!(labels(&card), ["Started", "Resolved", "Duration"]);
        assert!(matches!(&card.fields[2].value, CardValue::Text(v) if v == "1h 35m"));
        assert!(!card.pings, "an all-clear does not wake anybody");
    }

    #[test]
    fn a_multi_region_incident_names_the_regions() {
        let mut n = notice(NotificationReason::Opened);
        n.regions_down = vec!["eu-helsinki".into()];
        n.regions_up = vec!["apac-sg".into()];
        let card = AlertCard::for_notice(&n, None);
        assert!(matches!(
            &card.fields[1].value,
            CardValue::Text(v) if v == "down: eu-helsinki · not confirmed: apac-sg"
        ));
    }

    #[test]
    fn a_single_region_incident_omits_the_breakdown() {
        let mut n = notice(NotificationReason::Opened);
        n.regions_down = vec!["eu-helsinki".into()];
        assert_eq!(labels(&AlertCard::for_notice(&n, None)), ["Started"]);
    }

    /// A declared incident that reads like a detection makes the product look
    /// as if it misfired, and only an open one still describes the present.
    #[test]
    fn a_declared_incident_says_so_instead_of_claiming_a_detection() {
        let mut n = notice(NotificationReason::Opened);
        n.origin = IncidentOrigin::Manual;
        assert!(labels(&AlertCard::for_notice(&n, None)).contains(&"Origin"));

        n.reason = NotificationReason::Resolved;
        assert!(!labels(&AlertCard::for_notice(&n, None)).contains(&"Origin"));
    }

    /// A base URL set without a scheme is not a link any client accepts, and
    /// they refuse the message rather than the link.
    #[test]
    fn an_unusable_link_is_dropped_before_it_reaches_a_transport() {
        let mut n = notice(NotificationReason::Opened);
        n.url = Some("app.example.test/incidents/7".into());
        assert!(AlertCard::for_notice(&n, None).link.is_none());
    }

    const ACK: &str = "https://app.test/incidents/7/acknowledge?org=1&episode=0";

    /// An all-clear or a monitoring gap has nothing to take, and a button on
    /// one would promise an action the page then refuses.
    #[test]
    fn only_an_incident_still_to_be_taken_offers_the_acknowledge_page() {
        for reason in [
            NotificationReason::Opened,
            NotificationReason::Reopened,
            NotificationReason::Escalated,
            NotificationReason::Reminder,
        ] {
            let card = AlertCard::for_notice(&notice(reason), Some(ACK));
            assert_eq!(card.ack_link.as_deref(), Some(ACK), "{reason:?}");
        }
        for reason in [
            NotificationReason::Resolved,
            NotificationReason::NoData,
            NotificationReason::DataResumed,
        ] {
            let card = AlertCard::for_notice(&notice(reason), Some(ACK));
            assert!(card.ack_link.is_none(), "{reason:?}");
        }
    }

    /// Discord refuses the whole message over a link button's URL cap, and a
    /// base URL without a scheme is no link at all.
    #[test]
    fn an_acknowledge_link_a_client_would_refuse_is_dropped() {
        let n = notice(NotificationReason::Opened);
        let long = format!("https://app.test/{}", "x".repeat(MAX_ACK_LINK_CHARS));
        assert!(AlertCard::for_notice(&n, Some(&long)).ack_link.is_none());
        let bare = "app.test/incidents/7/acknowledge";
        assert!(AlertCard::for_notice(&n, Some(bare)).ack_link.is_none());
    }

    /// Recovery and a monitor that merely started reporting again ask for
    /// different follow-up, so a responder can tell them apart at a glance.
    #[test]
    fn resumed_monitoring_does_not_look_like_a_resolved_outage() {
        let resolved = AlertCard::for_notice(&notice(NotificationReason::Resolved), None);
        let resumed = AlertCard::for_notice(&notice(NotificationReason::DataResumed), None);
        assert_ne!(resolved.tone.icon(), resumed.tone.icon());
    }

    /// A monitor name is customer text, and a newline in it would push the
    /// state out of a card heading.
    #[test]
    fn a_heading_stays_on_one_line() {
        let mut n = notice(NotificationReason::Opened);
        n.monitor_name = Some("api-prod\nstaging".into());
        assert_eq!(
            AlertCard::for_notice(&n, None).heading(),
            "🔴 api-prod staging"
        );
    }

    /// A page of HTML in the error field would push a vendor's payload past its
    /// cap, and they refuse the whole message rather than trimming it.
    #[test]
    fn a_huge_error_is_capped_once_for_every_transport() {
        let mut n = notice(NotificationReason::Opened);
        n.error_sample = Some("x".repeat(20_000));
        let card = AlertCard::for_notice(&n, None);
        assert_eq!(card.error.unwrap().chars().count(), MAX_ERROR_CHARS);
    }
}
