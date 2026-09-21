use std::time::Duration;

use anyhow::Context;
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use hyper::body::Bytes;
use hyper::header::{ACCEPT, CONTENT_TYPE};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::error::{AppError, Result};
use crate::security::{SsrfGuard, SsrfHttpConnector};

/// Shared HTTPS client used by outbound non-check traffic (notification
/// transports, RDAP lookups). Uses [`SsrfHttpConnector`] so a webhook URL
/// pointing at a private IP (loopback, RFC1918, link-local, cloud metadata,
/// 6to4 / NAT64-embedded private v4) is dropped at DNS-filter time before
/// any TCP open — DNS-rebinding safe by construction.
///
/// Distinct from `HttpClients` (the check-path client): that one wraps a
/// custom connector that records DNS/connect/TLS/TTFB histograms and uses
/// the shared Hickory resolver, both of which would poison check metrics
/// when called from non-check paths.
pub type OutboundHttpClient = Client<HttpsConnector<SsrfHttpConnector>, Full<Bytes>>;

// Cap outbound response bodies at 1 MiB: IANA RDAP bootstrap is ~50 KiB, RDAP
// per-domain responses are <10 KiB, Slack/webhook ack bodies are tiny. Anything
// larger is a misconfigured or hostile endpoint, and we want a streamed limit
// so we never allocate a multi-MB buffer just to reject it.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// One outbound exchange: connect, headers and body together. Per-phase budgets
/// would let an endpoint slow at each hold a task for twice this long.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `hyper_util`'s error prints only its kind; the cause sits in `source()`.
pub fn error_chain(e: impl std::error::Error + Send + Sync + 'static) -> String {
    format!("{:#}", anyhow::Error::new(e))
}

pub fn build_outbound_client(guard: SsrfGuard) -> OutboundHttpClient {
    Client::builder(TokioExecutor::new()).build(https_connector(guard))
}

/// One connection per request: hyper cannot replay a POST whose body it has
/// already written to a pooled connection the far side dropped.
pub fn build_single_use_outbound_client(guard: SsrfGuard) -> OutboundHttpClient {
    Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build(https_connector(guard))
}

fn https_connector(guard: SsrfGuard) -> HttpsConnector<SsrfHttpConnector> {
    crate::http_client::client::install_default_crypto_provider();
    // Native trust store first; fall back to the bundled webpki roots when
    // it can't be read (an empty/broken store, or a macOS keychain I/O
    // hiccup under load) rather than panicking the process. Same rule as
    // `http_client::client::server_roots`, which needs an owned store.
    match hyper_rustls::HttpsConnectorBuilder::new().with_native_roots() {
        Ok(b) => b,
        Err(_) => hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots(),
    }
    .https_or_http()
    .enable_http1()
    .enable_http2()
    .wrap_connector(SsrfHttpConnector::new(guard))
}

pub async fn post_json<T: Serialize>(
    client: &OutboundHttpClient,
    url: &Url,
    body: &T,
) -> Result<()> {
    post_json_with_headers(client, url, body, &std::collections::BTreeMap::new()).await
}

/// Like [`post_json`] but adds caller-supplied request headers (generic
/// webhook channels let users attach e.g. an `Authorization` header). A
/// header whose name or value is not valid HTTP is skipped rather than
/// failing the whole delivery — the receiver still gets the payload.
pub async fn post_json_with_headers<T: Serialize>(
    client: &OutboundHttpClient,
    url: &Url,
    body: &T,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let payload = serde_json::to_vec(body).context("serializing request payload")?;
    post_bytes_with_headers(client, url, payload, headers).await
}

/// POST pre-serialized JSON bytes with caller-supplied headers. Lets a signed
/// transport hash the exact bytes it sends (the signature must cover the wire
/// body verbatim, so serialization can't happen twice). Same header-skip and
/// bounded-error-body behaviour as [`post_json_with_headers`].
pub async fn post_bytes_with_headers(
    client: &OutboundHttpClient,
    url: &Url,
    payload: Vec<u8>,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    post_bytes_ct(client, url, "application/json", payload, headers).await
}

