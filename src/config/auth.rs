//! Sign-in, sessions, invitations, API tokens and first-run seeding.

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use super::{empty_secret, secret_str};
use crate::domain::OauthProvider;

/// Unattended first-run seeding, for app-store installs that have no terminal.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BootstrapConfig {
    /// Owner to seed when the instance has no users yet. Empty disables it.
    pub email: String,
    pub org_name: String,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            email: String::new(),
            org_name: "My Org".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AuthConfig {
    pub enabled_methods: Vec<String>,
    /// Off makes the deployment invite-only. Existing users still sign in and
    /// an invitation still bootstraps its recipient.
    pub open_signup: bool,
    pub fingerprint_salt: String,
    /// External base URL (scheme + host + optional port) used to build links
    /// the user sees in emails — invitation accept/decline, magic-link verify.
    /// Trailing slashes are tolerated. Required in production; dev defaults to
    /// `http://localhost:8080`.
    pub public_base_url: String,
    pub session: SessionConfig,
    pub github: OauthClientConfig,
    pub google: OauthClientConfig,
    pub microsoft: MicrosoftOauthConfig,
    pub gitlab: GitlabOauthConfig,
    pub invitations: InvitationsConfig,
    pub api_tokens: ApiTokensConfig,
    pub magic_link: MagicLinkConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled_methods: vec![
                "github_oauth".into(),
                "google_oauth".into(),
                "microsoft_oauth".into(),
                "gitlab_oauth".into(),
                "passkey".into(),
                "magic_link".into(),
            ],
            open_signup: true,
            fingerprint_salt: String::new(),
            public_base_url: "http://localhost:8080".into(),
            session: SessionConfig::default(),
            // Scopes empty: default.toml + provider DEFAULT_SCOPES own them.
            github: OauthClientConfig::default(),
            google: OauthClientConfig::default(),
            microsoft: MicrosoftOauthConfig::default(),
            gitlab: GitlabOauthConfig::default(),
            invitations: InvitationsConfig::default(),
            api_tokens: ApiTokensConfig::default(),
            magic_link: MagicLinkConfig::default(),
        }
    }
}

impl AuthConfig {
    /// List = policy switch; OAuth additionally needs creds (capability).
    pub fn method_enabled(&self, name: &str) -> bool {
        self.enabled_methods.iter().any(|m| m == name)
    }

    /// Single predicate for the magic-link surface — route mounting, the
    /// login-page form, and the token-purge ticker must agree.
    pub fn magic_link_enabled(&self) -> bool {
        self.method_enabled("magic_link")
    }

    /// Needs the magic-link surface too: a link is how a stranger proves the
    /// address is theirs.
    pub fn open_signup_enabled(&self) -> bool {
        self.open_signup && self.magic_link_enabled()
    }

    pub fn github_login_enabled(&self) -> bool {
        self.method_enabled("github_oauth") && self.github.is_configured()
    }

    pub fn google_login_enabled(&self) -> bool {
        self.method_enabled("google_oauth") && self.google.is_configured()
    }

    pub fn microsoft_login_enabled(&self) -> bool {
        self.method_enabled("microsoft_oauth") && self.microsoft.client.is_configured()
    }

    /// Providers this deployment will actually complete a sign-in for. One
    /// switched off or half-configured answers `/auth/{p}/login` with a 404,
    /// so its identities are rows in the table, not ways in.
    pub fn enabled_login_providers(&self) -> Vec<OauthProvider> {
        use OauthProvider as P;
        P::ALL
            .iter()
            .copied()
            .filter(|p| match p {
                P::Github => self.github_login_enabled(),
                P::Google => self.google_login_enabled(),
                P::Microsoft => self.microsoft_login_enabled(),
                P::Gitlab => self.gitlab_login_enabled(),
            })
            .collect()
    }

    pub fn gitlab_login_enabled(&self) -> bool {
        self.method_enabled("gitlab_oauth") && self.gitlab.client.is_configured()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionConfig {
    pub idle_timeout_days: u32,
    pub absolute_timeout_days: u32,
    pub cookie_name: String,
    pub cookie_secure: bool,
    pub cookie_domain: String,
    pub renew_on_use: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout_days: 30,
            absolute_timeout_days: 90,
            cookie_name: "_sm_session".into(),
            cookie_secure: true,
            cookie_domain: String::new(),
            renew_on_use: true,
        }
    }
}

