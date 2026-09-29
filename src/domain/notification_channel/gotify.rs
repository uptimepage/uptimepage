use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::ChannelKind;
use super::transport::{MASK, TransportConfig, require_https, trim_in_place};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GotifyConfig {
    /// Base URL of the self-hosted server, subpath allowed (`/gotify` behind
    /// a reverse proxy). Customer-controlled, so it rides the abuse deny-list.
    pub server_url: String,
    /// Application token, sent as `X-Gotify-Key`. The application it belongs
    /// to is what the message shows up under.
    pub token: String,
}

impl GotifyConfig {
    /// Publish endpoint. The token picks the application, so the path never
    /// varies per channel.
    pub fn publish_url(&self) -> String {
        format!("{}/message", self.server_url.trim_end_matches('/'))
    }
}

impl TransportConfig for GotifyConfig {
    const KIND: ChannelKind = ChannelKind::Gotify;

    fn redact_in_place(&mut self) {
        self.token = MASK.to_string();
    }

    fn has_redaction_sentinel(&self) -> bool {
        self.token == MASK
    }

    fn normalize(&mut self) {
        trim_in_place(&mut self.server_url);
        // Only the trailing slash goes: the path is the install's own base
        // (`/gotify` behind a proxy), so nothing else may be stripped from it.
        let base = self.server_url.trim_end_matches('/');
        if base.len() != self.server_url.len() {
            self.server_url = base.to_string();
        }
        trim_in_place(&mut self.token);
    }

    fn validate(&self) -> Result<(), String> {
        require_https(&self.server_url, "server_url")?;
        let parsed = url::Url::parse(&self.server_url)
            .map_err(|_| "server_url is not a valid URL".to_string())?;
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err("server_url must be the server base URL, without query or fragment".into());
        }
        // server_url stays visible after redaction, so inline credentials
        // would be echoed back by the API.
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err("server_url must not embed credentials — use the token field".into());
        }
        // Token formats have changed across Gotify majors (`A…`, `gtfy…`), so
        // only the shape a header can carry is enforced.
        if self.token.is_empty()
            || self.token.len() > 200
            || self.token.chars().any(|c| c.is_ascii_whitespace())
        {
            return Err("token must be 1-200 characters without whitespace".into());
        }
        if self.token.chars().any(|c| !c.is_ascii_graphic()) {
            return Err("token must be printable ASCII".into());
        }
        Ok(())
    }

    fn abuse_url(&self) -> Option<&str> {
        Some(&self.server_url)
    }

    fn quiet_broadcast_mention(&mut self) {}
}
