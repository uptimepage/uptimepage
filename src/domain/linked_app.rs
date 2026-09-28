//! Apps whose own acknowledge control reports who pressed it, so a member who
//! linked their account there is named on the acknowledgement.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{ActorType, UserId};

/// Mirrors the `linked_app_accounts.app` CHECK; the enum-drift test ties the
/// two together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkedApp {
    Telegram,
    Pushover,
}

impl LinkedApp {
    pub const ALL: &'static [Self] = &[Self::Telegram, Self::Pushover];

    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Pushover => "pushover",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|a| a.as_db_str() == s)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Pushover => "Pushover",
        }
    }

    pub const fn actor_type(self) -> ActorType {
        match self {
            Self::Telegram => ActorType::Telegram,
            Self::Pushover => ActorType::Pushover,
        }
    }
}

/// Who an app says pressed, held only as a keyed hash of the app's own id for
/// them: enough to match and to tell two people apart, never to name anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalId(pub [u8; 32]);

impl ExternalId {
    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// One app account a person proved is theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedAppAccount {
    pub id: Uuid,
    pub user_id: UserId,
    pub app: LinkedApp,
    /// What the app calls them, as it was when they linked.
    pub label: Option<String>,
    pub linked_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_names_round_trip() {
        for app in LinkedApp::ALL {
            assert_eq!(LinkedApp::from_db_str(app.as_db_str()), Some(*app));
        }
        assert_eq!(LinkedApp::from_db_str("slack"), None);
    }
}
