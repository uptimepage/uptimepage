//! Microsoft provider half of the OAuth login dance: authorize-URL builder and
//! Phase B (code exchange + id_token claim read into a [`RemoteIdentity`]).
//! Phase A/C live in [`crate::auth::oauth_login`].
//!
//! Entra's `email` claim is a directory attribute a tenant admin can set to an
//! address they do not own. Trusting it would hand that admin the Phase C
//! email-recovery path, so it passes [`email_is_attested`] first.

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
use crate::config::MicrosoftOauthConfig;
use crate::error::{AppError, Result};
use crate::http_outbound::OutboundHttpClient;

const MS_LOGIN_HOST: &str = "https://login.microsoftonline.com";

/// The single tenant every personal Microsoft account lives in.
const MSA_TENANT_ID: &str = "9188040d-6c67-4c5b-b112-36a304b66dad";

/// Personal accounts never carry `xms_edov`, so a Microsoft-run domain is the
/// only attestation left. Country variants (`hotmail.co.uk`) stay off the list:
/// matching them needs a prefix rule `hotmail.attacker.test` also satisfies.
const MS_OWNED_DOMAINS: &[&str] = &[
    "outlook.com",
    "hotmail.com",
    "live.com",
    "msn.com",
    "passport.com",
    "windowslive.com",
];

/// Fallback when `[auth.microsoft]` is partially set in TOML — a nested
/// `#[serde(default)]` section resets unlisted fields, emptying `scopes`.
const DEFAULT_SCOPES: &[&str] = &["openid", "email", "profile"];

#[derive(Debug, Deserialize)]
struct IdClaims {
    /// Stable per tenant — paired with `tid`, since a guest gets another one.
    oid: Option<String>,
    /// Pairwise per (app, user); the fallback when `oid`/`tid` are absent.
    sub: Option<String>,
    tid: Option<String>,
    email: Option<String>,
    preferred_username: Option<String>,
    name: Option<String>,
    /// Email Domain Owner Verified — the optional claim saying the tenant
    /// proved it owns the `email` claim's domain.
    #[serde(default, deserialize_with = "de_bool_loose")]
    xms_edov: Option<bool>,
}

fn domain_of(email: &str) -> Option<String> {
    let (_, domain) = email.rsplit_once('@')?;
    (!domain.is_empty()).then(|| domain.to_ascii_lowercase())
}

fn email_is_attested(claims: &IdClaims, email: &str) -> bool {
    if claims.xms_edov == Some(true) {
        return true;
    }
    claims.tid.as_deref() == Some(MSA_TENANT_ID)
        && domain_of(email).is_some_and(|d| MS_OWNED_DOMAINS.contains(&d.as_str()))
}

fn map_claims(claims: IdClaims) -> Result<RemoteIdentity> {
    let provider_user_id = match (claims.tid.as_deref(), claims.oid.as_deref()) {
        (Some(tid), Some(oid)) => format!("{tid}/{oid}"),
        _ => claims.sub.clone().ok_or_else(|| {
            AppError::Other(anyhow::anyhow!("microsoft id_token: no oid/tid and no sub"))
        })?,
    };
    // `xms_edov` attests the `email` claim's domain, so promoting a UPN on the
    // strength of it would vouch for a string Microsoft never saw.
    let verified_email = claims
        .email
        .as_deref()
        .filter(|e| email_is_attested(&claims, e))
        .map(str::to_string);
    if claims.email.is_some() && verified_email.is_none() {
        tracing::warn!(
            "microsoft id_token: email present but unattested — add the xms_edov optional claim \
             to the app registration or work-account sign-ups cannot link an address"
        );
    }
    Ok(RemoteIdentity {
        provider_user_id,
        provider_username: claims.email.or(claims.preferred_username),
        verified_email,
        display_name: claims.name,
    })
}

