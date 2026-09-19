//! Authorization endpoint: GET renders consent (after login), POST records the
//! user's decision and mints a single-use code.
//!
//! Trust ordering is load-bearing: `client_id` + `redirect_uri` are validated
//! against the registered allow-list **before** any error is allowed to redirect
//! to that URI. An unrecognised client or unregistered redirect_uri renders a
//! local error page — never a redirect to an attacker-supplied location.

use axum::Json;
use axum::extract::{OriginalUri, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use rand::TryRng;
use rand::rngs::SysRng;
use serde::Deserialize;
use serde_json::json;

use crate::app::AppState;
use crate::auth::sha256_hex;
use crate::domain::OrgId;
use crate::storage::orgs::{is_active_member, list_orgs_for_user};
use crate::web::auth::{Session, login_redirect};
// Brought into scope so the askama-generated template code can resolve the
// custom filters (`source_url`, `source_commit`, `version`) used by base.html.
use crate::web::filters;

use super::error::OAuthError;
use super::{
    ACCESS_LEVELS, CODE_TTL_SECS, DEFAULT_TOKEN_TTL_DAYS, MAX_TOKEN_TTL_DAYS, OAuthUrls,
    grant_scope, is_acceptable_redirect_uri, level_covering, redirect_destination, store,
};

/// Upper bound on the opaque `state` we round-trip — generous for real clients,
/// tight enough to keep the consent page from being inflated by a crafted value.
const MAX_STATE_LEN: usize = 1024;

#[derive(Debug, Deserialize)]
pub struct AuthorizeParams {
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

/// Also what a connector sees after the sweeper dropped the client_id it
/// cached, so it says how to recover.
const UNKNOWN_CLIENT: &str =
    "unknown or expired client_id: remove this connector from your client and add it again";

/// Minimal, safe error page for failures that must NOT redirect (untrusted
/// client/redirect_uri). Plain text, no reflection of attacker input.
fn error_page(msg: &'static str) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, msg).into_response()
}

/// Append `error`/`state` to a *validated* redirect_uri and 302 there.
fn redirect_error(redirect_uri: &str, error: OAuthError, state: Option<&str>) -> Response {
    redirect_with(redirect_uri, &[("error", error.code())], state)
}

fn redirect_with(redirect_uri: &str, pairs: &[(&str, &str)], state: Option<&str>) -> Response {
    // redirect_uri is already validated as a registered, well-formed URI.
    match url::Url::parse(redirect_uri) {
        Ok(mut url) => {
            {
                let mut q = url.query_pairs_mut();
                for (k, v) in pairs {
                    q.append_pair(k, v);
                }
                if let Some(s) = state {
                    q.append_pair("state", s);
                }
            }
            Redirect::to(url.as_str()).into_response()
        }
        Err(_) => error_page("invalid redirect_uri"),
    }
}

/// Human-readable description for a granted scope token.
fn scope_label(scope: &str) -> &'static str {
    match scope {
        "targets:read" => "Read your monitors and their current status",
        "status_page:read" => "Read your status pages and components",
        "incidents:read" => "Read your incidents and their updates",
        "channels:read" => "Read the names of your notification channels",
        "targets:write" => "Create monitors, and pause, resume or retune existing ones",
        "targets:execute" => "Run checks on your monitors on demand",
        "incidents:write" => "Post updates to your incidents (shown publicly)",
        "status_page:write" => "Create and edit your status pages, including their public address",
        "variables:read" => "Read the names of your variables, never their values",
        _ => "Access your data",
    }
}

/// A non-`:read` scope grants the ability to change something.
fn is_write_scope(scope: &str) -> bool {
    !scope.ends_with(":read")
}

pub async fn authorize_page(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    Query(p): Query<AuthorizeParams>,
    session: Session,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return error_page("storage unavailable");
    };
    let urls = OAuthUrls::from_cfg(&state.cfg);

    // 1) Validate client + redirect_uri BEFORE trusting redirect_uri.
    let client = match store::get_client(pool, &p.client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page(UNKNOWN_CLIENT),
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "get_client failed");
            return error_page("internal error");
        }
    };
    if !client.redirect_uris.iter().any(|u| u == &p.redirect_uri) {
        return error_page("redirect_uri is not registered for this client");
    }
    // From here, errors may redirect to the (validated) redirect_uri.

    // 2) Resource (RFC 8707). If supplied it must be ours; if omitted, bind ours.
    if let Some(req_resource) = p.resource.as_deref()
        && req_resource.trim_end_matches('/') != urls.resource
    {
        return redirect_error(
            &p.redirect_uri,
            OAuthError::InvalidTarget,
            p.state.as_deref(),
        );
    }

    // 3) Response type + PKCE.
    if p.response_type != "code" {
        return redirect_error(
            &p.redirect_uri,
            OAuthError::UnsupportedResponseType,
            p.state.as_deref(),
        );
    }
    if p.code_challenge_method != "S256" || !super::pkce::is_valid_challenge(&p.code_challenge) {
        return redirect_error(
            &p.redirect_uri,
            OAuthError::InvalidRequest,
            p.state.as_deref(),
        );
    }
    // Bound the opaque `state` we echo back + embed in the consent page.
    if p.state.as_deref().is_some_and(|s| s.len() > MAX_STATE_LEN) {
        return redirect_error(
            &p.redirect_uri,
            OAuthError::InvalidRequest,
            // Don't echo an over-long state back.
            None,
        );
    }

    let granted = grant_scope(p.scope.as_deref());

    // 4) Require an authenticated session; else send to login and back.
    let Some(user) = session.user_id() else {
        let next = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        return login_redirect(next).into_response();
    };
    let orgs = match list_orgs_for_user(pool, user).await {
        Ok(orgs) => orgs,
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "list_orgs_for_user failed");
            return error_page("internal error");
        }
    };
    if orgs.is_empty() {
        return error_page("create an organization before connecting");
    }
    let active = session.active_org_id.unwrap_or(orgs[0].org.id);
    let orgs: Vec<ConsentOrg> = orgs
        .into_iter()
        .map(|o| ConsentOrg {
            id: o.org.id.0.to_string(),
            name: o.org.name,
            selected: o.org.id == active,
        })
        .collect();
    if let Err(e) = store::touch_client(pool, &p.client_id).await {
        tracing::warn!(target: "oauth", error = %e, "touch_client failed");
    }

    let requested = level_covering(&granted);
    let levels: Vec<ConsentLevel> = ACCESS_LEVELS
        .iter()
        .map(|level| ConsentLevel {
            id: level.id,
            label: level.label,
            summary: level.summary,
            scope: level
                .scopes
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            grants: level
                .scopes
                .iter()
                .map(|s| ConsentScope {
                    label: scope_label(s.as_str()),
                    write: is_write_scope(s.as_str()),
                })
                .collect(),
            selected: level.id == requested.id,
        })
        .collect();

    ConsentPage {
        active_tab: "",
        client_name: client
            .client_name
            .unwrap_or_else(|| "An application".to_string()),
        requested_label: requested.label,
        requested_writes: requested.id != ACCESS_LEVELS[0].id,
        orgs,
        levels,
        destination: redirect_destination(&p.redirect_uri),
        client_id: p.client_id,
        redirect_uri: p.redirect_uri,
        code_challenge: p.code_challenge,
        state: p.state.unwrap_or_default(),
        resource: urls.resource,
        default_ttl_days: DEFAULT_TOKEN_TTL_DAYS,
    }
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct DecisionRequest {
    action: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    scope: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    expires_in_days: Option<u32>,
    #[serde(default)]
    org_id: Option<uuid::Uuid>,
}

