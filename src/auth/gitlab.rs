//! GitLab provider half of the OAuth login dance: authorize-URL builder and
//! Phase B (code exchange + id_token claim read into a [`RemoteIdentity`]).
//! Phase A/C live in [`crate::auth::oauth_login`].
//!
//! `sub` is unique only within the instance that issued it, so the identity
//! key carries the issuer, and `iss` is checked against the configured
//! `base_url` first: taken on trust, a self-managed instance could mint ids
//! that collide with gitlab.com's and land its users on someone else's
//! account.

use http_body_util::Full;
use hyper::Request;
use hyper::body::Bytes;
use hyper::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use secrecy::ExposeSecret;
use serde::Deserialize;

use crate::auth::oauth_login::{
    RemoteIdentity, UA, de_bool_loose, decode_id_token_claims, fetch_limited, parse_id_token,
};
use crate::auth::url::url_encode;
use crate::config::GitlabOauthConfig;
use crate::error::{AppError, Result};
use crate::http_outbound::OutboundHttpClient;

/// Fallback when `[auth.gitlab]` is partially set in TOML — a nested
/// `#[serde(default)]` section resets unlisted fields, emptying `scopes`.
const DEFAULT_SCOPES: &[&str] = &["openid", "email", "profile"];

#[derive(Debug, Deserialize)]
struct IdClaims {
    iss: Option<String>,
    /// The GitLab user id, unique only within the issuing instance.
    sub: Option<String>,
    email: Option<String>,
    /// A self-managed instance can let users sign up without confirming.
    #[serde(default, deserialize_with = "de_bool_loose")]
    email_verified: Option<bool>,
    preferred_username: Option<String>,
    nickname: Option<String>,
    name: Option<String>,
}

/// Both sides of the `iss` comparison go through this, so a `base_url` that
/// differs from GitLab's issuer only in case, port or trailing slash matches.
fn origin(base: &str) -> String {
    match url::Url::parse(base) {
        Ok(u) => format!(
            "{}{}",
            u.origin().ascii_serialization(),
            u.path().trim_end_matches('/')
        ),
        Err(_) => base.trim_end_matches('/').to_string(),
    }
}

fn map_claims(cfg: &GitlabOauthConfig, claims: IdClaims) -> Result<RemoteIdentity> {
    let iss = origin(claims.iss.as_deref().unwrap_or_default());
    if iss != origin(&cfg.base_url) {
        return Err(AppError::Other(anyhow::anyhow!(
            "gitlab id_token: issuer {iss:?} is not the configured instance"
        )));
    }
    let sub = claims
        .sub
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("gitlab id_token: no sub")))?;
    let provider_user_id = format!("{iss}/{sub}");

    let verified_email = claims
        .email
        .as_deref()
        .filter(|_| claims.email_verified == Some(true))
        .map(str::to_string);
    if claims.email.is_some() && verified_email.is_none() {
        tracing::warn!(
            "gitlab id_token: email present but unconfirmed on the instance — the address cannot \
             link an account until the user confirms it"
        );
    }
    Ok(RemoteIdentity {
        provider_user_id,
        provider_username: claims.preferred_username.or(claims.nickname),
        verified_email,
        display_name: claims.name,
    })
}

pub fn authorize_url(cfg: &GitlabOauthConfig, state: &str) -> String {
    let scope = if cfg.client.scopes.is_empty() {
        DEFAULT_SCOPES.join(" ")
    } else {
        cfg.client.scopes.join(" ")
    };
    format!(
        "{base}/oauth/authorize?response_type=code&client_id={cid}&state={st}&scope={sc}&redirect_uri={ru}",
        base = origin(&cfg.base_url),
        cid = url_encode(&cfg.client.client_id),
        st = url_encode(state),
        sc = url_encode(&scope),
        ru = url_encode(&cfg.client.redirect_url),
    )
}

