//! Application configuration: the whole tree, how it loads, and what it
//! refuses to start with.
//!
//! Values come from `config/default.toml` (overridable via
//! `UPTIMEPAGE_CONFIG_PATH`) and then from `UPTIMEPAGE_`-prefixed environment
//! variables with `__` between nested keys, which win. Sections live in their own file by domain, and the
//! startup validators in `validate`.

use std::path::PathBuf;

use config::{Config, Environment, File};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::domain::LinkedApp;
use crate::error::Result;

mod auth;
mod billing;
mod boot;
mod limits;
mod notify;
mod observability;
mod ops;
mod public;
mod runtime;
mod storage;
#[cfg(test)]
mod tests;
mod validate;

pub use auth::{
    ApiTokensConfig, AuthConfig, BootstrapConfig, GitlabOauthConfig, InvitationsConfig,
    MagicLinkConfig, MicrosoftOauthConfig, OauthClientConfig, SessionConfig,
};
pub use billing::{BillingConfig, PaddleConfig, PaddleEnvironment};
pub use limits::{
    AbuseConfig, ApiConfig, CorsConfig, EmailPolicyConfig, PerIpRateLimits, QuotasConfig,
    RateLimitJanitorConfig, RateLimitsConfig, SignupPolicy,
};
pub use notify::{
    ConnectOauthConfig, DiscordInteractionsConfig, EmailProvider, ResendConfig,
    SlackInteractivityConfig, TelegramBotConfig, TransactionalEmailConfig, WhatsAppAppBotConfig,
};
pub use observability::{GrafanaConfig, HeartbeatConfig, LogFormat, ObservabilityConfig};
pub use ops::{AgentConfig, EscalationConfig, FlowConfig, McpConfig, OperatorConfig};
pub use public::{MarketingConfig, PublicStatusConfig, RetentionConfig, TenancyConfig};
pub use runtime::{
    CheckerConfig, CircuitBreakerConfig, DnsConfig, HttpClientConfig, RuntimeConfig,
    SchedulerConfig, SecurityConfig, ServerConfig,
};
pub use storage::{ClickhouseConfig, PostgresConfig, StorageConfig};

/// Default for a secret-bearing config field: an empty secret. Used by
/// `#[serde(default = "empty_secret")]` so a missing key deserialises to an
/// empty value rather than failing.
pub(crate) fn empty_secret() -> SecretString {
    SecretString::from(String::new())
}

/// (De)serialisation for `SecretString` config fields. `secrecy` deliberately
/// gives `SecretString` no `Serialize`, so `AppConfig`'s derive needs this:
/// it reads a plain string in and writes a fixed placeholder out, ensuring a
/// serialised config can never carry a real secret.
pub(crate) mod secret_str {
    use secrecy::SecretString;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(_v: &SecretString, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("[redacted]")
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SecretString, D::Error> {
        Ok(SecretString::from(String::deserialize(d)?))
    }
}

/// A list field read from a TOML array or, from the environment, from one
/// comma-separated string. Either way items are trimmed and blanks dropped.
pub(crate) mod comma_list {
    use std::fmt;
    use std::marker::PhantomData;
    use std::str::FromStr;

    use serde::de::{Deserializer, Error, SeqAccess, Visitor};

    pub fn deserialize<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: fmt::Display,
    {
        d.deserialize_any(List(PhantomData))
    }

    fn parse<'a, T, E>(items: impl Iterator<Item = &'a str>) -> Result<Vec<T>, E>
    where
        T: FromStr,
        T::Err: fmt::Display,
        E: Error,
    {
        items
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| {
                item.parse()
                    .map_err(|e| E::custom(format!("{item:?}: {e}")))
            })
            .collect()
    }

    struct List<T>(PhantomData<T>);

    impl<'de, T> Visitor<'de> for List<T>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a list or a comma-separated string")
        }

        fn visit_str<E: Error>(self, s: &str) -> Result<Vec<T>, E> {
            parse(s.split(','))
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                items.push(item);
            }
            parse(items.iter().map(String::as_str))
        }
    }
}