/// One OAuth login provider's client credentials. A partially-written TOML
/// section resets `scopes` to `[]` via the nested `#[serde(default)]`; each
/// provider module falls back to its own default scopes for that case.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OauthClientConfig {
    pub client_id: String,
    #[serde(default = "empty_secret", with = "secret_str")]
    pub client_secret: SecretString,
    pub redirect_url: String,
    pub scopes: Vec<String>,
}

impl Default for OauthClientConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: empty_secret(),
            redirect_url: String::new(),
            scopes: Vec::new(),
        }
    }
}

impl OauthClientConfig {
    /// All three required — Google hard-rejects an empty redirect_uri.
    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty()
            && !self.client_secret.expose_secret().is_empty()
            && !self.redirect_url.is_empty()
    }
}

/// Microsoft's client credentials plus the tenant its endpoints are addressed
/// to. `common` admits work, school and personal accounts; `organizations`
/// drops personal ones; a tenant GUID or domain locks sign-in to one tenant.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MicrosoftOauthConfig {
    #[serde(flatten)]
    pub client: OauthClientConfig,
    pub tenant: String,
}

impl Default for MicrosoftOauthConfig {
    fn default() -> Self {
        Self {
            client: OauthClientConfig::default(),
            tenant: "common".into(),
        }
    }
}

impl MicrosoftOauthConfig {
    /// Tenant lands in a URL path: `common`, `organizations`, `consumers`, a
    /// GUID, or a domain, and never `.`/`..`. Checked at boot — a fallback
    /// would turn a mistyped single-tenant lock into `common` silently.
    pub fn tenant_is_valid(&self) -> bool {
        let tenant = self.tenant.as_str();
        !tenant.is_empty()
            && tenant != "."
            && tenant != ".."
            && tenant
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    }
}

/// The instance is the issuer half of the identity key, so changing it after
/// sign-ups orphans every identity minted under the old one.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GitlabOauthConfig {
    #[serde(flatten)]
    pub client: OauthClientConfig,
    pub base_url: String,
}

impl Default for GitlabOauthConfig {
    fn default() -> Self {
        Self {
            client: OauthClientConfig::default(),
            base_url: "https://gitlab.com".into(),
        }
    }
}

impl GitlabOauthConfig {
    /// https only — the client secret rides this origin in a POST body.
    pub fn base_url_is_valid(&self) -> bool {
        let Ok(u) = url::Url::parse(&self.base_url) else {
            return false;
        };
        u.scheme() == "https"
            && u.host_str().is_some_and(|h| !h.is_empty())
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct InvitationsConfig {
    pub expiry_hours: u32,
    // The pending-invitation cap moved to `plans.max_pending_invitations`
    // (one source of truth). A CI guard rejects re-reading the old key.
}

impl Default for InvitationsConfig {
    fn default() -> Self {
        Self { expiry_hours: 168 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ApiTokensConfig {
    // The per-user token cap moved to `plans.max_api_tokens_per_user` (one
    // source of truth). A CI guard rejects re-reading the old key.
    /// First N chars of every token surfaced in UI + used as a lookup-narrowing
    /// index. Single source of truth at INSERT and at lookup. Floor of 16 gives
    /// 48 bits of entropy in the prefix (collision-safe to ~16M tokens); a
    /// startup assertion refuses to boot below that.
    pub prefix_visible_chars: u32,
}

impl Default for ApiTokensConfig {
    fn default() -> Self {
        Self {
            prefix_visible_chars: 16,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MagicLinkConfig {
    pub expiry_minutes: u32,
    /// Per-email send throttle on `/auth/magic-link/request`: at most one
    /// real email per address per window, regardless of source IP. Enforced
    /// inside `tokio::spawn` so the response time stays anti-enum-safe.
    /// Set to `0` to disable the throttle.
    pub rate_limit_seconds: u32,
}

impl Default for MagicLinkConfig {
    fn default() -> Self {
        Self {
            expiry_minutes: 15,
            rate_limit_seconds: 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AuthConfig;

    #[test]
    fn open_signup_needs_a_link_to_arrive_by() {
        let mut cfg = AuthConfig::default();
        assert!(cfg.open_signup_enabled(), "both on by default");

        cfg.enabled_methods.retain(|m| m != "magic_link");
        assert!(!cfg.open_signup_enabled(), "no link, no signup");

        let cfg = AuthConfig {
            open_signup: false,
            ..AuthConfig::default()
        };
        assert!(!cfg.open_signup_enabled(), "policy still wins");
        assert!(
            cfg.magic_link_enabled(),
            "and existing users keep their way in"
        );
    }
}
