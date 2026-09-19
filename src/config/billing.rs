use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use super::{empty_secret, secret_str};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BillingConfig {
    /// "paddle", or "none" (the default): no checkout, no webhook receiver,
    /// no billing surface at all. Self-host installs stay on "none".
    pub provider: String,
    pub paddle: PaddleConfig,
}

impl BillingConfig {
    pub fn enabled(&self) -> bool {
        self.provider == "paddle"
    }
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            paddle: PaddleConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaddleEnvironment {
    Sandbox,
    Live,
}

impl PaddleEnvironment {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sandbox" => Some(Self::Sandbox),
            "live" => Some(Self::Live),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PaddleConfig {
    /// "sandbox" or "live". Keys are bound to one of the two.
    pub environment: String,
    #[serde(default = "empty_secret", with = "secret_str")]
    pub api_key: SecretString,
    /// The notification destination's secret key (`pdl_ntfset_…`).
    #[serde(default = "empty_secret", with = "secret_str")]
    pub webhook_secret: SecretString,
    /// Client-side token Paddle.js initialises with on the pay page. Public
    /// by design, it is embedded in HTML.
    pub client_token: String,
}

impl PaddleConfig {
    pub fn complete(&self) -> bool {
        !self.api_key.expose_secret().trim().is_empty()
            && !self.webhook_secret.expose_secret().trim().is_empty()
            && !self.client_token.trim().is_empty()
    }
}

impl Default for PaddleConfig {
    fn default() -> Self {
        Self {
            environment: "sandbox".into(),
            api_key: empty_secret(),
            webhook_secret: empty_secret(),
            client_token: String::new(),
        }
    }
}
