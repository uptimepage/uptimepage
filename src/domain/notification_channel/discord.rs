use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::mention::{cleared_when_empty, tokens, validate as validate_mention, without_broadcast};
use super::transport::{MASK, TransportConfig, require_provider_webhook, trim_in_place};

/// Discord snowflakes are 17 to 20 digits; shorter is a typo, not an id.
const ID_DIGITS: std::ops::RangeInclusive<usize> = 17..=20;

pub(super) fn snowflake(s: &str) -> bool {
    ID_DIGITS.contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiscordConfig {
    /// Channel webhook URL. The path carries the webhook token, so the
    /// whole value is treated as a secret.
    pub webhook_url: String,
    /// Who to ping on an alert: `@everyone`, `@here`, a role id (`&123…`) or a
    /// member id (`123…`), space or comma separated. Visible routing, not a
    /// secret, so it survives redaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
}

/// Both halves of a Discord ping: the markup it renders and the allow-list it
/// enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordMention {
    pub markup: String,
    pub everyone: bool,
    pub roles: Vec<String>,
    pub users: Vec<String>,
}

impl DiscordConfig {
    /// A typo drops out alone rather than costing the on-call ping beside it.
    pub fn mention_targets(&self) -> Option<DiscordMention> {
        mention_targets(self.mention.as_deref())
    }
}

