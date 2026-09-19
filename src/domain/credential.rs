//! OAuth providers wired through the auth flow. The variant set is the
//! single source of truth: the Postgres CHECK on `oauth_identities.provider`
//! is the closed list of [`OauthProvider::ALL`]; `oauth_states.provider`
//! additionally accepts [`CONNECT_PROVIDERS`]. Both validated by
//! `tests/enum_drift_test.rs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OauthProvider {
    Github,
    Google,
    Microsoft,
    Gitlab,
}

impl OauthProvider {
    /// Every variant in declaration order. Used by the enum-drift integration
    /// test to compare against the live Postgres CHECK constraint; keep in
    /// lockstep with the enum body.
    pub const ALL: &'static [Self] = &[Self::Github, Self::Google, Self::Microsoft, Self::Gitlab];

    /// Stable string used in the Postgres CHECK constraints and bound as the
    /// `provider` parameter at every INSERT / WHERE site. Routing through this
    /// method means adding a new provider requires updating the enum, which
    /// the drift test then ties to a matching migration.
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Google => "google",
            Self::Microsoft => "microsoft",
            Self::Gitlab => "gitlab",
        }
    }

    /// Display name, cased the way each vendor writes it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Google => "Google",
            Self::Microsoft => "Microsoft",
            Self::Gitlab => "GitLab",
        }
    }

    pub fn from_db_str(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.as_db_str() == s)
    }
}

/// What happened to a credential. Closed list: the `credential_events.action`
/// CHECK is [`CredentialAction::ALL`], tied by `tests/enum_drift_test.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAction {
    Linked,
    Unlinked,
}

impl CredentialAction {
    pub const ALL: &'static [Self] = &[Self::Linked, Self::Unlinked];

    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Linked => "linked",
            Self::Unlinked => "unlinked",
        }
    }
}

/// How a credential change came about. Closed list: the
/// `credential_events.origin` CHECK is [`CredentialOrigin::ALL`], tied by
/// `tests/enum_drift_test.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialOrigin {
    /// The credential the account was created with.
    Signup,
    /// Linked on an attested address, without anyone asking for it.
    EmailMatch,
    Session,
}

impl CredentialOrigin {
    pub const ALL: &'static [Self] = &[Self::Signup, Self::EmailMatch, Self::Session];

    pub const fn as_db_str(self) -> &'static str {
        match self {
            Self::Signup => "signup",
            Self::EmailMatch => "email_match",
            Self::Session => "session",
        }
    }
}

/// Connect-purpose OAuth dances (channel attach, not login): allowed in
/// `oauth_states.provider` on top of [`OauthProvider::ALL`], never in
/// `oauth_identities.provider` — they produce no identity row.
pub const SLACK_CONNECT_PROVIDER: &str = "slack_connect";

pub const DISCORD_CONNECT_PROVIDER: &str = "discord_connect";

pub const CONNECT_PROVIDERS: &[&str] = &[SLACK_CONNECT_PROVIDER, DISCORD_CONNECT_PROVIDER];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LinkedIdentity {
    pub provider: String,
    pub provider_user_id: String,
    pub provider_username: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login_at: DateTime<Utc>,
}

/// What still opens the account. `magic_link` counts only where the mail can
/// actually be delivered — the default sender writes to the log. A linked
/// provider counts only where this deployment will complete a sign-in for it;
/// one switched off answers `/auth/{p}/login` with a 404.
#[derive(Debug, Clone)]
pub struct WaysIn {
    pub enabled_providers: Vec<OauthProvider>,
    pub email_is_a_way_back: bool,
    pub passkeys_open_the_account: bool,
}