pub async fn decision(
    State(state): State<AppState>,
    session: Session,
    Json(req): Json<DecisionRequest>,
) -> Response {
    let Some(pool) = state.db.as_ref() else {
        return error_page("storage unavailable");
    };
    let urls = OAuthUrls::from_cfg(&state.cfg);

    // Session is authoritative for identity; never trust the body for it.
    let Some(user) = session.user_id() else {
        return error_page("session expired; reload and try again");
    };
    let Some(org) = req.org_id.map(OrgId).or(session.active_org_id) else {
        return error_page("select an organization before connecting");
    };

    // Re-validate client + redirect_uri against the registry (don't trust the
    // posted hidden fields to redirect anywhere).
    let client = match store::get_client(pool, &req.client_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page(UNKNOWN_CLIENT),
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "get_client failed");
            return error_page("internal error");
        }
    };
    if !client.redirect_uris.iter().any(|u| u == &req.redirect_uri)
        || !is_acceptable_redirect_uri(&req.redirect_uri)
    {
        return error_page("redirect_uri is not registered for this client");
    }
    if req.resource.trim_end_matches('/') != urls.resource {
        return error_page("resource mismatch");
    }
    if !super::pkce::is_valid_challenge(&req.code_challenge) {
        return error_page("invalid code_challenge");
    }

    let state_opt = (!req.state.is_empty()).then_some(req.state.as_str());

    if req.action != "approve" {
        return Json(json!({
            "redirect": redirect_uri_string(
                &req.redirect_uri,
                &[("error", OAuthError::AccessDenied.code())],
                state_opt,
            ),
        }))
        .into_response();
    }

    // The org came from the form; membership is what makes it the user's to grant.
    match is_active_member(pool, user, org).await {
        Ok(true) => {}
        Ok(false) => return error_page("you are not a member of that organization"),
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "membership check failed");
            return error_page("internal error");
        }
    }

    // Re-clamp scope; user-chosen, bounded token lifetime (no "never").
    let granted = grant_scope(Some(&req.scope));
    let days = req
        .expires_in_days
        .unwrap_or(DEFAULT_TOKEN_TTL_DAYS)
        .clamp(1, MAX_TOKEN_TTL_DAYS);
    let now = Utc::now();
    // The user's choice governs how long the connection lives — i.e. the refresh
    // token. The access token is short-lived and auto-renewed against it.
    let refresh_expires_at = now + Duration::days(i64::from(days));

    // Mint a high-entropy code; store only its SHA-256.
    let mut bytes = [0u8; 32];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng must succeed for auth code");
    let code = URL_SAFE_NO_PAD.encode(bytes);
    let code_hash = sha256_hex(&code);

    let auth_code = store::AuthCode {
        client_id: req.client_id.clone(),
        redirect_uri: req.redirect_uri.clone(),
        code_challenge: req.code_challenge.clone(),
        scope: granted,
        resource: urls.resource.clone(),
        user_id: user,
        org_id: org,
        expires_at: now + Duration::seconds(CODE_TTL_SECS),
        refresh_expires_at,
    };
    match store::insert_code(pool, &code_hash, &auth_code).await {
        Ok(true) => {}
        Ok(false) => return error_page(UNKNOWN_CLIENT),
        Err(e) => {
            tracing::warn!(target: "oauth", error = %e, "insert_code failed");
            return error_page("internal error");
        }
    }

    Json(json!({
        "redirect": redirect_uri_string(&req.redirect_uri, &[("code", code.as_str())], state_opt),
    }))
    .into_response()
}

