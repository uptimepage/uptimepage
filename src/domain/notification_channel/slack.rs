use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::mention::{cleared_when_empty, tokens, validate as validate_mention, without_broadcast};
use super::transport::{MASK, TransportConfig, require_provider_webhook, trim_in_place};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SlackConfig {
    /// Incoming-webhook URL. The path carries the workspace token, so the
    /// whole value is treated as a secret.
    pub webhook_url: String,
    /// Who to ping on an alert: `@here`, `@channel`, a user-group id (`S…`)
    /// or a member id (`U…`), space or comma separated. Visible routing, not
    /// a secret, so it survives redaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
}

impl SlackConfig {
    pub fn mention_markup(&self) -> Option<String> {
        mention_markup(self.mention.as_deref())
    }
}

/// The markup `mention` renders as, `None` when it pings nobody. A typo drops
/// out alone rather than costing the on-call ping beside it.
pub(super) fn mention_markup(mention: Option<&str>) -> Option<String> {
    let mut markup: Vec<String> = Vec::new();
    for token in tokens(mention?).filter_map(token_markup) {
        if !markup.contains(&token) {
            markup.push(token);
        }
    }
    (!markup.is_empty()).then(|| markup.join(" "))
}

pub(super) fn validate_ping(mention: Option<&str>) -> Result<(), String> {
    validate_mention(
        mention,
        |t| token_markup(t).is_some(),
        |t| {
            format!(
                "Slack cannot ping \"{t}\" — use @here, @channel, a user-group id (S…) \
                 or a member id (U… / W… on Enterprise Grid)"
            )
        },
    )
}

/// The ping without `@here` or `@channel`, for a test send.
pub(super) fn without_broadcast_ping(mention: Option<&str>) -> Option<String> {
    without_broadcast(mention, is_broadcast)
}

/// `@here` / `@channel`: everyone in the channel, not a named responder.
fn is_broadcast(token: &str) -> bool {
    let t = unwrap_markup(token);
    let t = t.strip_prefix('@').unwrap_or(t);
    t.eq_ignore_ascii_case("here") || t.eq_ignore_ascii_case("channel")
}

/// A mention copied out of a message body, reduced to the id inside it.
fn unwrap_markup(token: &str) -> &str {
    let Some(inner) = token.strip_prefix('<').and_then(|t| t.strip_suffix('>')) else {
        return token;
    };
    // Everything after `|` is the label Slack renders, not the id.
    let inner = inner.split('|').next().unwrap_or(inner);
    let inner = inner.strip_prefix("!subteam^").unwrap_or(inner);
    let inner = inner.strip_prefix('!').unwrap_or(inner);
    inner.strip_prefix('@').unwrap_or(inner)
}