/// Phase B — exchange the code, read the claims out of the id_token that comes
/// back with it. No DB connection held.
pub async fn fetch_identity(
    http: &OutboundHttpClient,
    cfg: &GitlabOauthConfig,
    code: &str,
) -> Result<RemoteIdentity> {
    let payload = crate::auth::url::form_body(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", &cfg.client.client_id),
        ("client_secret", cfg.client.client_secret.expose_secret()),
        ("redirect_uri", &cfg.client.redirect_url),
    ]);
    let url = format!("{}/oauth/token", origin(&cfg.base_url));
    let req = Request::post(&url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, UA)
        .body(Full::new(Bytes::from(payload)))
        .map_err(|e| AppError::Other(anyhow::anyhow!("gitlab token request: {e}")))?;
    let body = fetch_limited(http, req, "gitlab token").await?;
    let id_token = parse_id_token(&body, "gitlab token endpoint")?;
    map_claims(cfg, decode_id_token_claims(&id_token, "gitlab id_token")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OauthClientConfig;

    fn cfg(base_url: &str) -> GitlabOauthConfig {
        GitlabOauthConfig {
            client: OauthClientConfig {
                client_id: "cid".into(),
                redirect_url: "https://app.example.test/auth/gitlab/callback".into(),
                scopes: vec![],
                ..Default::default()
            },
            base_url: base_url.into(),
        }
    }

    fn claims_from(base_url: &str, json: &str) -> Result<RemoteIdentity> {
        map_claims(&cfg(base_url), serde_json::from_str(json).unwrap())
    }

    #[test]
    fn authorize_url_has_code_response_type_and_encoded_scopes() {
        let url = authorize_url(&cfg("https://gitlab.com"), "st&ate");
        assert!(url.starts_with("https://gitlab.com/oauth/authorize?response_type=code"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=st%26ate"));
        assert!(url.contains("scope=openid+email+profile"));
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fapp.example.test%2Fauth%2Fgitlab%2Fcallback")
        );
    }

    #[test]
    fn a_self_managed_instance_keeps_its_own_path() {
        let url = authorize_url(&cfg("https://git.corp.test/gitlab/"), "s");
        assert!(url.starts_with("https://git.corp.test/gitlab/oauth/authorize?"));
    }

    #[test]
    fn the_identity_key_carries_the_issuing_instance() {
        let id = claims_from(
            "https://gitlab.com",
            r#"{"iss":"https://gitlab.com","sub":"42","name":"A"}"#,
        )
        .unwrap();
        assert_eq!(id.provider_user_id, "https://gitlab.com/42");

        let id = claims_from(
            "https://git.corp.test",
            r#"{"iss":"https://git.corp.test","sub":"42"}"#,
        )
        .unwrap();
        assert_eq!(id.provider_user_id, "https://git.corp.test/42");
    }

    #[test]
    fn an_issuer_that_is_not_the_configured_instance_is_refused() {
        assert!(
            claims_from(
                "https://git.corp.test",
                r#"{"iss":"https://gitlab.com","sub":"42"}"#
            )
            .is_err()
        );
        assert!(claims_from("https://gitlab.com", r#"{"sub":"42"}"#).is_err());
    }

    /// Each passes `GitlabOauthConfig::base_url_is_valid`, so a byte comparison would let the
    /// boot succeed and then fail every callback.
    #[test]
    fn a_base_url_that_only_looks_different_still_matches() {
        for base in [
            "https://gitlab.com/",
            "https://GitLab.com",
            "https://gitlab.com:443",
        ] {
            let id = claims_from(base, r#"{"iss":"https://gitlab.com","sub":"42"}"#)
                .unwrap_or_else(|e| panic!("{base} should match the issuer: {e}"));
            assert_eq!(id.provider_user_id, "https://gitlab.com/42");
        }
    }

    #[test]
    fn an_unconfirmed_address_never_links_an_account() {
        let linked = |json: &str| claims_from("https://gitlab.com", json).unwrap();

        let id = linked(
            r#"{"iss":"https://gitlab.com","sub":"1","email":"a@corp.test","email_verified":true}"#,
        );
        assert_eq!(id.verified_email.as_deref(), Some("a@corp.test"));

        let id = linked(r#"{"iss":"https://gitlab.com","sub":"1","email":"a@corp.test"}"#);
        assert_eq!(id.verified_email, None);

        let id = linked(
            r#"{"iss":"https://gitlab.com","sub":"1","email":"a@corp.test","email_verified":false}"#,
        );
        assert_eq!(id.verified_email, None);

        let id = linked(
            r#"{"iss":"https://gitlab.com","sub":"1","email":"a@corp.test","email_verified":1}"#,
        );
        assert_eq!(id.verified_email, None);
        assert_eq!(id.provider_user_id, "https://gitlab.com/1");
    }

    #[test]
    fn the_username_falls_back_to_nickname() {
        let id = claims_from(
            "https://gitlab.com",
            r#"{"iss":"https://gitlab.com","sub":"1","nickname":"slim"}"#,
        )
        .unwrap();
        assert_eq!(id.provider_username.as_deref(), Some("slim"));

        let id = claims_from(
            "https://gitlab.com",
            r#"{"iss":"https://gitlab.com","sub":"1","preferred_username":"slim","nickname":"other"}"#,
        )
        .unwrap();
        assert_eq!(id.provider_username.as_deref(), Some("slim"));
    }

    #[test]
    fn an_empty_sub_is_not_an_identity() {
        assert!(
            claims_from(
                "https://gitlab.com",
                r#"{"iss":"https://gitlab.com","sub":""}"#
            )
            .is_err()
        );
    }
}
