//! The org members an on-call or escalation page offers, and the roster that
//! names the users a schedule resolves to.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{OrgId, UserId};
use crate::web::error::WebResult;

/// One org member offered as a participant / override coverer.
#[derive(Clone)]
pub struct MemberChoice {
    pub id: UserId,
    pub email: String,
    /// Has a channel that can deliver a page. Without one, a member on shift
    /// resolves as on call and nothing reaches them.
    pub reachable: bool,
}

/// Members by user id, to name the users a schedule resolves to.
pub struct Roster<'a> {
    by_id: HashMap<UserId, &'a MemberChoice>,
    /// Email local parts, lowercased, that more than one member has.
    shared: HashSet<String>,
}

impl<'a> Roster<'a> {
    pub fn new(members: &'a [MemberChoice]) -> Self {
        let mut seen = HashSet::new();
        Self {
            by_id: members.iter().map(|m| (m.id, m)).collect(),
            shared: members
                .iter()
                .map(|m| local_part(&m.email).to_lowercase())
                .filter(|local| !seen.insert(local.clone()))
                .collect(),
        }
    }

    pub fn get(&self, user: &UserId) -> Option<&'a MemberChoice> {
        self.by_id.get(user).copied()
    }

    /// Someone who left came off every schedule with their membership, so a
    /// miss is only the moment between the two reads.
    pub fn email(&self, user: &UserId) -> &'a str {
        self.get(user).map_or("former member", |m| m.email.as_str())
    }

    /// The email's local part, or the whole email when another member's
    /// reads the same in any case.
    pub fn short(&self, user: &UserId) -> &'a str {
        let email = self.email(user);
        let local = local_part(email);
        if self.shared.contains(&local.to_lowercase()) {
            email
        } else {
            local
        }
    }

    /// No page reaches someone no longer a member.
    pub fn reachable(&self, user: &UserId) -> bool {
        self.get(user).is_some_and(|m| m.reachable)
    }
}

fn local_part(email: &str) -> &str {
    email.split_once('@').map_or(email, |(local, _)| local)
}

/// Org members as builder choices. Empty without a DB (single-tenant dev).
pub(crate) async fn org_members(state: &AppState, org: OrgId) -> WebResult<Vec<MemberChoice>> {
    let Some(pool) = &state.db else {
        return Ok(vec![]);
    };
    // Only a warning rides on this, so a channel that fails to load leaves
    // everyone unflagged rather than taking the page down.
    let reachable = reachable_users(state, org)
        .await
        .inspect_err(
            |e| tracing::warn!(error = %e, org = %org.0, "on-call reachability unavailable"),
        )
        .ok();
    Ok(crate::storage::orgs::list_members(pool, org)
        .await?
        .into_iter()
        .map(|m| MemberChoice {
            reachable: reachable
                .as_ref()
                .is_none_or(|r| r.contains(&m.membership.user_id)),
            id: m.membership.user_id,
            email: m.email,
        })
        .collect())
}

/// Members with a channel that can deliver a page.
async fn reachable_users(state: &AppState, org: OrgId) -> crate::error::Result<HashSet<UserId>> {
    let delivering: HashSet<Uuid> = state
        .notification_channel_store
        .list(org)
        .await?
        .into_iter()
        .filter(|c| c.can_deliver())
        .map(|c| c.id)
        .collect();
    Ok(state
        .contact_store
        .for_org(org)
        .await?
        .into_iter()
        .filter(|(_, c)| delivering.contains(c))
        .map(|(u, _)| u)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: UserId, email: &str) -> MemberChoice {
        MemberChoice {
            id,
            email: email.into(),
            reachable: true,
        }
    }

    fn uid(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    #[test]
    fn short_names_keep_the_domain_when_two_members_share_a_local_part() {
        let members = vec![
            member(uid(1), "ops@acme.com"),
            member(uid(2), "ops@contractor.io"),
            member(uid(3), "olena@acme.com"),
        ];
        let roster = Roster::new(&members);
        assert_eq!(roster.short(&uid(1)), "ops@acme.com");
        assert_eq!(roster.short(&uid(2)), "ops@contractor.io");
        assert_eq!(roster.short(&uid(3)), "olena");
    }

    #[test]
    fn short_names_keep_the_domain_when_local_parts_differ_only_in_case() {
        let members = vec![
            member(uid(1), "Ops@acme.com"),
            member(uid(2), "ops@contractor.io"),
        ];
        let roster = Roster::new(&members);
        assert_eq!(roster.short(&uid(1)), "Ops@acme.com");
        assert_eq!(roster.short(&uid(2)), "ops@contractor.io");
    }
}