/// Slack ids are uppercase alphanumeric and at least 9 long. The floor is what
/// separates an id from a shouted handle like `@SRE`, which would otherwise
/// render as a ping that silently reaches nobody.
pub(super) fn id_like(s: &str) -> bool {
    (9..=24).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// One token as Slack markup; `None` where Slack would render inert text.
fn token_markup(token: &str) -> Option<String> {
    let t = unwrap_markup(token);
    let t = t.strip_prefix('@').unwrap_or(t);
    if t.eq_ignore_ascii_case("here") {
        Some("<!here>".into())
    } else if t.eq_ignore_ascii_case("channel") {
        Some("<!channel>".into())
    } else if t.starts_with('S') && id_like(t) {
        Some(format!("<!subteam^{t}>"))
    } else if (t.starts_with('U') || t.starts_with('W')) && id_like(t) {
        Some(format!("<@{t}>"))
    } else {
        None
    }
}

impl TransportConfig for SlackConfig {
    const KIND: ChannelKind = ChannelKind::Slack;

    fn redact_in_place(&mut self) {
        self.webhook_url = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.webhook_url == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.webhook_url);
        self.mention = cleared_when_empty(self.mention.take());
    }

    fn validate(&self) -> Result<(), String> {
        require_provider_webhook(
            &self.webhook_url,
            "Slack",
            &["hooks.slack.com", "hooks.slack-gov.com"],
            None,
        )?;
        validate_ping(self.mention.as_deref())
    }

    fn quiet_broadcast_mention(&mut self) {
        self.mention = without_broadcast_ping(self.mention.as_deref());
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.webhook_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mention: Option<&str>) -> SlackConfig {
        SlackConfig {
            webhook_url: "https://hooks.slack.com/services/T/B/x".into(),
            mention: mention.map(str::to_string),
        }
    }

    fn quieted(mut c: SlackConfig) -> SlackConfig {
        c.quiet_broadcast_mention();
        c
    }

    #[test]
    fn every_accepted_mention_form_renders_as_a_real_ping() {
        assert_eq!(cfg(Some("@here")).mention_markup().unwrap(), "<!here>");
        assert_eq!(cfg(Some("channel")).mention_markup().unwrap(), "<!channel>");
        assert_eq!(
            cfg(Some("S01ABC234")).mention_markup().unwrap(),
            "<!subteam^S01ABC234>"
        );
        assert_eq!(
            cfg(Some("U01ABC234")).mention_markup().unwrap(),
            "<@U01ABC234>"
        );
        assert_eq!(
            cfg(Some("@sre, S01ABC234")).mention_markup(),
            Some("<!subteam^S01ABC234>".into()),
            "an unpingable handle drops out instead of printing as dead text"
        );
    }

    #[test]
    fn a_handle_without_its_id_is_rejected_because_slack_would_not_ping_it() {
        assert!(cfg(Some("@sre")).validate().is_err());
        // Shouted, so it looks like an id until Slack renders it as dead text.
        assert!(cfg(Some("@SRE")).validate().is_err());
        assert!(cfg(Some("@DBTEAM")).validate().is_err());
        // A pasted mention carries its id, so it is accepted like Discord's.
        assert!(cfg(Some("<!subteam^S01ABC234>")).validate().is_ok());
        assert!(cfg(Some("<!subteam^S01ABC234|@sre>")).validate().is_ok());
        assert!(cfg(Some("<@U01ABC234>")).validate().is_ok());
        // Wrapped, but still a handle with no id behind it.
        assert!(cfg(Some("<@sre>")).validate().is_err());
        assert!(cfg(Some("  ")).validate().is_err());
        assert!(
            cfg(Some("here channel S01ABC234 U01ABC234 U05XYZ678 U09QRS012"))
                .validate()
                .is_err()
        );
        assert!(cfg(Some("@here S01ABC234")).validate().is_ok());
        assert!(cfg(None).validate().is_ok());
    }

    /// Slack copies a group id wrapped, so every form has to land on the same
    /// markup or one ping is stored and printed twice.
    #[test]
    fn a_pasted_mention_folds_onto_the_bare_id() {
        let markup = |m: &str| cfg(Some(m)).mention_markup().unwrap();
        assert_eq!(markup("<!subteam^S01ABC234>"), "<!subteam^S01ABC234>");
        assert_eq!(markup("<!subteam^S01ABC234|@sre>"), "<!subteam^S01ABC234>");
        assert_eq!(markup("<@U01ABC234>"), "<@U01ABC234>");
        assert_eq!(markup("<!here>"), "<!here>");
        assert_eq!(
            markup("S01ABC234 <!subteam^S01ABC234|@sre>"),
            "<!subteam^S01ABC234>"
        );
        // A pasted broadcast is still a broadcast a test send must drop.
        assert_eq!(quieted(cfg(Some("<!channel>"))).mention, None);
    }

    #[test]
    fn a_test_send_keeps_the_group_ping_and_drops_the_broadcast() {
        let quiet = quieted(cfg(Some("@here S01ABC234, channel")));
        assert_eq!(quiet.mention_markup().unwrap(), "<!subteam^S01ABC234>");
        assert_eq!(quieted(cfg(Some("@channel"))).mention, None);
        assert_eq!(quieted(cfg(None)).mention, None);
    }

    #[test]
    fn a_mention_survives_redaction_because_it_is_not_a_secret() {
        let mut c = cfg(Some("@here"));
        c.redact_in_place();
        assert_eq!(c.mention.as_deref(), Some("@here"));
        assert!(c.has_redaction_sentinel());
    }
}