/// Both halves of the ping `mention` asks for, `None` when it names nobody.
pub(super) fn mention_targets(mention: Option<&str>) -> Option<DiscordMention> {
    let mut targets: Vec<Mention> = Vec::new();
    for target in tokens(mention?).filter_map(parse_token) {
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    (!targets.is_empty()).then(|| DiscordMention {
        markup: targets
            .iter()
            .map(Mention::markup)
            .collect::<Vec<_>>()
            .join(" "),
        everyone: targets.iter().any(Mention::pings_everyone),
        roles: targets.iter().filter_map(Mention::role).collect(),
        users: targets.iter().filter_map(Mention::user).collect(),
    })
}

pub(super) fn validate_ping(mention: Option<&str>) -> Result<(), String> {
    validate_mention(
        mention,
        |t| parse_token(t).is_some(),
        |t| {
            format!(
                "Discord cannot ping \"{t}\" — use @everyone, @here, a role id (&123…) \
                 or a member id (123…), which you copy from Discord with developer mode on"
            )
        },
    )
}

pub(super) fn require_discord_webhook(url: &str) -> Result<(), String> {
    require_provider_webhook(
        url,
        "Discord",
        &["discord.com", "discordapp.com"],
        Some("/api/webhooks/"),
    )
}

/// The ping without `@everyone` or `@here`, for a test send.
pub(super) fn without_broadcast_ping(mention: Option<&str>) -> Option<String> {
    without_broadcast(mention, is_broadcast)
}

#[derive(PartialEq, Eq)]
enum Mention {
    Everyone,
    Here,
    Role(String),
    User(String),
}

impl Mention {
    fn markup(&self) -> String {
        match self {
            Self::Everyone => "@everyone".into(),
            Self::Here => "@here".into(),
            Self::Role(id) => format!("<@&{id}>"),
            Self::User(id) => format!("<@{id}>"),
        }
    }

    fn pings_everyone(&self) -> bool {
        matches!(self, Self::Everyone | Self::Here)
    }

    fn role(&self) -> Option<String> {
        match self {
            Self::Role(id) => Some(id.clone()),
            _ => None,
        }
    }

    fn user(&self) -> Option<String> {
        match self {
            Self::User(id) => Some(id.clone()),
            _ => None,
        }
    }
}

fn is_broadcast(token: &str) -> bool {
    parse_token(token).is_some_and(|m| m.pings_everyone())
}

/// `None` for anything Discord renders as inert text, such as a handle typed
/// without its id.
fn parse_token(token: &str) -> Option<Mention> {
    // Both what "Copy ID" yields and what a paste of a message body carries.
    let t = token
        .strip_prefix("<@&")
        .and_then(|t| t.strip_suffix('>'))
        .map(|id| format!("&{id}"))
        .or_else(|| {
            token
                .strip_prefix("<@")
                .and_then(|t| t.strip_suffix('>'))
                // Discord still renders the older nickname form `<@!id>`.
                .map(|t| t.strip_prefix('!').unwrap_or(t).to_string())
        })
        .unwrap_or_else(|| token.to_string());
    let t = t.strip_prefix('@').unwrap_or(&t);
    if t.eq_ignore_ascii_case("everyone") {
        Some(Mention::Everyone)
    } else if t.eq_ignore_ascii_case("here") {
        Some(Mention::Here)
    } else if let Some(id) = t.strip_prefix('&').filter(|id| snowflake(id)) {
        Some(Mention::Role(id.to_string()))
    } else if snowflake(t) {
        Some(Mention::User(t.to_string()))
    } else {
        None
    }
}

impl TransportConfig for DiscordConfig {
    const KIND: ChannelKind = ChannelKind::Discord;

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
        require_discord_webhook(&self.webhook_url)?;
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

    fn cfg(mention: Option<&str>) -> DiscordConfig {
        DiscordConfig {
            webhook_url: "https://discord.com/api/webhooks/123/tok".into(),
            mention: mention.map(str::to_string),
        }
    }

    fn quieted(mut c: DiscordConfig) -> DiscordConfig {
        c.quiet_broadcast_mention();
        c
    }

    #[test]
    fn every_accepted_mention_form_renders_as_a_real_ping() {
        let markup = |m: &str| cfg(Some(m)).mention_targets().unwrap().markup;
        assert_eq!(markup("@everyone"), "@everyone");
        assert_eq!(markup("here"), "@here");
        assert_eq!(markup("&123456789012345678"), "<@&123456789012345678>");
        assert_eq!(markup("123456789012345678"), "<@123456789012345678>");
        assert_eq!(markup("<@&123456789012345678>"), "<@&123456789012345678>");
        assert_eq!(markup("<@123456789012345678>"), "<@123456789012345678>");
        assert_eq!(markup("<@!123456789012345678>"), "<@123456789012345678>");
    }

    #[test]
    fn the_allow_list_names_only_the_configured_targets() {
        let t = cfg(Some("@here &123456789012345678, 987654321098765432"))
            .mention_targets()
            .unwrap();
        assert!(t.everyone, "@here is allowed through the everyone parse");
        assert_eq!(t.roles, ["123456789012345678"]);
        assert_eq!(t.users, ["987654321098765432"]);

        let quiet = cfg(Some("&123456789012345678")).mention_targets().unwrap();
        assert!(!quiet.everyone);
    }

    #[test]
    fn a_handle_without_its_id_is_rejected_because_discord_would_not_ping_it() {
        assert!(cfg(Some("@sre")).validate().is_err());
        assert!(cfg(Some("@ops-team")).validate().is_err());
        assert!(cfg(Some("12345")).validate().is_err());
        assert!(cfg(Some("&12345")).validate().is_err());
        assert!(cfg(Some("  ")).validate().is_err());
        assert!(
            cfg(Some(
                "everyone here &123456789012345678 987654321098765432 \
                 123456789012345679 123456789012345670"
            ))
            .validate()
            .is_err()
        );
        assert!(cfg(Some("@here &123456789012345678")).validate().is_ok());
        assert!(cfg(None).validate().is_ok());
    }

    #[test]
    fn a_repeated_target_is_folded_once() {
        let t = cfg(Some("&123456789012345678 &123456789012345678 @here here"))
            .mention_targets()
            .unwrap();
        assert_eq!(t.markup, "<@&123456789012345678> @here");
        assert_eq!(t.roles, ["123456789012345678"]);
    }

    #[test]
    fn an_unpingable_token_drops_out_instead_of_taking_the_rest() {
        let t = cfg(Some("@sre &123456789012345678"))
            .mention_targets()
            .unwrap();
        assert_eq!(t.markup, "<@&123456789012345678>");
    }

    #[test]
    fn a_test_send_keeps_the_role_ping_and_drops_the_broadcast() {
        let quiet = quieted(cfg(Some("@here &123456789012345678, everyone")));
        assert_eq!(
            quiet.mention_targets().unwrap().markup,
            "<@&123456789012345678>"
        );
        assert_eq!(quieted(cfg(Some("@everyone"))).mention, None);
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