impl WaysIn {
    /// `passkeys` is how many usable credentials would survive the removal,
    /// counted apart from the slugs because no vendor can switch one off.
    pub fn reachable_with<'a>(
        &self,
        mut remaining: impl Iterator<Item = &'a str>,
        passkeys: usize,
    ) -> bool {
        self.email_is_a_way_back
            || (self.passkeys_open_the_account && passkeys > 0)
            || remaining.any(|slug| {
                OauthProvider::from_db_str(slug)
                    .is_some_and(|p| self.enabled_providers.contains(&p))
            })
    }

    /// Two vendors can mint the same subject, so a sibling is anything that is
    /// not this exact pair. Shared with the unlink guard so the button and the
    /// guard behind it cannot answer differently.
    pub fn removable(&self, row: &LinkedIdentity, all: &[LinkedIdentity], passkeys: usize) -> bool {
        self.reachable_with(
            all.iter()
                .filter(|o| {
                    o.provider != row.provider || o.provider_user_id != row.provider_user_id
                })
                .map(|o| o.provider.as_str()),
            passkeys,
        )
    }

    /// The same question asked from the other side: taking one passkey away
    /// leaves the linked providers plus whatever passkeys remain.
    pub fn passkey_removable(&self, all: &[LinkedIdentity], surviving_passkeys: usize) -> bool {
        self.reachable_with(all.iter().map(|o| o.provider.as_str()), surviving_passkeys)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn row(provider: &str, subject: &str) -> LinkedIdentity {
        LinkedIdentity {
            provider: provider.into(),
            provider_user_id: subject.into(),
            provider_username: None,
            created_at: Utc::now(),
            last_login_at: Utc::now(),
        }
    }

    fn ways_in(enabled: &[OauthProvider], email: bool) -> WaysIn {
        WaysIn {
            enabled_providers: enabled.to_vec(),
            email_is_a_way_back: email,
            passkeys_open_the_account: true,
        }
    }

    #[test]
    fn two_vendors_sharing_a_subject_are_told_apart() {
        // Excluding a sibling by subject alone drops both rows from "what is
        // left", and every method then reads as the only one.
        let all = vec![row("github", "12345"), row("google", "12345")];
        let w = ways_in(&[OauthProvider::Github, OauthProvider::Google], false);
        assert!(w.removable(&all[0], &all, 0));
        assert!(w.removable(&all[1], &all, 0));
    }

    #[test]
    fn the_only_method_stays_unless_email_is_a_way_back() {
        let all = vec![row("github", "1")];
        assert!(!ways_in(&[OauthProvider::Github], false).removable(&all[0], &all, 0));
        assert!(ways_in(&[OauthProvider::Github], true).removable(&all[0], &all, 0));
    }

    #[test]
    fn a_method_this_deployment_cannot_sign_in_with_is_not_a_way_in() {
        // GitHub is switched off, so counting it would let the account drop
        // the one method that still works.
        let all = vec![row("github", "1"), row("gitlab", "2")];
        let w = ways_in(&[OauthProvider::Gitlab], false);
        assert!(
            !w.removable(&all[1], &all, 0),
            "gitlab is all that opens it"
        );
        assert!(w.removable(&all[0], &all, 0), "github opens nothing anyway");
    }
    #[test]
    fn a_passkey_keeps_the_last_provider_removable() {
        // Without counting it the page hides the remove button on the only
        // provider, from someone who already has another way in.
        let all = vec![row("github", "1")];
        let w = ways_in(&[OauthProvider::Github], false);
        assert!(!w.removable(&all[0], &all, 0));
        assert!(w.removable(&all[0], &all, 1));
    }

    #[test]
    fn a_passkey_does_not_count_where_the_deployment_switched_them_off() {
        let all = vec![row("github", "1")];
        let mut w = ways_in(&[OauthProvider::Github], false);
        w.passkeys_open_the_account = false;
        assert!(!w.removable(&all[0], &all, 3), "none of them can sign in");
    }

    #[test]
    fn the_last_passkey_stays_unless_something_else_opens_the_account() {
        let none: Vec<LinkedIdentity> = Vec::new();
        let w = ways_in(&[], false);
        assert!(!w.passkey_removable(&none, 0), "nothing would be left");
        assert!(w.passkey_removable(&none, 1), "a sibling passkey remains");

        let linked = vec![row("github", "1")];
        let w = ways_in(&[OauthProvider::Github], false);
        assert!(w.passkey_removable(&linked, 0), "github still opens it");
    }
}
