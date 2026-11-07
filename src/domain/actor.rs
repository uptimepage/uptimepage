//! Who acted on an incident, and the apps whose own control can say so.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Who caused an `IncidentEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActorType {
    System,
    User,
    Mcp,
    /// A person acting through a signed link. Possession of the notification
    /// is the whole proof, so there is no user to name.
    Link,
    /// Pressed in Telegram. Names the member who linked that Telegram account,
    /// and nobody when it is not linked.
    Telegram,
    /// Acknowledged in Pushover, named the same way as [`Self::Telegram`].
    Pushover,
    /// Pressed in Slack, named the same way as [`Self::Telegram`].
    Slack,
    /// Pressed in Discord, named the same way as [`Self::Telegram`].
    Discord,
}

impl ActorType {
    pub const ALL: &'static [Self] = &[
        Self::System,
        Self::User,
        Self::Mcp,
        Self::Link,
        Self::Telegram,
        Self::Pushover,
        Self::Slack,
        Self::Discord,
    ];
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Mcp => "mcp",
            Self::Link => "link",
            Self::Telegram => "telegram",
            Self::Pushover => "pushover",
            Self::Slack => "slack",
            Self::Discord => "discord",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "user" => Self::User,
            "mcp" => Self::Mcp,
            "link" => Self::Link,
            "telegram" => Self::Telegram,
            "pushover" => Self::Pushover,
            "slack" => Self::Slack,
            "discord" => Self::Discord,
            _ => Self::System,
        }
    }
    /// The app whose own control this actor pressed, which names a member
    /// only through an account they linked there.
    pub const fn linked_app(self) -> Option<LinkedApp> {
        match self {
            Self::Telegram => Some(LinkedApp::Telegram),
            Self::Pushover => Some(LinkedApp::Pushover),
            Self::Slack => Some(LinkedApp::Slack),
            Self::Discord => Some(LinkedApp::Discord),
            Self::System | Self::User | Self::Mcp | Self::Link => None,
        }
    }

    /// Where a member acted when it was not the web app itself.
    pub fn via(self) -> Option<&'static str> {
        match self {
            Self::Mcp => Some("MCP"),
            other => other.linked_app().map(LinkedApp::label),
        }
    }
}

/// Mirrors the `linked_app_accounts.app` CHECK; the enum-drift test ties the
/// two together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkedApp {
    Telegram,
    Pushover,
    Slack,
    Discord,
}

impl LinkedApp {
    pub const ALL: &'static [Self] = &[Self::Telegram, Self::Pushover, Self::Slack, Self::Discord];

    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Pushover => "pushover",
            Self::Slack => "slack",
            Self::Discord => "discord",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|a| a.as_db_str() == s)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Pushover => "Pushover",
            Self::Slack => "Slack",
            Self::Discord => "Discord",
        }
    }

    pub const fn actor_type(self) -> ActorType {
        match self {
            Self::Telegram => ActorType::Telegram,
            Self::Pushover => ActorType::Pushover,
            Self::Slack => ActorType::Slack,
            Self::Discord => ActorType::Discord,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_names_round_trip() {
        for a in ActorType::ALL {
            assert_eq!(ActorType::from_db_str(a.as_db_str()), *a);
        }
        for app in LinkedApp::ALL {
            assert_eq!(LinkedApp::from_db_str(app.as_db_str()), Some(*app));
        }
        assert_eq!(LinkedApp::from_db_str("teams"), None);
    }
}
