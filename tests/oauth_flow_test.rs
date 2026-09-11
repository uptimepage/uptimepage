//! End-to-end OAuth 2.1 authorization-code + PKCE flow against a real Postgres.
//! No-ops when DATABASE_URL is unset (mirrors the other live-DB suites).
//!
//! Covers the security-critical path and its key failure modes: PKCE binding,
//! single-use codes, exact redirect_uri matching, RFC 8707 resource binding,
//! and audience-checked access at /mcp.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{body_json, build_test_app_with_pg_store, pg_pool_from_env};
use tower::ServiceExt;

const ISSUER: &str = "https://app.test.example";
const RESOURCE: &str = "https://mcp.test.example/mcp";
const REDIRECT: &str = "https://claude.ai/api/mcp/auth/callback";
// RFC 7636 Appendix B known-good verifier/challenge pair.
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// Percent-encode every byte outside the unreserved set — enough to build query
/// strings + form bodies without pulling a url crate into the test.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == key
        {
            return Some(v.to_string());
        }
    }
    None
}

async fn send(app: &Router, req: Request<Body>) -> axum::http::Response<Body> {
    app.clone().oneshot(req).await.unwrap()
}

fn cfg_oauth(cfg: &mut uptimepage::config::AppConfig) {
    cfg.mcp.enabled = true;
    cfg.mcp.oauth_enabled = true;
    cfg.mcp.resource_uri = RESOURCE.into();
    cfg.auth.public_base_url = ISSUER.into();
}

async fn register_client(app: &Router) -> String {
    register_client_at(app, REDIRECT).await
}

async fn register_client_at(app: &Router, redirect: &str) -> String {
    let resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "client_name": "Claude",
                    "redirect_uris": [redirect],
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let j = body_json(resp).await;
    assert_eq!(j["token_endpoint_auth_method"], "none");
    j["client_id"].as_str().unwrap().to_string()
}

fn authorize_uri(client_id: &str, redirect: &str, resource: &str, challenge: &str) -> String {
    format!(
        "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&scope={}&state=xyz&resource={}",
        enc(client_id),
        enc(redirect),
        enc(challenge),
        enc("targets:read"),
        enc(resource),
    )
}