const ENV_PREFIX: &str = "UPTIMEPAGE";
const ENV_SEPARATOR: &str = "__";
const DEFAULT_CONFIG_PATH: &str = "config/default.toml";
const CONFIG_PATH_ENV: &str = "UPTIMEPAGE_CONFIG_PATH";

/// Env values reach serde as the strings they are. `config` still turns them
/// into bools and numbers for fields it reads itself, though not for fields
/// under `#[serde(flatten)]`, which see the raw string. Parsing up front would
/// turn a numeric-looking id such as `2923044121.11870682879441` into an f64
/// and hand back a rounded one.
fn env_source() -> Environment {
    Environment::with_prefix(ENV_PREFIX)
        .prefix_separator("_")
        .separator(ENV_SEPARATOR)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub runtime: RuntimeConfig,
    pub checker: CheckerConfig,
    pub http_client: HttpClientConfig,
    pub dns: DnsConfig,
    pub security: SecurityConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub storage: StorageConfig,
    pub scheduler: SchedulerConfig,
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub tenancy: TenancyConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub public_status: PublicStatusConfig,
    #[serde(default)]
    pub email: TransactionalEmailConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub quotas: QuotasConfig,
    #[serde(default)]
    pub rate_limits: RateLimitsConfig,
    #[serde(default)]
    pub abuse: AbuseConfig,
    #[serde(default)]
    pub email_policy: EmailPolicyConfig,
    #[serde(default)]
    pub marketing: MarketingConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub escalation: EscalationConfig,
    #[serde(default)]
    pub agent: AgentConfig,

    #[serde(default)]
    pub flow: FlowConfig,
    #[serde(default)]
    pub operator: OperatorConfig,
    #[serde(default)]
    pub telegram: TelegramBotConfig,
    #[serde(default)]
    pub whatsapp_app: WhatsAppAppBotConfig,
    #[serde(default)]
    pub slack_oauth: ConnectOauthConfig,
    #[serde(default)]
    pub slack_interactivity: SlackInteractivityConfig,
    #[serde(default)]
    pub discord_oauth: ConnectOauthConfig,
    #[serde(default)]
    pub discord_interactions: DiscordInteractionsConfig,
    #[serde(default)]
    pub bootstrap: BootstrapConfig,
    #[serde(default)]
    pub billing: BillingConfig,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let primary = std::env::var(CONFIG_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH));

        let builder = Config::builder()
            .add_source(File::from(primary).required(false))
            .add_source(env_source());

        let cfg = builder.build()?;
        Ok(cfg.try_deserialize()?)
    }

    /// Whether the org may add on-call coverage.
    ///
    /// Self-host is exempt for the same reason it is exempt from the SMS gate: the
    /// operator owns the `plans` row, so gating them against it means nothing.
    /// The plan's caps still decide how many of each it may keep.
    pub fn on_call_available(&self, plan: &crate::domain::Plan) -> bool {
        !self.marketing.enabled || plan.on_call_enabled
    }

    /// The apps whose button presses reach this deployment.
    pub fn pressed_apps(&self) -> Vec<LinkedApp> {
        LinkedApp::ALL
            .iter()
            .copied()
            .filter(|app| self.receives_presses(*app))
            .collect()
    }

    /// Whether presses on `app`'s own buttons reach this deployment. Pushover
    /// reports its acknowledgements on a receipt we poll instead.
    pub fn receives_presses(&self, app: LinkedApp) -> bool {
        match app {
            LinkedApp::Telegram => self.telegram.enabled(),
            LinkedApp::Slack => self.slack_interactivity.enabled(),
            LinkedApp::Discord => self.discord_interactions.enabled(),
            LinkedApp::Pushover => false,
        }
    }
}
