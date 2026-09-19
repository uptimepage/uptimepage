//! OAuth providers wired through the auth flow. The variant set is the
//! single source of truth: the Postgres CHECK on `oauth_identities.provider`
//! is the closed list of [`OauthProvider::ALL`]; `oauth_states.provider`
//! additionally accepts [`CONNECT_PROVIDERS`]. Both validated by
//! `tests/enum_drift_test.rs`.

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