/// Build a redirect URL string with appended query pairs (for the JSON the
/// consent page navigates to).
fn redirect_uri_string(redirect_uri: &str, pairs: &[(&str, &str)], state: Option<&str>) -> String {
    match url::Url::parse(redirect_uri) {
        Ok(mut url) => {
            {
                let mut q = url.query_pairs_mut();
                for (k, v) in pairs {
                    q.append_pair(k, v);
                }
                if let Some(s) = state {
                    q.append_pair("state", s);
                }
            }
            url.to_string()
        }
        Err(_) => redirect_uri.to_string(),
    }
}

// ── Consent page template ──────────────────────────────────────────────────

struct ConsentScope {
    label: &'static str,
    write: bool,
}

struct ConsentLevel {
    id: &'static str,
    label: &'static str,
    summary: &'static str,
    scope: String,
    grants: Vec<ConsentScope>,
    selected: bool,
}

struct ConsentOrg {
    id: String,
    name: String,
    selected: bool,
}

#[derive(askama::Template, askama_web::WebTemplate)]
#[template(path = "oauth/consent.html")]
struct ConsentPage {
    /// base.html nav uses this; "" shows the standard nav with nothing active.
    active_tab: &'static str,
    client_name: String,
    /// The level the client's own request maps to, preselected.
    requested_label: &'static str,
    requested_writes: bool,
    orgs: Vec<ConsentOrg>,
    levels: Vec<ConsentLevel>,
    destination: String,
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
    resource: String,
    default_ttl_days: u32,
}
