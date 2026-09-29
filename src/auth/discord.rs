//! "Add to Discord" connect dance. The `webhook.incoming` consent screen
//! carries Discord's server + channel picker, and the token exchange
//! returns a ready-made webhook owned by our app, whose id a press on one of
//! its alerts names. Everything else, the access and refresh tokens
//! included, is discarded, so no Discord credential is ever stored.

use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper::body::Bytes;
use hyper::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use secrecy::ExposeSecret;
use serde::Deserialize;

use crate::auth::url::url_encode;
use crate::config::ConnectOauthConfig;
use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, REQUEST_TIMEOUT};

const DISCORD_AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const DISCORD_TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const SCOPE: &str = "webhook.incoming";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const UA: &str = "uptimepage/discord-connect";

/// Webhook minted by the consent screen; the only part of the token
/// response that survives.
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingWebhook {
    pub id: String,
    pub url: String,
    /// Webhook name as shown in Discord; the consent screen lets the user
    /// rename it.
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    error: Option<String>,
    error_description: Option<String>,
    webhook: Option<IncomingWebhook>,
}

/// The state must already be persisted to `oauth_states` before this URL is
/// handed to the user.
pub fn authorize_url(cfg: &ConnectOauthConfig, redirect_uri: &str, state: &str) -> String {
    format!(
        "{DISCORD_AUTHORIZE_URL}?client_id={cid}&scope={sc}&response_type=code&state={st}&redirect_uri={ru}",
        cid = url_encode(&cfg.client_id),
        sc = url_encode(SCOPE),
        st = url_encode(state),
        ru = url_encode(redirect_uri),
    )
}

/// Exchange the callback `code` at the token endpoint and keep only the
/// webhook.
pub async fn exchange_code(
    http: &OutboundHttpClient,
    cfg: &ConnectOauthConfig,
    redirect_uri: &str,
    code: &str,
) -> Result<IncomingWebhook> {
    let payload = crate::auth::url::form_body(&[
        ("grant_type", "authorization_code"),
        ("client_id", &cfg.client_id),
        ("client_secret", cfg.client_secret.expose_secret()),
        ("code", code),
        ("redirect_uri", redirect_uri),
    ]);
    let req = Request::post(DISCORD_TOKEN_URL)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, UA)
        .body(Full::new(Bytes::from(payload)))
        .map_err(|e| AppError::Other(anyhow::anyhow!("discord token request: {e}")))?;
    let at = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    let resp = tokio::time::timeout_at(at, http.request(req))
        .await
        .map_err(|_| {
            AppError::Other(anyhow::anyhow!(
                "discord token endpoint: no response within {REQUEST_TIMEOUT:?}"
            ))
        })?
        .map_err(|e| AppError::Other(anyhow::anyhow!("discord request: {e}")))?;
    let status = resp.status();
    let body = tokio::time::timeout_at(
        at,
        Limited::new(resp.into_body(), MAX_RESPONSE_BYTES).collect(),
    )
    .await
    .map_err(|_| {
        AppError::Other(anyhow::anyhow!(
            "discord token endpoint: no body within {REQUEST_TIMEOUT:?}"
        ))
    })?
    .map_err(|e| AppError::Other(anyhow::anyhow!("discord body read: {e}")))?
    .to_bytes();
    let parsed: TokenResponse = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) if !status.is_success() => {
            return Err(AppError::Other(anyhow::anyhow!(
                "discord token endpoint: http {status}"
            )));
        }
        Err(e) => {
            return Err(AppError::Other(anyhow::anyhow!("discord token parse: {e}")));
        }
    };
    if let Some(err) = parsed.error {
        let desc = parsed.error_description.unwrap_or_default();
        return Err(AppError::Other(anyhow::anyhow!(
            "discord token endpoint: {err} ({desc})"
        )));
    }
    parsed.webhook.ok_or_else(|| {
        AppError::Other(anyhow::anyhow!(
            "discord token endpoint: ok response without webhook"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ConnectOauthConfig {
        ConnectOauthConfig {
            client_id: "1234567890".into(),
            ..Default::default()
        }
    }

    #[test]
    fn authorize_url_encodes_state_and_redirect() {
        let url = authorize_url(
            &cfg(),
            "https://app.example.test/auth/discord/callback",
            "a&b",
        );
        assert!(url.starts_with("https://discord.com/oauth2/authorize?"));
        assert!(url.contains("client_id=1234567890"));
        assert!(url.contains("scope=webhook.incoming"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("state=a%26b"));
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fapp.example.test%2Fauth%2Fdiscord%2Fcallback")
        );
    }

    #[test]
    fn token_response_keeps_webhook_and_ignores_tokens() {
        let parsed: TokenResponse = serde_json::from_str(
            r#"{"token_type":"Bearer","access_token":"secret","refresh_token":"secret2",
                "scope":"webhook.incoming","expires_in":604800,
                "webhook":{"id":"1","name":"alerts","channel_id":"2","guild_id":"3",
                           "url":"https://discord.com/api/webhooks/1/abc","token":"abc","type":1}}"#,
        )
        .unwrap();
        let wh = parsed.webhook.unwrap();
        assert_eq!(wh.id, "1");
        assert_eq!(wh.name.as_deref(), Some("alerts"));
        assert!(wh.url.starts_with("https://discord.com/api/webhooks/"));
    }

    #[test]
    fn token_response_surfaces_discord_error() {
        let parsed: TokenResponse = serde_json::from_str(
            r#"{"error":"invalid_grant","error_description":"Invalid authorization code"}"#,
        )
        .unwrap();
        assert_eq!(parsed.error.as_deref(), Some("invalid_grant"));
        assert!(parsed.webhook.is_none());
    }
}