/// POST a `application/x-www-form-urlencoded` body with caller-supplied
/// headers — for gateways whose send API is form-encoded (Twilio). Same
/// header-skip and bounded-error-body behaviour as [`post_bytes_with_headers`].
pub async fn post_form_with_headers(
    client: &OutboundHttpClient,
    url: &Url,
    payload: Vec<u8>,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    post_bytes_ct(
        client,
        url,
        "application/x-www-form-urlencoded",
        payload,
        headers,
    )
    .await
}

async fn post_bytes_ct(
    client: &OutboundHttpClient,
    url: &Url,
    content_type: &str,
    payload: Vec<u8>,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    use hyper::header::{HeaderName, HeaderValue};
    let mut builder = Request::post(url.as_str()).header(CONTENT_TYPE, content_type);
    for (k, v) in headers {
        match (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            (Ok(name), Ok(value)) => builder = builder.header(name, value),
            _ => tracing::warn!(header = %k, "skipping invalid webhook header"),
        }
    }
    let req = builder
        .body(Full::new(Bytes::from(payload)))
        .context("building request")?;
    let at = exchange_deadline();
    let resp = with_request_timeout(url, at, client.request(req)).await?;
    let status = resp.status();
    if !status.is_success() {
        // Losing the body costs a diagnostic; losing the status costs the
        // retry decision the escalation engine reads from it.
        let bytes = diagnostic_body(url, at, resp.into_body(), MAX_RESPONSE_BYTES).await;
        let body = String::from_utf8_lossy(&bytes);
        return Err(AppError::Other(anyhow::anyhow!(
            "endpoint returned {status}: {body}"
        )));
    }
    Ok(())
}

/// POST JSON and parse a `2xx` response body into `R`. Like [`post_json`] but
/// keeps the reply — transports that return an id to track later (Pushover's
/// emergency `receipt`) need it. Same bounded-body and error shape.
pub async fn post_json_capture<T: Serialize, R: DeserializeOwned>(
    client: &OutboundHttpClient,
    url: &Url,
    body: &T,
) -> Result<R> {
    let payload = serde_json::to_vec(body).context("serializing request payload")?;
    let req = Request::post(url.as_str())
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .body(Full::new(Bytes::from(payload)))
        .context("building request")?;
    let at = exchange_deadline();
    let resp = with_request_timeout(url, at, client.request(req)).await?;
    let status = resp.status();
    let collected = read_body_within(url, at, resp.into_body(), MAX_RESPONSE_BYTES).await;
    if !status.is_success() {
        let bytes = collected
            .ok()
            .and_then(std::result::Result::ok)
            .unwrap_or_default();
        let snippet = String::from_utf8_lossy(&bytes);
        return Err(AppError::Other(anyhow::anyhow!(
            "endpoint returned {status}: {snippet}"
        )));
    }
    let bytes = collected?.unwrap_or_default();
    serde_json::from_slice(&bytes).map_err(|e| AppError::Other(anyhow::anyhow!("{url}: {e}")))
}

fn exchange_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + REQUEST_TIMEOUT
}

async fn with_request_timeout<F, T, E>(url: &Url, at: tokio::time::Instant, fut: F) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout_at(at, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(AppError::Other(anyhow::anyhow!(
            "sending request to {url}: {}",
            error_chain(e)
        ))),
        Err(_) => Err(AppError::Other(anyhow::anyhow!(
            "request to {url} exceeded {REQUEST_TIMEOUT:?}"
        ))),
    }
}

/// The status already answered the caller, so neither a stall nor a read
/// failure may replace it.
async fn diagnostic_body(
    url: &Url,
    at: tokio::time::Instant,
    body: hyper::body::Incoming,
    max_bytes: usize,
) -> Bytes {
    read_body_within(url, at, body, max_bytes)
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .unwrap_or_default()
}

