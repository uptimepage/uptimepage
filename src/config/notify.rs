//! Outbound channels the operator owns: bot credentials and the transactional mailer.

use ed25519_dalek::VerifyingKey;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use super::{empty_secret, secret_str};

/// `[slack_oauth]` / `[discord_oauth]`. Credentials of an operator-owned
/// OAuth app behind a one-click connect button: the dance hands back a
/// ready-made webhook URL so the user never copies one by hand. Empty
/// credentials hide the button; the manual-paste kind works either way.
/// Env only, never a config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ConnectOauthConfig {
    pub client_id: String,
    #[serde(default = "empty_secret", with = "secret_str")]
    pub client_secret: SecretString,
}

impl Default for ConnectOauthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: empty_secret(),
        }
    }
}

impl ConnectOauthConfig {
    pub fn enabled(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.expose_secret().trim().is_empty()
    }
}

/// `[slack_interactivity]`. Signing secret of the operator Slack app behind
/// `[slack_oauth]`, which signs every button press Slack posts to
/// `/hooks/slack/interactions`. Empty leaves the receiver unmounted, and
/// channels connected through the app fall back to the acknowledge page.
/// Env only, never a config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SlackInteractivityConfig {
    #[serde(default = "empty_secret", with = "secret_str")]
    pub signing_secret: SecretString,
}

impl Default for SlackInteractivityConfig {
    fn default() -> Self {
        Self {
            signing_secret: empty_secret(),
        }
    }
}

impl SlackInteractivityConfig {
    pub fn enabled(&self) -> bool {
        !self.signing_secret.expose_secret().trim().is_empty()
    }
}

/// `[discord_interactions]`. Public key of the operator Discord app behind
/// `[discord_oauth]`, which checks the signature on every interaction Discord
/// posts to `/hooks/discord/interactions`. Empty leaves the receiver
/// unmounted, and channels connected through the app fall back to the
/// acknowledge page. Anything but the 64 hex characters Discord shows on the
/// app's General Information page fails startup: a wrong key would refuse
/// every press as unsigned. Not a secret.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DiscordInteractionsConfig {
    #[serde(with = "discord_public_key")]
    pub public_key: Option<VerifyingKey>,
}

impl DiscordInteractionsConfig {
    pub fn enabled(&self) -> bool {
        self.public_key.is_some()
    }
}

mod discord_public_key {
    use ed25519_dalek::VerifyingKey;
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(key: &Option<VerifyingKey>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&key.map(|k| hex::encode(k.to_bytes())).unwrap_or_default())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<VerifyingKey>, D::Error> {
        let raw = String::deserialize(d)?;
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        hex::decode(raw)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .and_then(|b| VerifyingKey::from_bytes(&b).ok())
            .map(Some)
            .ok_or_else(|| {
                D::Error::custom(
                    "discord_interactions.public_key must be the 64 hex characters of the \
                     Discord app's Public Key",
                )
            })
    }
}

/// `[telegram]`. Operator-owned central bot shared by every org: customers
/// link a chat by tapping a deep link instead of running their own BotFather
/// bot. A non-empty `bot_token` enables the whole surface — the connect
/// button, the `/hooks/telegram` receiver, and the boot webhook handshake.
/// Empty leaves it absent; the bring-your-own `telegram` channel is
/// unaffected either way. All three values are capabilities — env only,
/// never a config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TelegramBotConfig {
    #[serde(default = "empty_secret", with = "secret_str")]
    pub bot_token: SecretString,
    /// Verified against `getMe` at boot so a mismatched deep link can't be
    /// minted against the wrong bot.
    pub bot_username: String,
    /// Echoed back by Telegram in `X-Telegram-Bot-Api-Secret-Token` on every
    /// update; the only thing authenticating the receiver.
    #[serde(default = "empty_secret", with = "secret_str")]
    pub webhook_secret: SecretString,
}

impl Default for TelegramBotConfig {
    fn default() -> Self {
        Self {
            bot_token: empty_secret(),
            bot_username: String::new(),
            webhook_secret: empty_secret(),
        }
    }
}

