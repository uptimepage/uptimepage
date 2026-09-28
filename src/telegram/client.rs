//! Thin Bot API client over the shared outbound HTTP path, for the central
//! bot's own calls outside paging. The bot token lives in the request path, so
//! every error is scrubbed of the token-bearing URL before it propagates.

use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, get_json, post_json};

/// Stand-in for the real `api.telegram.org/bot<token>` base in error messages.
const REDACTED_BASE: &str = "https://api.telegram.org/bot<redacted>";

/// `{ "ok": true, "result": … }` envelope every Bot API method returns.
#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotIdentity {
    #[serde(default)]
    pub username: Option<String>,
}

pub struct TelegramClient {
    http: OutboundHttpClient,
    base: String,
}

impl TelegramClient {
    pub fn new(http: OutboundHttpClient, bot_token: &str) -> Self {
        Self {
            http,
            base: format!("https://api.telegram.org/bot{bot_token}"),
        }
    }

    fn endpoint(&self, method: &str) -> Result<Url> {
        format!("{}/{method}", self.base)
            .parse()
            .map_err(|_| AppError::Other(anyhow::anyhow!("telegram bot_token is not URL-safe")))
    }

    /// Replace the token-bearing base in any diagnostic so a timeout/transport
    /// error (which embeds the request URL) can't write the token to logs.
    fn scrub(&self, err: AppError) -> AppError {
        AppError::Other(anyhow::anyhow!(
            "{}",
            err.to_string().replace(&self.base, REDACTED_BASE)
        ))
    }

    fn unwrap_envelope<T>(resp: ApiResponse<T>, method: &str) -> Result<T> {
        if !resp.ok {
            let desc = resp.description.unwrap_or_default();
            return Err(AppError::Other(anyhow::anyhow!(
                "telegram {method} failed: {desc}"
            )));
        }
        resp.result
            .ok_or_else(|| AppError::Other(anyhow::anyhow!("telegram {method} returned no result")))
    }

    pub async fn get_me(&self) -> Result<BotIdentity> {
        let url = self.endpoint("getMe")?;
        let resp: ApiResponse<BotIdentity> = get_json(&self.http, &url)
            .await
            .map_err(|e| self.scrub(e))?;
        Self::unwrap_envelope(resp, "getMe")
    }

    /// Plain-text message to a chat.
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<()> {
        let url = self.endpoint("sendMessage")?;
        post_json(
            &self.http,
            &url,
            &json!({ "chat_id": chat_id, "text": text }),
        )
        .await
        .map_err(|e| self.scrub(e))
    }

    /// Reply to one message, landing as a plain message if it is gone.
    pub async fn send_reply(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let url = self.endpoint("sendMessage")?;
        post_json(
            &self.http,
            &url,
            &json!({
                "chat_id": chat_id,
                "text": text,
                "reply_parameters": {
                    "message_id": message_id,
                    "allow_sending_without_reply": true,
                },
            }),
        )
        .await
        .map_err(|e| self.scrub(e))
    }

    /// Stop the button's spinner and show the presser a short notice that
    /// only they see.
    pub async fn answer_callback_query(&self, query_id: &str, text: &str) -> Result<()> {
        let url = self.endpoint("answerCallbackQuery")?;
        post_json(
            &self.http,
            &url,
            &json!({ "callback_query_id": query_id, "text": text }),
        )
        .await
        .map_err(|e| self.scrub(e))
    }

    /// Leave a group after its last linked channel is deleted.
    pub async fn leave_chat(&self, chat_id: i64) -> Result<()> {
        let url = self.endpoint("leaveChat")?;
        post_json(&self.http, &url, &json!({ "chat_id": chat_id }))
            .await
            .map_err(|e| self.scrub(e))
    }

    /// Telegram returns a non-2xx on any setWebhook error, so the HTTP status
    /// (surfaced by [`post_json`]) is the success signal.
    pub async fn set_webhook(
        &self,
        webhook_url: &str,
        secret_token: &str,
        allowed_updates: &[&str],
    ) -> Result<()> {
        let url = self.endpoint("setWebhook")?;
        post_json(
            &self.http,
            &url,
            &json!({
                "url": webhook_url,
                "secret_token": secret_token,
                "allowed_updates": allowed_updates,
            }),
        )
        .await
        .map_err(|e| self.scrub(e))
    }
}
