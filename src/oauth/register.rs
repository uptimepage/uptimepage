//! Dynamic Client Registration (RFC 7591). Open + unauthenticated, as the MCP
//! connector flow requires; Caddy applies the per-IP backstop. Registers a
//! public client (PKCE, no secret) with an exact-match redirect-URI allow-list.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;

use super::error::{OAuthError, OAuthErrorResponse};
use super::{is_acceptable_redirect_uri, store};

const MAX_REDIRECT_URIS: usize = 10;
const MAX_URI_LEN: usize = 2048;
const MAX_NAME_LEN: usize = 200;

#[derive(Debug, Deserialize)]
pub struct RegistrationRequest {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
    // Other RFC 7591 metadata (grant_types, token_endpoint_auth_method, …) is
    // accepted but ignored: this server fixes them to authorization_code + PKCE
    // public-client, and echoes those fixed values back.
}

fn generate_client_id() -> String {
    let mut bytes = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for client id");
    format!("ump_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegistrationRequest>,
) -> Result<(StatusCode, Json<Value>), OAuthErrorResponse> {
    let pool = state
        .db
        .as_ref()
        .ok_or_else(|| OAuthError::ServerError.with("storage unavailable"))?;

    if req.redirect_uris.is_empty() || req.redirect_uris.len() > MAX_REDIRECT_URIS {
        return Err(OAuthError::InvalidRedirectUri
            .with("redirect_uris must contain between 1 and 10 entries"));
    }
    for uri in &req.redirect_uris {
        if uri.len() > MAX_URI_LEN || !is_acceptable_redirect_uri(uri) {
            // The scheme alone says which client is knocking; the URI would
            // carry its user id.
            let scheme = uri.split_once(':').map(|(s, _)| s).unwrap_or("");
            tracing::info!(target: "oauth", scheme, "client registration refused a redirect_uri");
            return Err(OAuthError::InvalidRedirectUri.with(
                "each redirect_uri must be https, http on a loopback host, or a native app \
                 scheme (reverse-DNS form, or cursor://)",
            ));
        }
    }
    let client_name = req
        .client_name
        .map(|n| n.chars().take(MAX_NAME_LEN).collect::<String>());

    let client_id = generate_client_id();
    store::insert_client(pool, &client_id, client_name.as_deref(), &req.redirect_uris)
        .await
        .map_err(|e| {
            tracing::warn!(target: "oauth", error = %e, "client registration failed");
            OAuthError::ServerError.with("could not register client")
        })?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "client_id": client_id,
            "client_name": client_name,
            "redirect_uris": req.redirect_uris,
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    ))
}
