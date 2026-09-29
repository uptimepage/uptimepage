use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::mention::{cleared_when_empty, tokens, validate as validate_mention, without_broadcast};
use super::transport::{MASK, TransportConfig, require_https, trim_in_place};

/// The server's rule for usernames and custom groups alike. The signup
/// form's tighter 3-22 letter-first shape is the UI's, not the API's.
const NAME_LEN: std::ops::RangeInclusive<usize> = 1..=64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MattermostConfig {
    /// Incoming-webhook URL. Secret in full: the path carries the key.
    pub webhook_url: String,
    /// Who to ping: `@channel`, `@here`, `@all`, or a username or group name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention: Option<String>,
}

impl MattermostConfig {
    /// Mattermost resolves a bare `@name`, so the markup is the token itself.
    pub fn mention_markup(&self) -> Option<String> {
        let mut markup: Vec<String> = Vec::new();
        for token in tokens(self.mention.as_deref()?).filter(|t| pingable(t)) {
            let at = format!("@{}", bare(token));
            if !markup.contains(&at) {
                markup.push(at);
            }
        }
        (!markup.is_empty()).then(|| markup.join(" "))
    }
}

fn bare(token: &str) -> &str {
    token.strip_prefix('@').unwrap_or(token)
}

fn is_broadcast(token: &str) -> bool {
    matches!(bare(token), "channel" | "here" | "all")
}

/// The handle is resolved at delivery, so only its shape is checked here.
fn pingable(token: &str) -> bool {
    let t = bare(token);
    is_broadcast(token)
        || (NAME_LEN.contains(&t.len())
            && t.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_')
            }))
}

/// Separates a webhook URL from the `/api/v4/…` one the same server serves.
/// Unanchored, so a reverse-proxy subpath install passes.
fn require_hook_path(webhook_url: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(webhook_url).map_err(|_| "webhook_url is not a valid URL".to_string())?;
    let key = parsed
        .path()
        .rsplit_once("/hooks/")
        .map(|(_, key)| key.trim_end_matches('/'));
    match key {
        Some(k) if !k.is_empty() && !k.contains('/') => Ok(()),
        _ => Err(
            "this doesn't look like a Mattermost incoming webhook URL — it ends in /hooks/<key>"
                .into(),
        ),
    }
}

impl TransportConfig for MattermostConfig {
    const KIND: ChannelKind = ChannelKind::Mattermost;

    fn redact_in_place(&mut self) {
        self.webhook_url = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.webhook_url == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.webhook_url);
        // Handles are lowercase; a pasted `@Bob` would reach nobody.
        self.mention = cleared_when_empty(self.mention.take()).map(|m| m.to_lowercase());
    }

    fn validate(&self) -> Result<(), String> {
        require_https(&self.webhook_url, "webhook_url")?;
        require_hook_path(&self.webhook_url)?;
        validate_mention(self.mention.as_deref(), pingable, |t| {
            format!(
                "Mattermost cannot ping \"{t}\" — use @channel, @here, @all, or a username or \
                 group name (up to 64 of a-z, 0-9, dot, dash, underscore)"
            )
        })
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.webhook_url)
    }

    fn quiet_broadcast_mention(&mut self) {
        self.mention = without_broadcast(self.mention.as_deref(), is_broadcast);
    }
}