impl TelegramBotConfig {
    pub fn enabled(&self) -> bool {
        !self.bot_token.expose_secret().trim().is_empty()
    }

    /// Bot token for linked-channel delivery; `None` when not configured.
    pub fn delivery_token(&self) -> Option<&str> {
        self.enabled().then(|| self.bot_token.expose_secret())
    }
}

/// Operator-owned WhatsApp business number (Meta Cloud API) behind the
/// one-tap `whatsapp_app` channels. `enabled` is a deliberate spend gate:
/// template sends ride the operator's WABA at per-message Meta pricing, so
/// creds alone never switch the surface on.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct WhatsAppAppBotConfig {
    pub enabled: bool,
    #[serde(default = "empty_secret", with = "secret_str")]
    pub access_token: SecretString,
    /// Cloud API id messages are sent from.
    pub phone_number_id: String,
    /// Display number in international digits — the `wa.me` deep-link
    /// target (NOT the phone_number_id).
    pub public_number: String,
    /// Meta app secret; signs every webhook delivery
    /// (`X-Hub-Signature-256`).
    #[serde(default = "empty_secret", with = "secret_str")]
    pub app_secret: SecretString,
    /// Echoed by Meta's one-time GET subscribe handshake.
    #[serde(default = "empty_secret", with = "secret_str")]
    pub verify_token: SecretString,
    /// Approved alert template with a single body parameter.
    pub template_name: String,
    pub language_code: String,
}

impl Default for WhatsAppAppBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            access_token: empty_secret(),
            phone_number_id: String::new(),
            public_number: String::new(),
            app_secret: empty_secret(),
            verify_token: empty_secret(),
            template_name: String::new(),
            language_code: "en".into(),
        }
    }
}

impl WhatsAppAppBotConfig {
    pub fn enabled(&self) -> bool {
        self.enabled
            && !self.access_token.expose_secret().trim().is_empty()
            && !self.phone_number_id.trim().is_empty()
            && !self.app_secret.expose_secret().trim().is_empty()
            && !self.verify_token.expose_secret().trim().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmailProvider {
    /// HTTP API; the only one that leaves the machine.
    Resend,
    /// Renders to tracing and drops the mail; the dev default.
    Log,
    /// Buffers every send in process so a test can read it back; refused at boot.
    Memory,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TransactionalEmailConfig {
    pub provider: EmailProvider,
    pub from_name: String,
    pub from_address: String,
    /// Empty leaves the help form, its route and its nav entry absent, so a
    /// self-host install never mails a vendor.
    pub support_address: String,
    pub resend: ResendConfig,
}

impl TransactionalEmailConfig {
    /// Whether a send goes anywhere a caller can observe. `memory` counts:
    /// the test harness reads it back.
    pub fn delivers(&self) -> bool {
        self.provider != EmailProvider::Log
    }

    pub fn support_enabled(&self) -> bool {
        !self.support_address.trim().is_empty()
    }
}

impl Default for TransactionalEmailConfig {
    fn default() -> Self {
        Self {
            provider: EmailProvider::Log,
            from_name: "Uptimepage".into(),
            from_address: String::new(),
            support_address: String::new(),
            resend: ResendConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ResendConfig {
    #[serde(default = "empty_secret", with = "secret_str")]
    pub api_key: SecretString,
    /// Svix signing secret (`whsec_…`) of the Resend webhook endpoint.
    /// Empty = the `/hooks/resend` receiver is absent and bounce events
    /// are not consumed.
    #[serde(default = "empty_secret", with = "secret_str")]
    pub webhook_secret: SecretString,
}

impl ResendConfig {
    pub fn webhook_enabled(&self) -> bool {
        !self.webhook_secret.expose_secret().trim().is_empty()
    }
}

impl Default for ResendConfig {
    fn default() -> Self {
        Self {
            api_key: empty_secret(),
            webhook_secret: empty_secret(),
        }
    }
}