pub fn authorize_url(cfg: &MicrosoftOauthConfig, state: &str) -> String {
    let scope = if cfg.client.scopes.is_empty() {
        DEFAULT_SCOPES.join(" ")
    } else {
        cfg.client.scopes.join(" ")
    };
    format!(
        "{MS_LOGIN_HOST}/{tn}/oauth2/v2.0/authorize?response_type=code&client_id={cid}&state={st}&scope={sc}&redirect_uri={ru}",
        tn = cfg.tenant,
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
    cfg: &MicrosoftOauthConfig,
    code: &str,
) -> Result<RemoteIdentity> {
    let payload = crate::auth::url::form_body(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", &cfg.client.client_id),
        ("client_secret", cfg.client.client_secret.expose_secret()),
        ("redirect_uri", &cfg.client.redirect_url),
    ]);
    let url = format!("{MS_LOGIN_HOST}/{}/oauth2/v2.0/token", cfg.tenant);
    let req = Request::post(&url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, UA)
        .body(Full::new(Bytes::from(payload)))
        .map_err(|e| AppError::Other(anyhow::anyhow!("microsoft token request: {e}")))?;
    let body = fetch_limited(http, req, "microsoft token").await?;
    let id_token = parse_id_token(&body, "microsoft token endpoint")?;
    map_claims(decode_id_token_claims(&id_token, "microsoft id_token")?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OauthClientConfig;

    fn cfg(tenant: &str) -> MicrosoftOauthConfig {
        MicrosoftOauthConfig {
            client: OauthClientConfig {
                client_id: "cid".into(),
                redirect_url: "https://app.example.test/auth/microsoft/callback".into(),
                scopes: vec![],
                ..Default::default()
            },
            tenant: tenant.into(),
        }
    }

    fn claims_from(json: &str) -> Result<RemoteIdentity> {
        map_claims(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn authorize_url_has_code_response_type_and_encoded_scopes() {
        let url = authorize_url(&cfg("common"), "st&ate");
        assert!(url.starts_with(
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?response_type=code"
        ));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("state=st%26ate"));
        assert!(url.contains("scope=openid+email+profile"));
        assert!(
            url.contains(
                "redirect_uri=https%3A%2F%2Fapp.example.test%2Fauth%2Fmicrosoft%2Fcallback"
            )
        );
    }

    #[test]
    fn tenant_guid_addresses_one_tenant() {
        let url = authorize_url(&cfg("72f988bf-86f1-41af-91ab-2d7cd011db47"), "s");
        assert!(url.contains("/72f988bf-86f1-41af-91ab-2d7cd011db47/oauth2/v2.0/authorize"));
    }

    #[test]
    fn work_account_email_links_only_with_edov() {
        let id = claims_from(
            r#"{"oid":"o1","tid":"t1","email":"a@corp.test","xms_edov":true,"name":"A"}"#,
        )
        .unwrap();
        assert_eq!(id.verified_email.as_deref(), Some("a@corp.test"));
        assert_eq!(id.provider_user_id, "t1/o1");

        let id =
            claims_from(r#"{"oid":"o1","tid":"t1","email":"a@corp.test","name":"A"}"#).unwrap();
        assert_eq!(id.verified_email, None);
        assert_eq!(id.provider_username.as_deref(), Some("a@corp.test"));

        let id = claims_from(r#"{"oid":"o1","tid":"t1","email":"a@corp.test","xms_edov":"true"}"#)
            .unwrap();
        assert_eq!(id.verified_email.as_deref(), Some("a@corp.test"));
    }

    #[test]
    fn personal_account_links_only_on_a_microsoft_owned_domain() {
        let msa =
            |email: &str| format!(r#"{{"oid":"o1","tid":"{MSA_TENANT_ID}","email":"{email}"}}"#);
        let id = claims_from(&msa("a@Outlook.com")).unwrap();
        assert_eq!(id.verified_email.as_deref(), Some("a@Outlook.com"));

        // Only a Microsoft-run domain proves the holder reads that mailbox.
        let id = claims_from(&msa("ceo@victim.test")).unwrap();
        assert_eq!(id.verified_email, None);
    }

    #[test]
    fn an_unexpected_edov_shape_costs_the_link_not_the_login() {
        // A tenant emitting 1 instead of true must not take the parse with it.
        let id =
            claims_from(r#"{"oid":"o1","tid":"t1","email":"a@corp.test","xms_edov":1}"#).unwrap();
        assert_eq!(id.verified_email, None);
        assert_eq!(id.provider_user_id, "t1/o1");

        let id = claims_from(r#"{"oid":"o1","tid":"t1","email":"a@corp.test","xms_edov":null}"#)
            .unwrap();
        assert_eq!(id.verified_email, None);
    }

    #[test]
    fn upn_never_becomes_a_verified_address() {
        let id = claims_from(
            r#"{"oid":"o1","tid":"t1","preferred_username":"a@corp.test","xms_edov":true}"#,
        )
        .unwrap();
        assert_eq!(id.verified_email, None);
        assert_eq!(id.provider_username.as_deref(), Some("a@corp.test"));
    }

    #[test]
    fn sub_carries_the_identity_when_oid_or_tid_is_missing() {
        let id = claims_from(r#"{"sub":"s1","email":"a@corp.test"}"#).unwrap();
        assert_eq!(id.provider_user_id, "s1");

        assert!(claims_from(r#"{"email":"a@corp.test"}"#).is_err());
    }

    #[test]
    fn decode_claims_reads_an_unpadded_jwt_payload() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = URL_SAFE_NO_PAD.encode(br#"{"oid":"o1","tid":"t1","name":"A"}"#);
        let claims: IdClaims =
            decode_id_token_claims(&format!("header.{payload}.signature"), "t").unwrap();
        assert_eq!(claims.oid.as_deref(), Some("o1"));
        assert!(decode_id_token_claims::<IdClaims>("not-a-jwt", "t").is_err());
    }
}
