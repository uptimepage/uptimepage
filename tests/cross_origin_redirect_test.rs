//! A sensitive request header (auth, cookies, API keys) must not follow a
//! redirect to a different origin, while a plain custom header still may. Guards
//! the credential-leak path for both configured headers and values resolved
//! from secret variables (resolution happens server-side, so the worker sees
//! plain values and strips by header name).

use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::response::Redirect;
use axum::routing::get;
use uptimepage::domain::{CheckStatus, ExpectedStatus, HttpMethod};
use uptimepage::worker::execute_http_check;
use url::Url;
use uuid::Uuid;

use common::{spawn_router, test_client};

#[tokio::test]
async fn sensitive_header_dropped_across_origin_plain_kept() {
    // Origin B records the request headers it receives.
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let app_b = Router::new().route(
        "/capture",
        get(move |headers: HeaderMap| {
            let sink = sink.clone();
            async move {
                sink.lock()
                    .unwrap()
                    .extend(headers.keys().map(|k| k.as_str().to_ascii_lowercase()));
                "ok"
            }
        }),
    );
    let addr_b = spawn_router(app_b).await;

    // Origin A (a different port) redirects to B.
    let target = format!("http://{addr_b}/capture");
    let app_a = Router::new().route(
        "/r",
        get(move || {
            let target = target.clone();
            async move { Redirect::to(&target) }
        }),
    );
    let addr_a = spawn_router(app_a).await;

    let mut headers = HashMap::new();
    headers.insert("x-api-key".to_string(), "sk-live-secret".to_string());
    headers.insert("x-trace".to_string(), "plain-value".to_string());
    let check = uptimepage::domain::HttpCheck {
        url: Url::parse(&format!("http://{addr_a}/r")).unwrap(),
        method: HttpMethod::Get,
        timeout: Duration::from_secs(3),
        follow_redirects: true,
        max_redirects: 3,
        expected_status: ExpectedStatus::Exact(200),
        expected_body_contains: None,
        headers,
        body: None,
        verify_tls: true,
        basic_auth: None,
        bearer_token: None,
    };

    let client = test_client();
    let r = execute_http_check(Uuid::now_v7(), Uuid::nil(), &check, &client).await;
    assert_eq!(r.status, CheckStatus::Up, "error: {:?}", r.error);

    let received = seen.lock().unwrap();
    assert!(
        !received.iter().any(|h| h == "x-api-key"),
        "sensitive header leaked across origin: {received:?}"
    );
    assert!(
        received.iter().any(|h| h == "x-trace"),
        "plain header should still reach the redirect target: {received:?}"
    );
}
