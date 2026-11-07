//! Accounts a member linked in an app whose own acknowledge control reports
//! who pressed it, so that member is named on the acknowledgement.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::UserId;
use super::actor::LinkedApp;

/// Who an app says pressed, held only as a keyed hash of the app's own id for
/// them: enough to match and to tell two people apart, never to name anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalId(pub [u8; 32]);

impl ExternalId {
    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// Who an app account is to one org.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linked {
    /// A live member of the org linked it.
    Member(UserId),
    /// Linked by someone the org does not count as a member.
    Outsider,
    /// Nobody linked it.
    Unlinked,
    /// The lookup failed, so nothing is known about it.
    Unknown,
}

impl Linked {
    pub fn member(self) -> Option<UserId> {
        match self {
            Self::Member(user) => Some(user),
            Self::Outsider | Self::Unlinked | Self::Unknown => None,
        }
    }

    /// Whoever holds the account is worth telling how to link it only when
    /// it is known that nobody has.
    pub fn invites_link(self) -> bool {
        self == Self::Unlinked
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
    fn only_an_account_nobody_linked_is_invited_to_link() {
        let user = UserId(Uuid::now_v7());
        assert_eq!(Linked::Member(user).member(), Some(user));
        assert!(!Linked::Member(user).invites_link());
        assert!(Linked::Unlinked.invites_link());
        for nobody in [Linked::Outsider, Linked::Unlinked, Linked::Unknown] {
            assert_eq!(nobody.member(), None);
        }
        assert!(!Linked::Outsider.invites_link());
        assert!(!Linked::Unknown.invites_link());
    }
}
