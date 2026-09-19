//! Transport-level auth + gating for the read MCP server at `/mcp`.
//!
//! These cover the unauthenticated discovery path (initialize and the `*_list`
//! methods are open so directories can read the catalog), the no-DB rejection
//! paths (execution and any present-but-invalid token must fail closed), and the
//! `cfg.mcp.enabled` mount gate. The full JSON-RPC tool round-trip is exercised
//! against a live PG+CH via `mcp-remote`/Claude Desktop. See the connector
//! smoke notes.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::build_test_app_with_web;
use tower::ServiceExt;

fn mcp_post_body(authorization: Option<&str>, body: &'static str) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream");
    if let Some(a) = authorization {
        b = b.header("authorization", a);
    }
    b.body(Body::from(body)).unwrap()
}

/// `initialize` body declaring (or not) the elicitation client capability.
fn initialize_body(elicitation: bool) -> String {
    let caps = if elicitation {
        r#"{"elicitation":{}}"#
    } else {
        "{}"
    };
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{caps},"clientInfo":{{"name":"probe","version":"0"}}}}}}"#
    )
}

fn session_post(session: &str, body: &'static str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("host", "localhost")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", session)
        .body(Body::from(body))
        .unwrap()
}

/// Handshake, then read the tool catalog that client is offered. Bodies come
/// back as SSE frames, so the JSON is whatever follows the `data:` prefix.
async fn tool_names_for_client(elicitation: bool) -> Vec<String> {
    let app = build_test_app_with_web(|cfg| cfg.mcp.enabled = true);
    let init = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("host", "localhost")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(initialize_body(elicitation)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(init.status().is_success(), "initialize: {}", init.status());
    let session = init
        .headers()
        .get("mcp-session-id")
        .expect("transport must issue a session id")
        .to_str()
        .unwrap()
        .to_string();

    let resp = app
        .oneshot(session_post(
            &session,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success(), "tools/list: {}", resp.status());
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    // The stream opens with an empty keep-alive frame; the answer is the first
    // `data:` line carrying anything.
    let json = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| panic!("no data frame in {text:?}"));
    let parsed: serde_json::Value = serde_json::from_str(json).expect("tools/list json");
    parsed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn tool_catalog_is_the_same_whether_or_not_the_client_can_confirm() {
    let without = tool_names_for_client(false).await;
    let with = tool_names_for_client(true).await;
    assert_eq!(without, with);
    for write in [
        "pause_monitor",
        "run_check_now",
        "publish_incident",
        "post_incident_update",
    ] {
        assert!(
            without.contains(&write.to_string()),
            "{write} in {without:?}"
        );
    }
}

fn mcp_post(authorization: Option<&str>) -> Request<Body> {
    mcp_post_body(
        authorization,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    )
}

const INITIALIZE_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"glama","version":"0"}}}"#;
const TOOLS_CALL_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_org_health","arguments":{}}}"#;

#[tokio::test]
async fn mcp_disabled_is_not_mounted() {
    // Default config leaves the server off; `/mcp` must 404, not 401.
    let app = build_test_app_with_web(|_| {});
    let resp = app
        .oneshot(mcp_post(Some("Bearer sm_live_anything")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mcp_without_token_allows_discovery() {
    // Discovery (initialize) must pass auth with no credential so MCP
    // directories and clients can read the public tool catalog.
    let app = build_test_app_with_web(|cfg| cfg.mcp.enabled = true);
    let resp = app
        .oneshot(mcp_post_body(None, INITIALIZE_BODY))
        .await
        .unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated discovery must not be challenged"
    );
    assert!(
        resp.status().is_success(),
        "initialize should succeed without a credential, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn mcp_without_token_blocks_execution() {
    // A tool call with no credential still challenges, so the OAuth step-up
    // fires for real clients.
    let app = build_test_app_with_web(|cfg| cfg.mcp.enabled = true);
    let resp = app
        .oneshot(mcp_post_body(None, TOOLS_CALL_BODY))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers().get("www-authenticate").unwrap(),
        "Bearer",
        "401s must advertise the Bearer scheme for the OAuth step-up"
    );
}

#[tokio::test]
async fn mcp_with_non_sm_live_bearer_challenges() {
    let app = build_test_app_with_web(|cfg| cfg.mcp.enabled = true);
    let resp = app
        .oneshot(mcp_post(Some("Bearer github_pat_xxx")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mcp_with_sm_live_token_but_no_db_challenges() {
    // The in-memory harness has `db: None`; an `sm_live_` token can't be looked
    // up, so the door fails closed rather than letting an unverified token in.
    let app = build_test_app_with_web(|cfg| cfg.mcp.enabled = true);
    let resp = app
        .oneshot(mcp_post(Some(
            "Bearer sm_live_0000000000000000000000000000000000000000000",
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn server_card_is_public_and_matches_the_running_server() {
    let app = build_test_app_with_web(|cfg| {
        cfg.mcp.enabled = true;
        cfg.mcp.resource_uri = "https://mcp.example.test/mcp".into();
        cfg.marketing.canonical_origin = "https://example.test".into();
    });
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/.well-known/mcp/server-card.json")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let card: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(card["name"], "test.example/uptimepage");
    assert_eq!(card["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(card["remotes"][0]["url"], "https://mcp.example.test/mcp");
    assert_eq!(card["remotes"][0]["transport"], "streamable-http");
    assert!(card["capabilities"]["tools"].is_object(), "{card}");
}

#[tokio::test]
async fn server_card_is_absent_without_a_configured_endpoint() {
    let app = build_test_app_with_web(|cfg| cfg.mcp.enabled = true);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/.well-known/mcp/server-card.json")
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