/// `Limited` bounds bytes, not seconds, so without a clock an endpoint that
/// answers and then stalls mid-body holds the caller forever — and some of
/// these callers hold one of sixteen global paging permits.
///
/// Outer `Err` is that timeout and is fatal; inner is the size or read failure
/// each caller already handles.
async fn read_body_within(
    url: &Url,
    at: tokio::time::Instant,
    body: hyper::body::Incoming,
    max_bytes: usize,
) -> Result<std::result::Result<Bytes, String>> {
    match tokio::time::timeout_at(at, Limited::new(body, max_bytes).collect()).await {
        Ok(Ok(collected)) => Ok(Ok(collected.to_bytes())),
        Ok(Err(e)) => Ok(Err(e.to_string())),
        Err(_) => Err(AppError::Other(anyhow::anyhow!(
            "reading the response from {url} exceeded {REQUEST_TIMEOUT:?}"
        ))),
    }
}

/// GET a URL and succeed on any 2xx, discarding the body. For fire-and-check
/// pings (e.g. an external heartbeat snitch) where only reachability + status
/// matter. The body is bounded-drained so a hostile endpoint can't OOM us.
/// The returned error embeds `url`; callers whose URL carries a secret token
/// must not log it verbatim.
pub async fn get_ok(client: &OutboundHttpClient, url: &Url) -> Result<()> {
    let req = Request::get(url.as_str())
        .body(Full::new(Bytes::new()))
        .context("building request")?;
    let at = exchange_deadline();
    let resp = with_request_timeout(url, at, client.request(req)).await?;
    let status = resp.status();
    // Drained so the connection can be pooled; only status was promised.
    diagnostic_body(url, at, resp.into_body(), MAX_RESPONSE_BYTES).await;
    if !status.is_success() {
        return Err(AppError::Other(anyhow::anyhow!("{url} returned {status}")));
    }
    Ok(())
}

/// GET a plain-text document, bounded by an explicit `max_bytes` rather than
/// the shared [`MAX_RESPONSE_BYTES`]. Feeds are legitimately larger than any
/// notification payload — the disposable-domain corpus is over a megabyte — so
/// the caller states the ceiling it is prepared to hold in memory.
pub async fn get_text(client: &OutboundHttpClient, url: &Url, max_bytes: usize) -> Result<String> {
    let req = Request::get(url.as_str())
        .header(ACCEPT, "text/plain")
        .body(Full::new(Bytes::new()))
        .context("building request")?;
    let at = exchange_deadline();
    let resp = with_request_timeout(url, at, client.request(req)).await?;
    let status = resp.status();
    let collected = read_body_within(url, at, resp.into_body(), max_bytes).await;
    if !status.is_success() {
        return Err(AppError::Other(anyhow::anyhow!("{url} returned {status}")));
    }
    let bytes = collected?.map_err(|e| {
        AppError::Other(anyhow::anyhow!(
            "{url} body exceeded {max_bytes} bytes or read failed: {e}"
        ))
    })?;
    String::from_utf8(bytes.to_vec())
        .map_err(|e| AppError::Other(anyhow::anyhow!("{url}: body is not UTF-8: {e}")))
}