/// Approve with an explicit requested scope (exercises opt-in write scopes).
async fn approve_with_scope(app: &Router, client_id: &str, redirect: &str, scope: &str) -> String {
    let resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/oauth/authorize/decision")
            .header("content-type", "application/json")
            .header("x-requested-with", "uptimepage")
            .body(Body::from(
                serde_json::json!({
                    "action": "approve",
                    "client_id": client_id,
                    "redirect_uri": redirect,
                    "code_challenge": CHALLENGE,
                    "scope": scope,
                    "state": "xyz",
                    "resource": RESOURCE,
                    "expires_in_days": 30,
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    query_param(j["redirect"].as_str().unwrap(), "code").expect("code in redirect")
}

async fn approve(app: &Router, client_id: &str, redirect: &str) -> String {
    let resp = send(
        app,
        Request::builder()
            .method("POST")
            .uri("/oauth/authorize/decision")
            .header("content-type", "application/json")
            .header("x-requested-with", "uptimepage")
            .body(Body::from(
                serde_json::json!({
                    "action": "approve",
                    "client_id": client_id,
                    "redirect_uri": redirect,
                    "code_challenge": CHALLENGE,
                    "scope": "targets:read",
                    "state": "xyz",
                    "resource": RESOURCE,
                    "expires_in_days": 30,
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let redirect_url = j["redirect"].as_str().unwrap().to_string();
    assert_eq!(query_param(&redirect_url, "state").as_deref(), Some("xyz"));
    query_param(&redirect_url, "code").expect("code in redirect")
}

fn token_body(code: &str, redirect: &str, client_id: &str, verifier: &str) -> String {
    format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        enc(code),
        enc(redirect),
        enc(client_id),
        enc(verifier),
    )
}

fn refresh_body(refresh: &str, client_id: &str) -> String {
    format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        enc(refresh),
        enc(client_id),
    )
}

/// Run the full code+PKCE flow and return (client_id, refresh_token).
async fn obtain_tokens(app: &Router) -> (String, String) {
    let client_id = register_client(app).await;
    let code = approve(app, &client_id, REDIRECT).await;
    let resp = post_token(app, token_body(&code, REDIRECT, &client_id, VERIFIER)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let refresh = j["refresh_token"].as_str().unwrap().to_string();
    (client_id, refresh)
}

async fn post_token(app: &Router, body: String) -> axum::http::Response<Body> {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn full_authorization_code_pkce_flow() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;

    let client_id = register_client(&app).await;

    // Consent screen renders for the logged-in owner.
    let resp = send(
        &app,
        Request::builder()
            .uri(authorize_uri(&client_id, REDIRECT, RESOURCE, CHALLENGE))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let code = approve(&app, &client_id, REDIRECT).await;

    // Exchange the code → audience-bound access token.
    let resp = post_token(&app, token_body(&code, REDIRECT, &client_id, VERIFIER)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let token = j["access_token"].as_str().unwrap().to_string();
    assert!(token.starts_with("sm_live_"));
    assert_eq!(j["token_type"], "Bearer");
    assert_eq!(j["scope"], "targets:read");
    assert!(
        j["refresh_token"].as_str().is_some_and(|r| !r.is_empty()),
        "code grant must also issue a refresh token"
    );
    // Access token is short-lived (auto-renewed via refresh), not the 30d pick.
    assert!(j["expires_in"].as_i64().unwrap() <= 3600);

    // The minted token is accepted at /mcp (audience matches) — not a 401.
    let resp = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            ))
            .unwrap(),
    )
    .await;
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "audience-bound token must pass /mcp auth"
    );
}

#[tokio::test]
async fn a_native_client_on_its_own_scheme_completes_the_flow() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let redirect = "cursor://anysphere.cursor-retrieval/oauth/user-7/callback";

    let client_id = register_client_at(&app, redirect).await;

    // Consent renders with the native redirect in its hidden field.
    let resp = send(
        &app,
        Request::builder()
            .uri(authorize_uri(&client_id, redirect, RESOURCE, CHALLENGE))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&html).contains(redirect),
        "consent page carries the redirect"
    );

    // A pre-login error goes back on the same scheme, as a Location the
    // server can actually emit.
    let resp = send(
        &app,
        Request::builder()
            .uri(authorize_uri(
                &client_id,
                redirect,
                "https://other.example/mcp",
                CHALLENGE,
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(loc.starts_with(redirect), "{loc}");
    assert_eq!(query_param(loc, "error").as_deref(), Some("invalid_target"));

    let code = approve(&app, &client_id, redirect).await;
    let resp = post_token(&app, token_body(&code, redirect, &client_id, VERIFIER)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert!(
        j["access_token"]
            .as_str()
            .is_some_and(|t| t.starts_with("sm_live_"))
    );
}

#[tokio::test]
async fn reused_code_is_rejected() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let client_id = register_client(&app).await;
    let code = approve(&app, &client_id, REDIRECT).await;

    let first = post_token(&app, token_body(&code, REDIRECT, &client_id, VERIFIER)).await;
    assert_eq!(first.status(), StatusCode::OK);

    // Second exchange of the same code must fail — single-use.
    let second = post_token(&app, token_body(&code, REDIRECT, &client_id, VERIFIER)).await;
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);
    let j = body_json(second).await;
    assert_eq!(j["error"], "invalid_grant");
}

#[tokio::test]
async fn wrong_pkce_verifier_is_rejected() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let client_id = register_client(&app).await;
    let code = approve(&app, &client_id, REDIRECT).await;

    // A different (valid-shaped) verifier whose SHA-256 ≠ the bound challenge.
    let bad = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let resp = post_token(&app, token_body(&code, REDIRECT, &client_id, bad)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"], "invalid_grant");
}

#[tokio::test]
async fn unregistered_redirect_uri_is_rejected() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let client_id = register_client(&app).await;

    // Authorize with a redirect_uri the client never registered → local 400,
    // never a redirect to the attacker URI.
    let resp = send(
        &app,
        Request::builder()
            .uri(authorize_uri(
                &client_id,
                "https://attacker.example/cb",
                RESOURCE,
                CHALLENGE,
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn oauth_grants_write_scope_only_when_requested() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let client_id = register_client(&app).await;

    // A connector that asks for write gets a write-scoped token (write ⇒ read).
    let code =
        approve_with_scope(&app, &client_id, REDIRECT, "targets:write incidents:write").await;
    let resp = post_token(&app, token_body(&code, REDIRECT, &client_id, VERIFIER)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let scope = body_json(resp).await["scope"].as_str().unwrap().to_string();
    assert!(scope.contains("targets:write"), "got scope: {scope}");
    assert!(scope.contains("incidents:write"), "got scope: {scope}");
    // write implies read — the connector can still read too.
    assert!(scope.contains("targets:read"), "got scope: {scope}");
}

#[tokio::test]
async fn refresh_token_rotates() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let (client_id, refresh1) = obtain_tokens(&app).await;

    let resp = post_token(&app, refresh_body(&refresh1, &client_id)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    let refresh2 = j["refresh_token"].as_str().unwrap();
    assert!(j["access_token"].as_str().unwrap().starts_with("sm_live_"));
    assert_ne!(refresh2, refresh1, "refresh token must rotate on use");
    assert_eq!(
        j["scope"], "targets:read",
        "scope must be preserved, not widened"
    );
}

#[tokio::test]
async fn refresh_replay_revokes_family() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let (client_id, refresh1) = obtain_tokens(&app).await;

    // First rotation succeeds, yielding refresh2.
    let resp = post_token(&app, refresh_body(&refresh1, &client_id)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let refresh2 = body_json(resp).await["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    // Replaying the retired refresh1 → invalid_grant + family burned.
    let replay = post_token(&app, refresh_body(&refresh1, &client_id)).await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(replay).await["error"], "invalid_grant");

    // The whole family is now revoked: even the current refresh2 is dead.
    let after = post_token(&app, refresh_body(&refresh2, &client_id)).await;
    assert_eq!(
        after.status(),
        StatusCode::BAD_REQUEST,
        "replay must revoke the entire refresh family"
    );
}

#[tokio::test]
async fn wrong_resource_is_rejected() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };
    let (app, _org) = build_test_app_with_pg_store(pool, cfg_oauth).await;
    let client_id = register_client(&app).await;

    // A mismatched RFC 8707 resource → error redirect (not a consent screen).
    let resp = send(
        &app,
        Request::builder()
            .uri(authorize_uri(
                &client_id,
                REDIRECT,
                "https://other.example/mcp",
                CHALLENGE,
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(loc.starts_with(REDIRECT));
    assert_eq!(query_param(loc, "error").as_deref(), Some("invalid_target"));
}
