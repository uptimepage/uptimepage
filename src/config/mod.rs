//! Application configuration: the whole tree, how it loads, and what it
//! refuses to start with.
//!
//! Values come from `config/default.toml` (overridable via
//! `UPTIMEPAGE_CONFIG_PATH`) and then from `UPTIMEPAGE__`-prefixed environment
//! variables, which win. Sections live in their own file by domain, and the
//! startup validators in `validate`.

use std::path::PathBuf;

use config::{Config, Environment, File};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

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
    ConnectOauthConfig, ResendConfig, TelegramBotConfig, TransactionalEmailConfig,
    WhatsAppAppBotConfig,
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

const ENV_PREFIX: &str = "UPTIMEPAGE";
const ENV_SEPARATOR: &str = "__";
const DEFAULT_CONFIG_PATH: &str = "config/default.toml";
const CONFIG_PATH_ENV: &str = "UPTIMEPAGE_CONFIG_PATH";

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
    pub discord_oauth: ConnectOauthConfig,
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
            .add_source(
                Environment::with_prefix(ENV_PREFIX)
                    .prefix_separator("_")
                    .separator(ENV_SEPARATOR)
                    .try_parsing(true)
                    .list_separator(",")
                    .with_list_parse_key("dns.servers")
                    .with_list_parse_key("security.trusted_proxies"),
            );

        let cfg = builder.build()?;
        Ok(cfg.try_deserialize()?)
    }
}