pub async fn get_json<T: DeserializeOwned>(client: &OutboundHttpClient, url: &Url) -> Result<T> {
    let req = Request::get(url.as_str())
        .header(ACCEPT, "application/json")
        .body(Full::new(Bytes::new()))
        .context("building request")?;
    let at = exchange_deadline();
    let resp = with_request_timeout(url, at, client.request(req)).await?;
    let status = resp.status();
    let collected = read_body_within(url, at, resp.into_body(), MAX_RESPONSE_BYTES).await;

    // Report a non-2xx from the status alone: an unframed body (no
    // Content-Length, closed mid-read) or one that stalls must not mask it.
    if !status.is_success() {
        let detail = collected
            .ok()
            .and_then(std::result::Result::ok)
            .map(|b| error_body_summary(&b))
            .filter(|s| !s.is_empty())
            .map(|s| format!(": {s}"))
            .unwrap_or_default();
        return Err(AppError::Other(anyhow::anyhow!(
            "{url} returned {status}{detail}"
        )));
    }

    let bytes = collected?.map_err(|e| {
        AppError::Other(anyhow::anyhow!(
            "{url} body exceeded {MAX_RESPONSE_BYTES} bytes or read failed: {e}"
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|e| AppError::Other(anyhow::anyhow!("{url}: {e}")))
}

/// Bounded, operator-facing summary of an error body: prefers a JSON error
/// field (RDAP `description`, or `title`/`message`/`error`) so ToS boilerplate
/// doesn't drown the reason; else truncated raw text.
fn error_body_summary(bytes: &[u8]) -> String {
    const CAP: usize = 300;
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        for key in [
            "description",
            "title",
            "message",
            "error",
            "error_description",
        ] {
            let text = match value.get(key) {
                Some(serde_json::Value::String(s)) => s.trim().to_owned(),
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("; "),
                _ => continue,
            };
            if !text.is_empty() {
                return crate::text::truncate_bytes(&text, CAP);
            }
        }
    }
    crate::text::truncate_bytes(String::from_utf8_lossy(bytes).trim(), CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One byte of a much longer declared body, then nothing.
    async fn stalling_body_server() -> Url {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut scratch = [0u8; 1024];
                    let _ = sock.read(&mut scratch).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nx")
                        .await;
                    let _ = sock.flush().await;
                    std::future::pending::<()>().await;
                });
            }
        });
        Url::parse(&format!("http://{addr}/list.txt")).unwrap()
    }

    /// Real clock, short deadline: a paused clock races the server's headers
    /// and fires the *header* timeout instead, which passes an assertion that
    /// only looks for a timeout.
    #[tokio::test]
    async fn a_body_that_never_finishes_is_cut_off() {
        let url = stalling_body_server().await;
        let client = build_outbound_client(crate::security::SsrfGuard::relaxed_for_tests());
        let req = Request::get(url.as_str())
            .body(Full::new(Bytes::new()))
            .unwrap();

        let resp = tokio::time::timeout(Duration::from_secs(5), client.request(req))
            .await
            .expect("headers are answered at once")
            .expect("request");

        let at = tokio::time::Instant::now() + Duration::from_millis(100);
        let err = read_body_within(&url, at, resp.into_body(), MAX_RESPONSE_BYTES)
            .await
            .expect_err("a body that never finishes must not be waited on forever");

        assert!(
            err.to_string().contains("reading the response from"),
            "want the body-read timeout, got: {err}"
        );
    }

    #[test]
    fn error_summary_prefers_rdap_description_over_tos_notices() {
        let body = br#"{
            "errorCode": 400,
            "title": "Bad Request",
            "description": ["app.example.dev is not a valid domain name: Domain name must have exactly one part above the TLD"],
            "notices": [{"description": ["By querying our Domain Database you agree to the terms ..."]}]
        }"#;
        let summary = error_body_summary(body);
        assert!(summary.contains("not a valid domain name"));
        assert!(!summary.contains("terms"));
    }

    #[test]
    fn error_summary_reads_generic_message_fields() {
        assert_eq!(error_body_summary(br#"{"message":"boom"}"#), "boom");
        assert_eq!(error_body_summary(br#"{"error":"nope"}"#), "nope");
    }

    #[test]
    fn error_summary_falls_back_to_raw_text() {
        assert_eq!(
            error_body_summary(b"plain text failure"),
            "plain text failure"
        );
    }

    #[test]
    fn error_summary_caps_long_body() {
        let out = error_body_summary(&vec![b'x'; 500]);
        assert!(out.len() <= 300);
        assert!(out.ends_with('…'));
    }
}
