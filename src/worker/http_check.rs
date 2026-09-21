use std::borrow::Cow;
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bytes::Bytes;
use chrono::Utc;
use flate2::read::GzDecoder;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Bytes as HBytes;
use hyper::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, HOST, HeaderName, HeaderValue, LOCATION, RETRY_AFTER,
    USER_AGENT,
};
use hyper::{Method, Request, Uri};
use metrics::counter;
use uuid::Uuid;

use crate::domain::{
    CheckDiagnostic, CheckDiagnosticKind, CheckResult, CheckStatus, DiagnosticConfidence,
    DiagnosticEvidence, EdgeProvider, ExpectedStatus, HttpCheck, HttpMethod,
};
use crate::http_client::HttpClients;
use crate::http_client::connector::{
    ConnGuard, PhaseTimings, TimedConnection, handshake, timed_connect,
};
use crate::http_outbound::error_chain;
use crate::metric_names;

/// Redirect-hop ceiling, and the fallback when following is on but `max_redirects` is 0.
const MAX_REDIRECT_HOPS: u8 = HttpCheck::MAX_REDIRECTS;

/// Past this the read is abandoned rather than left to allocate freely.
/// Status pages, the dominant target shape, sit well under 100 KiB.
const MAX_RAW_BODY_BYTES: usize = 1 << 20;

/// Names the cap so the fix — assert on something smaller — is readable from
/// the result row alone.
const OVER_CAP_ERROR: &str = "body over the 1 MiB read cap";

/// Bounds gzip/brotli expansion against a zip bomb. 8 MiB tolerates the ~8×
/// that real HTML/JSON pages hit.
const MAX_DECODED_BODY_BYTES: usize = 8 << 20;

/// A page that compresses well clears the raw cap and is only refused here,
/// so the two caps need separate wording.
const OVER_DECODED_CAP_ERROR: &str = "body over the 8 MiB decoded cap";

/// Body snippet cap for the verbose test-check response.
const PROBE_BODY_SNIPPET_BYTES: usize = 1024;
/// Bounds the diagnostic scan, which would otherwise re-allocate the decoded
/// body to lowercase it.
const DIAGNOSTIC_BODY_BYTES: usize = 16 << 10;
/// Header-pair cap (after dedup-by-name) for the verbose test-check response.
const PROBE_HEADERS_MAX: usize = 20;
/// Header names whose values are credentials: redacted from the verbose
/// response, and dropped from a request that follows a redirect off-origin.
pub(crate) const PROBE_REDACT_HEADERS: &[&str] = &[
    "set-cookie",
    "authorization",
    "cookie",
    "proxy-authorization",
    "x-auth-token",
    "x-api-key",
    "api-key",
    "x-csrf-token",
    "x-amz-security-token",
];

/// Captured HTTP-level detail for the test-check endpoint.
#[derive(Debug, Clone, Default)]
pub(crate) struct HttpProbe {
    pub response_headers_preview: Vec<crate::domain::agent_wire::HeaderPreview>,
    pub response_body_snippet: Option<String>,
}

pub async fn execute_http_check(
    target_id: Uuid,
    org_id: Uuid,
    check: &HttpCheck,
    clients: &HttpClients,
) -> CheckResult {
    do_http_check(target_id, org_id, check, clients, false)
        .await
        .0
}

/// Verbose variant for the operator test-check UI. Same result plus a
/// probe of headers + body snippet (sensitive values redacted, body capped).
pub(crate) async fn execute_http_check_probe(
    target_id: Uuid,
    org_id: Uuid,
    check: &HttpCheck,
    clients: &HttpClients,
) -> (CheckResult, HttpProbe) {
    let (r, p) = do_http_check(target_id, org_id, check, clients, true).await;
    (r, p.unwrap_or_default())
}

async fn do_http_check(
    target_id: Uuid,
    org_id: Uuid,
    check: &HttpCheck,
    clients: &HttpClients,
    capture: bool,
) -> (CheckResult, Option<HttpProbe>) {
    let started_at = Utc::now();
    let start = Instant::now();

    let origin = check.url.origin();
    let max_hops = effective_max_redirects(check);

    let early = |err: &str, response_code: Option<u16>| -> (CheckResult, Option<HttpProbe>) {
        let mut r = CheckResult::error_with_elapsed(
            target_id,
            org_id,
            started_at,
            start.elapsed().as_millis() as u32,
            err.to_owned(),
        );
        r.response_code = response_code;
        (r, None)
    };

    // Each hop reconnects through the same SSRF-guarded connector, so an
    // internal `Location` is refused at connect time. No per-hop revalidation.
    let mut current = check.url.clone();
    let mut method = check.method;
    let mut send_body = check.body.clone();

    for hop in 0..=max_hops {
        let uri: Uri = match current.as_str().parse() {
            Ok(u) => u,
            Err(_) => return early("invalid url", None),
        };

        let Some(host) = uri.host() else {
            return early("invalid url", None);
        };
        let is_https = uri.scheme_str() == Some("https");
        let port = uri.port_u16().unwrap_or(if is_https { 443 } else { 80 });

        // Fresh connection per hop, each phase timed. DNS+connect+TLS are
        // bounded by the connect budget; the request+body by what's left.
        let params = clients.connect_params(check.verify_tls);
        let remaining = check.timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return early("connect timeout", None);
        }
        let connect_budget = params.connect_timeout.min(remaining);
        let TimedConnection {
            stream,
            timings,
            alpn_h2,
        } = match tokio::time::timeout(connect_budget, timed_connect(&params, host, port, is_https))
            .await
        {
            Err(_) => return early("connect timeout", None),
            Ok(Err(err)) => {
                tracing::debug!(target_id = %target_id, hop, error = %err, "http connect failed");
                let (dns_ms, connect_ms) = err.partial_timings();
                let (mut r, _) = early(err.reason(), None);
                r.dns_ms = dns_ms;
                r.connect_ms = connect_ms;
                return (r, None);
            }
            Ok(Ok(c)) => c,
        };

        // Request-target form depends on the negotiated protocol; `same_origin`
        // gates credential-bearing headers on cross-origin redirects.
        let same_origin = current.origin() == origin;
        let req = match build_request(
            method,
            &uri,
            check,
            &send_body,
            same_origin,
            alpn_h2,
            clients,
        ) {
            Ok(r) => r,
            Err(err) => return early(&format!("request build failed: {err}"), None),
        };

        // Connection up — failures below carry the measured connect timings.
        let stamp =
            |reason: &str| timed_error(target_id, org_id, started_at, start, reason, &timings);

        let remaining = check.timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return stamp("no response");
        }
        let (mut sender, conn_guard) =
            match tokio::time::timeout(remaining, handshake(stream, alpn_h2)).await {
                Err(_) => return stamp("no response"),
                Ok(Err(err)) => return stamp(&request_failure(target_id, hop, err)),
                Ok(Ok(s)) => s,
            };

        let remaining = check.timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return stamp("no response");
        }
        let send_start = Instant::now();
        let response = match tokio::time::timeout(remaining, sender.send_request(req)).await {
            Err(_) => return stamp("no response"),
            Ok(Ok(r)) => r,
            Ok(Err(err)) => return stamp(&request_failure(target_id, hop, err)),
        };
        let ttfb_ms = send_start.elapsed().as_millis().min(u16::MAX as u128) as u16;

        let status_code = response.status().as_u16();

        if check.follow_redirects && is_redirect(status_code) {
            if hop == max_hops {
                counter!(metric_names::CHECK_REDIRECTS, "outcome" => "limit_exceeded").increment(1);
                tracing::warn!(
                    target_id = %target_id,
                    hops = max_hops,
                    "redirect limit exceeded"
                );
                return early("too many redirects", Some(status_code));
            }

            let next = match response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| current.join(loc).ok())
            {
                Some(u) => u,
                None => {
                    counter!(metric_names::CHECK_REDIRECTS, "outcome" => "invalid_location")
                        .increment(1);
                    tracing::warn!(
                        target_id = %target_id,
                        hop,
                        "redirect with missing or unparseable Location"
                    );
                    return early("invalid redirect location", Some(status_code));
                }
            };
            if !matches!(next.scheme(), "http" | "https") {
                counter!(metric_names::CHECK_REDIRECTS, "outcome" => "blocked_scheme").increment(1);
                tracing::warn!(
                    target_id = %target_id,
                    hop,
                    scheme = next.scheme(),
                    "redirect to unsupported scheme"
                );
                return early("unsupported redirect scheme", Some(status_code));
            }

            // 301/302/303 degrade to a bodyless GET: browser behaviour, and a
            // monitor must never silently replay a POST.
            if !matches!(status_code, 307 | 308) {
                method = HttpMethod::Get;
                send_body = None;
            }

            counter!(metric_names::CHECK_REDIRECTS, "outcome" => "followed").increment(1);
            tracing::debug!(
                target_id = %target_id,
                hop,
                status = status_code,
                // Host only — a full URL may carry tokens.
                to_host = next.host_str().unwrap_or("?"),
                "following redirect"
            );

            // Not drained before drop: the next hop is almost always a
            // different host, so there is no pooled connection to reuse.
            current = next;
            continue;
        }

        return finalize(
            target_id,
            org_id,
            check,
            started_at,
            start,
            response,
            status_code,
            timings,
            ttfb_ms,
            conn_guard,
            clients,
            capture,
        )
        .await;
    }

    // The loop always returns; this keeps the function total without a panic.
    early("too many redirects", None)
}

/// Post-connect failure carrying the measured connect-phase timings. `ttfb_ms`
/// stays `None` — no first byte arrived.
fn timed_error(
    target_id: Uuid,
    org_id: Uuid,
    started_at: chrono::DateTime<Utc>,
    start: Instant,
    reason: &str,
    timings: &PhaseTimings,
) -> (CheckResult, Option<HttpProbe>) {
    let mut r = CheckResult::error_with_elapsed(
        target_id,
        org_id,
        started_at,
        start.elapsed().as_millis() as u32,
        reason,
    );
    r.dns_ms = Some(timings.dns_ms);
    r.connect_ms = Some(timings.connect_ms);
    r.tls_ms = timings.tls_ms;
    (r, None)
}

/// Collect, decode, and score the final (non-redirect) response.
// `conn_guard` is held until the body is drained: the response streams from
// its connection task.
#[allow(clippy::too_many_arguments)]
async fn finalize(
    target_id: Uuid,
    org_id: Uuid,
    check: &HttpCheck,
    started_at: chrono::DateTime<Utc>,
    start: Instant,
    response: hyper::Response<hyper::body::Incoming>,
    status_code: u16,
    timings: PhaseTimings,
    ttfb_ms: u16,
    _conn_guard: ConnGuard,
    clients: &HttpClients,
    capture: bool,
) -> (CheckResult, Option<HttpProbe>) {
    clients.ttfb_ms.record(ttfb_ms as f64);

    // Taken before the body: moving the response into Limited drops the
    // headers handle.
    let headers_preview = capture.then(|| snapshot_headers(response.headers()));
    let response_fingerprint = ResponseFingerprint::from_headers(response.headers());
    let retry_after_raw = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .filter(|s| sanitised_retry_after(s))
        .map(|s| s.to_string());
    let status_ok = match_status(status_code, &check.expected_status);
    // Taken before the body is consumed, so a decode or timeout failure still
    // reports evidence that already arrived.
    let header_diagnostic = detect_diagnostic(&response_fingerprint, b"", status_code);

    let err_with_ttfb = |reason: &'static str| -> (CheckResult, Option<HttpProbe>) {
        let mut r = CheckResult::error_with_elapsed(
            target_id,
            org_id,
            started_at,
            start.elapsed().as_millis() as u32,
            reason,
        );
        r.dns_ms = Some(timings.dns_ms);
        r.connect_ms = Some(timings.connect_ms);
        r.tls_ms = timings.tls_ms;
        r.ttfb_ms = Some(ttfb_ms);
        // Status line came back (TTFB recorded) — preserve the code so a body
        // failure still distinguishes 200-then-truncated from 503-broken-body.
        r.response_code = Some(status_code);
        if let Some(diagnostic) = header_diagnostic.as_ref() {
            record_matched_diagnostic(diagnostic);
        }
        r.diagnostic = header_diagnostic.clone();
        let probe = headers_preview.clone().map(|h| HttpProbe {
            response_headers_preview: h,
            response_body_snippet: None,
        });
        (r, probe)
    };

    // HEAD body is empty; the origin still sets Content-Encoding from the
    // GET variant, so any decode attempt fails on the empty stream.
    let mut over_cap = false;
    let decoded = if check.method == HttpMethod::Head {
        Bytes::new()
    } else {
        let content_encoding = response
            .headers()
            .get(hyper::header::CONTENT_ENCODING)
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_ascii_lowercase());
        let body_remaining = check.timeout.saturating_sub(start.elapsed());
        // Bounded before allocation, so an oversized response never sits
        // fully in memory.
        let collected = match tokio::time::timeout(
            body_remaining,
            Limited::new(response.into_body(), MAX_RAW_BODY_BYTES).collect(),
        )
        .await
        {
            Err(_) => return err_with_ttfb("body timeout"),
            Ok(Ok(c)) => Some(c),
            // Without a body assertion the status already decided, so an
            // unbuffered page is no fault.
            Ok(Err(e)) if e.downcast_ref::<LengthLimitError>().is_some() => {
                if check.expected_body_contains.is_some() {
                    return err_with_ttfb(OVER_CAP_ERROR);
                }
                over_cap = true;
                None
            }
            Ok(Err(_)) => return err_with_ttfb("body"),
        };
        match collected {
            None => Bytes::new(),
            Some(c) => {
                let raw = c.to_bytes();
                match decode_body(&raw, content_encoding.as_deref()) {
                    Ok(b) => b,
                    // Same rule as the raw cap: a body nobody asserts on is
                    // not needed, so refusing to hold it is no fault.
                    Err(DecodeError::OverCap) => {
                        if check.expected_body_contains.is_some() {
                            return err_with_ttfb(OVER_DECODED_CAP_ERROR);
                        }
                        over_cap = true;
                        Bytes::new()
                    }
                    Err(DecodeError::Malformed) => return err_with_ttfb("decode"),
                }
            }
        }
    };

    // An abandoned read never learned the length; the cap is not a measurement.
    let size = (!over_cap).then(|| decoded.len() as u32);
    let duration_ms = start.elapsed().as_millis() as u32;

    let body_ok = match &check.expected_body_contains {
        Some(needle) => std::str::from_utf8(&decoded)
            .map(|s| s.contains(needle))
            .unwrap_or(false),
        None => true,
    };

    let (status, error) =
        classify_outcome(status_code, status_ok, body_ok, retry_after_raw.as_deref());
    // Explains a failed verdict, never creates or clears one.
    let diagnostic = (status != CheckStatus::Up)
        .then(|| detect_diagnostic(&response_fingerprint, &decoded, status_code))
        .flatten();
    if let Some(diagnostic) = diagnostic.as_ref() {
        record_matched_diagnostic(diagnostic);
    } else if !over_cap
        && let Some(kind) = unmatched_diagnosable_kind(
            &response_fingerprint,
            status_code,
            status_ok,
            !decoded.is_empty(),
        )
    {
        counter!(
            metric_names::HTTP_ACCESS_DIAGNOSTICS,
            "outcome" => "unmatched",
            "kind" => kind,
            "provider" => "unknown",
            "confidence" => "unknown",
        )
        .increment(1);
    }

    let probe = capture.then(|| HttpProbe {
        response_headers_preview: headers_preview.unwrap_or_default(),
        // Nothing was kept; a blank snippet would read as an empty response.
        response_body_snippet: (!over_cap).then(|| snapshot_body(&decoded)),
    });

    (
        CheckResult {
            target_id,
            org_id,
            timestamp: started_at,
            status,
            duration_ms,
            dns_ms: Some(timings.dns_ms),
            connect_ms: Some(timings.connect_ms),
            tls_ms: timings.tls_ms,
            ttfb_ms: Some(ttfb_ms),
            response_code: Some(status_code),
            response_size: size,
            diagnostic,
            error,
        },
        probe,
    )
}

/// What a vendor-namespaced mitigation header declared. `Other` keeps an
/// unrecognised action out of the detectors while still counting as drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VercelMitigation {
    Challenge,
    Deny,
    Other,
}

/// Non-sensitive signals, held only until the result is classified, and kept
/// as decisions rather than copied values so the passing majority never
/// allocates. A provider alone never attributes.
#[derive(Debug, Default)]
struct ResponseFingerprint {
    akamai_edge: bool,
    vercel_edge: bool,
    aws_waf_challenge: bool,
    cloudflare_challenge: bool,
    azure_reference: bool,
    datadome_signal: bool,
    vercel_mitigation: Option<VercelMitigation>,
    vercel_challenge_token: bool,
    cloudflare_edge: bool,
    cloudflare_ray: bool,
    /// On relayed responses, absent on pages Cloudflare writes itself.
    /// Measured, not documented — hence the body markers below.
    cloudflare_relayed: bool,
}

impl ResponseFingerprint {
    fn from_headers(headers: &hyper::HeaderMap) -> Self {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let server_is = |want: &str| header("server").is_some_and(|v| v.eq_ignore_ascii_case(want));
        Self {
            akamai_edge: server_is("AkamaiGHost"),
            vercel_edge: server_is("Vercel"),
            aws_waf_challenge: header("x-amzn-waf-action").is_some_and(|v| {
                v.eq_ignore_ascii_case("challenge") || v.eq_ignore_ascii_case("captcha")
            }),
            cloudflare_challenge: header("cf-mitigated")
                .is_some_and(|v| v.eq_ignore_ascii_case("challenge")),
            azure_reference: headers.contains_key("x-azure-ref"),
            datadome_signal: headers
                .keys()
                .any(|name| name.as_str().starts_with("x-datadome")),
            vercel_mitigation: header("x-vercel-mitigated").map(|action| {
                if action.eq_ignore_ascii_case("challenge") {
                    VercelMitigation::Challenge
                } else if action.eq_ignore_ascii_case("deny") {
                    VercelMitigation::Deny
                } else {
                    VercelMitigation::Other
                }
            }),
            vercel_challenge_token: headers.contains_key("x-vercel-challenge-token"),
            cloudflare_edge: server_is("cloudflare"),
            cloudflare_ray: headers.contains_key("cf-ray"),
            cloudflare_relayed: headers.contains_key("cf-cache-status"),
        }
    }
}

/// 502 is here because a tunnel with a dead service returns it — and because
/// it is a common honest origin response, the page decides, never the code.
const CLOUDFLARE_ORIGIN_ERROR_CODES: &[u16] = &[502, 520, 521, 522, 523, 524, 530];

const CLOUDFLARE_TUNNEL_ERROR: u16 = 1033;

fn detect_access_interference(
    fingerprint: &ResponseFingerprint,
    decoded_body: &[u8],
) -> Option<CheckDiagnostic> {
    // AWS documents this header for its Challenge and CAPTCHA actions. A plain
    // CloudFront 403 does not carry it and stays unattributed.
    if fingerprint.aws_waf_challenge {
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::High,
            Some(EdgeProvider::AwsWaf),
            vec![DiagnosticEvidence::ChallengeHeader],
        ));
    }

    if fingerprint.cloudflare_challenge {
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::High,
            Some(EdgeProvider::Cloudflare),
            vec![DiagnosticEvidence::ChallengeHeader],
        ));
    }

    let prefix = &decoded_body[..decoded_body.len().min(DIAGNOSTIC_BODY_BYTES)];
    let body = String::from_utf8_lossy(prefix).to_ascii_lowercase();
    let access_denied = body.contains("access denied");
    let akamai_reference =
        body.contains("errors.edgesuite.net") || body.contains("errors&#46;edgesuite&#46;net");
    if fingerprint.akamai_edge && access_denied && akamai_reference {
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::High,
            Some(EdgeProvider::Akamai),
            vec![
                DiagnosticEvidence::EdgeServer,
                DiagnosticEvidence::BlockPage,
                DiagnosticEvidence::ReferenceId,
            ],
        ));
    }

    if fingerprint.azure_reference && body.contains("the request is blocked") {
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::High,
            Some(EdgeProvider::AzureFrontDoor),
            vec![
                DiagnosticEvidence::BlockPage,
                DiagnosticEvidence::ReferenceId,
            ],
        ));
    }

    let datadome_page = body.contains("captcha-delivery.com")
        || (body.contains("datadome")
            && (body.contains("captcha") || body.contains("device check")));
    if fingerprint.datadome_signal && datadome_page {
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::High,
            Some(EdgeProvider::DataDome),
            vec![
                DiagnosticEvidence::ChallengeHeader,
                DiagnosticEvidence::BlockPage,
            ],
        ));
    }

    // Vercel documents the behaviour but not a stable detector contract, so
    // three signals and medium confidence: a copied header must not name it.
    let vercel_page = body.contains("vercel security checkpoint");
    if fingerprint.vercel_mitigation == Some(VercelMitigation::Challenge)
        && fingerprint.vercel_edge
        && (fingerprint.vercel_challenge_token || vercel_page)
    {
        let mut evidence = vec![
            DiagnosticEvidence::ChallengeHeader,
            DiagnosticEvidence::EdgeServer,
        ];
        if vercel_page {
            evidence.push(DiagnosticEvidence::BlockPage);
        }
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::Medium,
            Some(EdgeProvider::Vercel),
            evidence,
        ));
    }

    // No page to corroborate, so the vendor header stands alone as
    // `cf-mitigated` does. A fronting proxy rewrites `server`, not the header.
    if fingerprint.vercel_mitigation == Some(VercelMitigation::Deny) {
        let mut evidence = vec![DiagnosticEvidence::MitigationHeader];
        if fingerprint.vercel_edge {
            evidence.push(DiagnosticEvidence::EdgeServer);
        }
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::Medium,
            Some(EdgeProvider::Vercel),
            evidence,
        ));
    }

    // Several appliances ship this page, so no vendor is named. The colon
    // matters: a bare "support id" is ordinary English on a help page.
    let quotes_support_id = body.contains("support id:") || body.contains("support id is:");
    if body.contains("the requested url was rejected") && quotes_support_id {
        return Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::Medium,
            None,
            vec![
                DiagnosticEvidence::BlockPage,
                DiagnosticEvidence::ReferenceId,
            ],
        ));
    }

    None
}

/// Edge marker, no relay marker, on a code the edge serves itself.
fn cloudflare_error_page_headers(fingerprint: &ResponseFingerprint, status_code: u16) -> bool {
    fingerprint.cloudflare_edge
        && fingerprint.cloudflare_ray
        && !fingerprint.cloudflare_relayed
        && CLOUDFLARE_ORIGIN_ERROR_CODES.contains(&status_code)
}

/// The code proves nothing: 5xx above 511 is unassigned and other vendors
/// emit 530. Only Cloudflare's own page separates them.
fn detect_origin_unreachable(
    fingerprint: &ResponseFingerprint,
    decoded_body: &[u8],
    status_code: u16,
) -> Option<CheckDiagnostic> {
    if !cloudflare_error_page_headers(fingerprint, status_code) {
        return None;
    }

    let prefix = &decoded_body[..decoded_body.len().min(DIAGNOSTIC_BODY_BYTES)];
    let body = String::from_utf8_lossy(prefix).to_ascii_lowercase();

    // The header pair does not prove Cloudflare wrote the page: a Worker
    // carries it too. No page, no diagnostic.
    if !body.contains("cf-error-details") {
        return None;
    }
    let origin_error = parse_cloudflare_error_code(&body);
    if origin_error.is_none() && !body.contains(&format!("error code {status_code}")) {
        return None;
    }

    Some(CheckDiagnostic::origin_unreachable(
        vec![
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::ReferenceId,
            DiagnosticEvidence::OriginErrorCode,
        ],
        origin_error == Some(CLOUDFLARE_TUNNEL_ERROR),
    ))
}

/// The page carries unrelated four-digit numbers, so read only the one the
/// `errorCode:` label names.
fn parse_cloudflare_error_code(lowercased_body: &str) -> Option<u16> {
    let rest = lowercased_body.split_once("errorcode:")?.1.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Access reads headers and pages, origin reads a status the edge serves
/// itself, so the order is determinism not precedence.
fn detect_diagnostic(
    fingerprint: &ResponseFingerprint,
    decoded_body: &[u8],
    status_code: u16,
) -> Option<CheckDiagnostic> {
    detect_access_interference(fingerprint, decoded_body)
        .or_else(|| detect_origin_unreachable(fingerprint, decoded_body, status_code))
}

fn record_matched_diagnostic(diagnostic: &CheckDiagnostic) {
    counter!(
        metric_names::HTTP_ACCESS_DIAGNOSTICS,
        "outcome" => "matched",
        "kind" => diagnostic.kind.as_str(),
        "provider" => diagnostic.provider.map(EdgeProvider::as_str).unwrap_or("unknown"),
        "confidence" => diagnostic.confidence.as_str(),
    )
    .increment(1);
}

/// Only failures that cleared the detector's own gate and stayed unattributed.
/// A relayed origin 5xx is correct, not drift, and would swamp the ratio.
fn unmatched_diagnosable_kind(
    fingerprint: &ResponseFingerprint,
    status_code: u16,
    status_ok: bool,
    body_available: bool,
) -> Option<&'static str> {
    if status_ok {
        return None;
    }
    // Detectable without a body, whatever status the vendor paired it with.
    if status_code == 403 || fingerprint.vercel_mitigation.is_some() {
        return Some(CheckDiagnosticKind::AccessInterference.as_str());
    }
    // A HEAD monitor never carries the page: a permanent miss, not drift.
    if body_available && cloudflare_error_page_headers(fingerprint, status_code) {
        return Some(CheckDiagnosticKind::OriginUnreachable.as_str());
    }
    None
}

/// First N unique-by-name headers, sensitive values redacted. Multi-value
/// names collapse to one entry so the cap is not burnt on a repeat.
fn snapshot_headers(headers: &hyper::HeaderMap) -> Vec<crate::domain::agent_wire::HeaderPreview> {
    use crate::domain::agent_wire::HeaderPreview;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<HeaderPreview> = Vec::with_capacity(PROBE_HEADERS_MAX);
    for (k, v) in headers.iter() {
        let name = k.as_str().to_owned();
        let lower = name.to_ascii_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        let value = if PROBE_REDACT_HEADERS.iter().any(|r| *r == lower) {
            "***REDACTED***".to_owned()
        } else {
            v.to_str().unwrap_or("<non-ascii>").to_owned()
        };
        out.push(HeaderPreview { name, value });
        if out.len() >= PROBE_HEADERS_MAX {
            break;
        }
    }
    out
}

/// First [`PROBE_BODY_SNIPPET_BYTES`] of the decoded body, lossy UTF-8, `…`
/// when cut so the hub's scrub can see a secret may straddle the end. The
/// cut snaps back to a codepoint boundary so a multi-byte char is not chopped.
fn snapshot_body(decoded: &[u8]) -> String {
    if decoded.len() <= PROBE_BODY_SNIPPET_BYTES {
        return String::from_utf8_lossy(decoded).into_owned();
    }
    let mut end = PROBE_BODY_SNIPPET_BYTES;
    while end > 0 && (decoded[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    let mut out = String::from_utf8_lossy(&decoded[..end]).into_owned();
    out.push('…');
    out
}

fn build_request(
    method: HttpMethod,
    uri: &Uri,
    check: &HttpCheck,
    body: &Option<String>,
    include_auth: bool,
    alpn_h2: bool,
    clients: &HttpClients,
) -> Result<Request<Full<HBytes>>, hyper::http::Error> {
    let body_bytes: HBytes = match body {
        Some(b) => HBytes::from(b.clone().into_bytes()),
        None => HBytes::new(),
    };

    let (target, host_header) = request_target(uri, alpn_h2)?;

    let mut builder = Request::builder().method(map_method(method)).uri(target);

    // Replaces rather than joins: two User-Agent lines are malformed, and
    // overriding ours is the documented way past an edge that blocks it.
    for (name, default) in [
        (USER_AGENT, clients.user_agent()),
        // Sending none reads as a scripted client; `*/*` steers nothing.
        (ACCEPT, "*/*"),
    ] {
        if !overrides_header(check, name.as_str()) {
            builder = builder.header(name, default);
        }
    }
    // Not overridable: advertising a codec `decode_body` cannot read would
    // return bytes no assertion could match.
    builder = builder.header(ACCEPT_ENCODING, "gzip, br");

    let bodyless = body.is_none();
    let user_set_host = check.headers.keys().any(|k| k.eq_ignore_ascii_case("host"));
    if let Some(host) = host_header.filter(|_| !user_set_host) {
        builder = builder.header(HOST, host);
    }
    // The dedicated field wins over a raw `Authorization` row so the probe
    // never sends two.
    let field_authz = include_auth
        .then(|| {
            check
                .bearer_token
                .as_ref()
                .and_then(|t| HeaderValue::try_from(format!("Bearer {t}")).ok())
                .or_else(|| {
                    check.basic_auth.as_ref().and_then(|(user, pass)| {
                        let encoded = BASE64_STANDARD.encode(format!("{user}:{pass}"));
                        HeaderValue::try_from(format!("Basic {encoded}")).ok()
                    })
                })
        })
        .flatten();
    for (k, v) in &check.headers {
        if field_authz.is_some() && k.eq_ignore_ascii_case("authorization") {
            continue;
        }
        // A credential must not follow a hostile `Location` off-origin.
        if !include_auth
            && PROBE_REDACT_HEADERS
                .iter()
                .any(|s| k.eq_ignore_ascii_case(s))
        {
            continue;
        }
        // A caller-set codec we cannot decode fails every body assertion.
        if k.eq_ignore_ascii_case("accept-encoding") {
            continue;
        }
        // Framing a body that no longer exists is malformed, and strict
        // origins reject it — a spurious Down on canonical redirects.
        if bodyless
            && (k.eq_ignore_ascii_case("content-type")
                || k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("transfer-encoding"))
        {
            continue;
        }
        if let (Ok(name), Ok(val)) = (HeaderName::try_from(k.as_str()), HeaderValue::try_from(v)) {
            builder = builder.header(name, val);
        }
    }
    if let Some(val) = field_authz {
        builder = builder.header(AUTHORIZATION, val);
    }

    builder.body(Full::new(body_bytes))
}

/// A value hyper rejects is no replacement: the loop drops it and the request
/// would carry neither.
fn overrides_header(check: &HttpCheck, name: &str) -> bool {
    check
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(name) && HeaderValue::try_from(v).is_ok())
}

/// hyper's low-level http/1 client writes the URI verbatim, so an absolute one
/// leaves as absolute-form and strict origins answer 404. http/1 gets
/// origin-form plus an explicit Host; h2 keeps the authority.
fn request_target(uri: &Uri, alpn_h2: bool) -> Result<(Uri, Option<String>), hyper::http::Error> {
    if alpn_h2 {
        return Ok((uri.clone(), None));
    }
    let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    Ok((Uri::try_from(path)?, host_header_value(uri)))
}

/// Authority host plus a non-default port. Userinfo never belongs in `Host`.
fn host_header_value(uri: &Uri) -> Option<String> {
    let host = uri.host()?;
    let default_port = if uri.scheme_str() == Some("http") {
        80
    } else {
        443
    };
    Some(match uri.port_u16() {
        Some(port) if port != default_port => format!("{host}:{port}"),
        _ => host.to_owned(),
    })
}

/// `0` disables following; an enabled check with `max_redirects == 0` falls
/// back to [`MAX_REDIRECT_HOPS`], which also clamps any configured value.
fn effective_max_redirects(check: &HttpCheck) -> u8 {
    if !check.follow_redirects {
        return 0;
    }
    match check.max_redirects {
        0 => MAX_REDIRECT_HOPS,
        n => n.min(MAX_REDIRECT_HOPS),
    }
}

fn is_redirect(code: u16) -> bool {
    matches!(code, 301 | 302 | 303 | 307 | 308)
}

/// Not interchangeable: our cap is a decision, malformed bytes are a fault,
/// and only the caller knows whether the body was needed.
#[derive(Debug, PartialEq, Eq)]
enum DecodeError {
    OverCap,
    Malformed,
}

fn decode_body(raw: &HBytes, encoding: Option<&str>) -> std::result::Result<Bytes, DecodeError> {
    match encoding {
        // Read +1 past the cap so a body that exactly fills the budget reads
        // as over rather than truncating to a misleading length.
        Some("gzip") => decode_capped(GzDecoder::new(raw.as_ref()), raw.len()),
        Some("br") => decode_capped(brotli::Decompressor::new(raw.as_ref(), 4096), raw.len()),
        _ => Ok(raw.clone()),
    }
}

fn decode_capped<R: std::io::Read>(
    reader: R,
    raw_len: usize,
) -> std::result::Result<Bytes, DecodeError> {
    use std::io::Read;
    // Pre-size for the typical 4× expansion ratio without exceeding the cap.
    let capacity = raw_len.saturating_mul(4).min(MAX_DECODED_BODY_BYTES);
    let mut out = Vec::with_capacity(capacity);
    reader
        .take((MAX_DECODED_BODY_BYTES as u64) + 1)
        .read_to_end(&mut out)
        .map_err(|_| DecodeError::Malformed)?;
    if out.len() > MAX_DECODED_BODY_BYTES {
        return Err(DecodeError::OverCap);
    }
    Ok(Bytes::from(out))
}

fn map_method(m: HttpMethod) -> Method {
    match m {
        HttpMethod::Get => Method::GET,
        HttpMethod::Head => Method::HEAD,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Options => Method::OPTIONS,
    }
}

/// RFC 9110 allows only delta-seconds or an HTTP-date here. Anything else is
/// untrusted input that a markdown-flavoured notifier would render verbatim.
fn sanitised_retry_after(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && raw.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b' ' | b',' | b':' | b'.' | b'+' | b'-')
        })
}

/// Back-pressure is not an outage, so 429/503 maps to `Degraded` and opens no
/// incident. An `expected_status` that accepts them is honoured first.
fn classify_outcome(
    status_code: u16,
    status_ok: bool,
    body_ok: bool,
    retry_after_raw: Option<&str>,
) -> (CheckStatus, Option<String>) {
    if !status_ok && matches!(status_code, 429 | 503) {
        let detail = retry_after_raw
            .map(|v| format!(" (Retry-After: {v})"))
            .unwrap_or_default();
        return (
            CheckStatus::Degraded,
            Some(format!("rate-limited {status_code}{detail}")),
        );
    }
    if !status_ok {
        return (
            CheckStatus::Down,
            Some(format!("unexpected status {status_code}")),
        );
    }
    if !body_ok {
        return (CheckStatus::Down, Some("body match failed".to_string()));
    }
    (CheckStatus::Up, None)
}

fn match_status(code: u16, expected: &ExpectedStatus) -> bool {
    match expected {
        ExpectedStatus::Exact(c) => code == *c,
        ExpectedStatus::Range { min, max } => code >= *min && code <= *max,
        ExpectedStatus::OneOf(list) => list.contains(&code),
    }
}

/// The row keeps the code; the full chain goes to the agent log.
fn request_failure(target_id: Uuid, hop: u8, err: hyper::Error) -> Cow<'static, str> {
    let reason = classify_hyper_error(&err);
    tracing::debug!(
        target_id = %target_id,
        hop,
        reason = %reason,
        error = %error_chain(err),
        "http request failed"
    );
    reason
}

/// Post-handshake only: stable codes for the common shapes, `transport: <root cause>` otherwise.
fn classify_hyper_error(err: &hyper::Error) -> Cow<'static, str> {
    if has_timeout_in_chain(err) {
        return "timeout".into();
    }
    if err.is_incomplete_message() {
        return "closed before response".into();
    }
    if err.is_parse_too_large() {
        return "response headers too large".into();
    }
    if err.is_parse() {
        return "invalid response".into();
    }
    let mut root: &(dyn std::error::Error + 'static) = err;
    for cause in std::iter::successors(Some(root), |e| e.source()) {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
                    return "reset before response".into();
                }
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe => {
                    return "closed before response".into();
                }
                _ => {}
            }
        }
        if let Some(h2) = cause.downcast_ref::<h2::Error>()
            && let Some(reason) = h2.reason()
        {
            // h2 signs what it detects itself: a header block over our limit,
            // or malformed frames.
            return if h2.is_library() {
                format!("h2 rejected locally: {reason:?}").into()
            } else {
                h2_reason_code(reason)
            };
        }
        root = cause;
    }
    format!("transport: {root}").into()
}

fn h2_reason_code(reason: h2::Reason) -> Cow<'static, str> {
    match reason {
        // A graceful GOAWAY before our stream was answered.
        h2::Reason::NO_ERROR => "closed before response".into(),
        h2::Reason::PROTOCOL_ERROR => "h2 protocol error".into(),
        h2::Reason::INTERNAL_ERROR => "h2 internal error".into(),
        h2::Reason::REFUSED_STREAM => "h2 stream refused".into(),
        h2::Reason::CANCEL => "h2 stream cancelled".into(),
        h2::Reason::ENHANCE_YOUR_CALM => "h2 enhance your calm".into(),
        other => format!("h2 {other:?}").into(),
    }
}

// Narrow on purpose: a bare "timeout" would match `connect_timeout=…` in a
// config-field error and flip a connect failure to a timeout.
fn has_timeout_in_chain(err: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(err), |e| e.source()).any(|e| {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::TimedOut
        {
            return true;
        }
        let s = e.to_string().to_ascii_lowercase();
        s.contains("timed out") || s.contains("deadline")
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use url::Url;

    use super::*;
    use crate::domain::DiagnosticRemediation;

    fn check(url: &str, follow: bool, max: u8) -> HttpCheck {
        HttpCheck {
            url: Url::parse(url).unwrap(),
            method: HttpMethod::Get,
            timeout: Duration::from_secs(3),
            follow_redirects: follow,
            max_redirects: max,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: Default::default(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        }
    }

    #[test]
    fn body_snippet_marks_its_cut() {
        assert_eq!(snapshot_body(b"ok"), "ok");
        let long = "é".repeat(PROBE_BODY_SNIPPET_BYTES);
        let cut = snapshot_body(long.as_bytes());
        assert!(cut.ends_with('…'));
        assert!(
            cut.len() <= PROBE_BODY_SNIPPET_BYTES + '…'.len_utf8(),
            "{}",
            cut.len()
        );
        assert!(!cut.contains('\u{fffd}'));
    }

    #[test]
    fn effective_budget_off_when_not_following() {
        assert_eq!(effective_max_redirects(&check("https://x/", false, 5)), 0);
    }

    #[test]
    fn effective_budget_falls_back_and_clamps() {
        assert_eq!(
            effective_max_redirects(&check("https://x/", true, 0)),
            MAX_REDIRECT_HOPS
        );
        assert_eq!(effective_max_redirects(&check("https://x/", true, 3)), 3);
        assert_eq!(
            effective_max_redirects(&check("https://x/", true, 250)),
            MAX_REDIRECT_HOPS
        );
    }

    #[test]
    fn sanitised_retry_after_rejects_markdown_injection() {
        assert!(sanitised_retry_after("30"));
        assert!(sanitised_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"));
        assert!(!sanitised_retry_after("<https://evil.example|free>"));
        assert!(!sanitised_retry_after("*bold*"));
        assert!(!sanitised_retry_after(""));
        assert!(!sanitised_retry_after(&"a".repeat(65)));
    }

    /// Verbatim from a live tunnel, so a template change breaks the test.
    const CF_ORIGIN_DOWN_502: &str =
        include_str!("../../tests/fixtures/cloudflare/origin_down_502.html");
    const CF_TUNNEL_DOWN_530: &str =
        include_str!("../../tests/fixtures/cloudflare/tunnel_down_530.html");

    const CF_UP_200_HEADERS: &str =
        include_str!("../../tests/fixtures/cloudflare/origin_up_200.headers");
    const CF_ORIGIN_DOWN_502_HEADERS: &str =
        include_str!("../../tests/fixtures/cloudflare/origin_down_502.headers");
    const CF_TUNNEL_DOWN_530_HEADERS: &str =
        include_str!("../../tests/fixtures/cloudflare/tunnel_down_530.headers");

    fn captured_fingerprint(raw: &str) -> ResponseFingerprint {
        let mut headers = hyper::HeaderMap::new();
        for line in raw.lines().skip(1).filter(|line| !line.trim().is_empty()) {
            let (name, value) = line.split_once(':').expect("header line");
            headers.insert(
                hyper::header::HeaderName::from_bytes(name.trim().as_bytes()).expect("name"),
                hyper::header::HeaderValue::from_str(value.trim()).expect("value"),
            );
        }
        ResponseFingerprint::from_headers(&headers)
    }

    /// `cf-cache-status` is undocumented, so pin it to what the edge sent
    /// rather than to a hand-written header set that restates the guess.
    #[test]
    fn the_relay_marker_holds_against_captured_responses() {
        assert!(
            captured_fingerprint(CF_UP_200_HEADERS).cloudflare_relayed,
            "a proxied 200 carries cf-cache-status"
        );
        for raw in [CF_ORIGIN_DOWN_502_HEADERS, CF_TUNNEL_DOWN_530_HEADERS] {
            let fingerprint = captured_fingerprint(raw);
            assert!(fingerprint.cloudflare_edge && fingerprint.cloudflare_ray);
            assert!(
                !fingerprint.cloudflare_relayed,
                "a Cloudflare-authored error page omits cf-cache-status"
            );
        }
    }

    #[test]
    fn origin_detector_matches_the_captured_responses_end_to_end() {
        let tunnel = detect_origin_unreachable(
            &captured_fingerprint(CF_TUNNEL_DOWN_530_HEADERS),
            CF_TUNNEL_DOWN_530.as_bytes(),
            530,
        )
        .expect("captured tunnel failure");
        assert_eq!(tunnel.kind, CheckDiagnosticKind::OriginTunnelDown);

        let origin = detect_origin_unreachable(
            &captured_fingerprint(CF_ORIGIN_DOWN_502_HEADERS),
            CF_ORIGIN_DOWN_502.as_bytes(),
            502,
        )
        .expect("captured origin failure");
        assert_eq!(origin.kind, CheckDiagnosticKind::OriginUnreachable);

        // The proxied 200's headers must not attribute even on an error code.
        assert!(
            detect_origin_unreachable(
                &captured_fingerprint(CF_UP_200_HEADERS),
                CF_TUNNEL_DOWN_530.as_bytes(),
                530
            )
            .is_none()
        );
    }

    fn cloudflare_headers(relayed: bool, ray: bool) -> ResponseFingerprint {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "server",
            hyper::header::HeaderValue::from_static("cloudflare"),
        );
        if ray {
            headers.insert(
                "cf-ray",
                hyper::header::HeaderValue::from_static("a340f0bcd922fd58-LCA"),
            );
        }
        if relayed {
            headers.insert(
                "cf-cache-status",
                hyper::header::HeaderValue::from_static("DYNAMIC"),
            );
        }
        ResponseFingerprint::from_headers(&headers)
    }

    #[test]
    fn origin_detector_names_a_downed_tunnel() {
        let hit = detect_origin_unreachable(
            &cloudflare_headers(false, true),
            CF_TUNNEL_DOWN_530.as_bytes(),
            530,
        )
        .expect("captured tunnel error page");

        assert_eq!(hit.kind, CheckDiagnosticKind::OriginTunnelDown);
        assert_eq!(hit.provider, Some(EdgeProvider::Cloudflare));
        assert_eq!(hit.confidence, DiagnosticConfidence::High);
        assert_eq!(
            hit.evidence,
            vec![
                DiagnosticEvidence::EdgeServer,
                DiagnosticEvidence::ReferenceId,
                DiagnosticEvidence::OriginErrorCode,
            ]
        );
        assert!(
            hit.remediations
                .contains(&DiagnosticRemediation::VerifyEdgeTunnel)
        );
    }

    #[test]
    fn origin_detector_keeps_the_tunnel_arm_off_a_live_tunnel() {
        // Tunnel up, service dead: naming the tunnel would misdirect.
        let hit = detect_origin_unreachable(
            &cloudflare_headers(false, true),
            CF_ORIGIN_DOWN_502.as_bytes(),
            502,
        )
        .expect("captured 502 error page");

        assert_eq!(hit.kind, CheckDiagnosticKind::OriginUnreachable);
        assert_eq!(hit.confidence, DiagnosticConfidence::High);
        assert_eq!(
            hit.remediations,
            vec![DiagnosticRemediation::VerifyOriginReachable]
        );
    }

    #[test]
    fn origin_detector_leaves_a_relayed_origin_error_alone() {
        // Load-bearing negative: blaming the edge for a customer's own 502
        // is worse than saying nothing.
        assert!(
            detect_origin_unreachable(
                &cloudflare_headers(true, true),
                b"<html>application error</html>",
                502
            )
            .is_none()
        );
    }

    #[test]
    fn origin_detector_requires_both_edge_markers() {
        assert!(
            detect_origin_unreachable(
                &cloudflare_headers(false, false),
                CF_TUNNEL_DOWN_530.as_bytes(),
                530
            )
            .is_none()
        );

        // 530 is not Cloudflare-exclusive.
        let mut headers = hyper::HeaderMap::new();
        headers.insert("server", hyper::header::HeaderValue::from_static("nginx"));
        assert!(
            detect_origin_unreachable(&ResponseFingerprint::from_headers(&headers), b"", 530)
                .is_none()
        );
    }

    #[test]
    fn origin_detector_will_not_attribute_on_headers_alone() {
        // A Worker carries the same edge markers. No page, no diagnostic.
        assert!(detect_origin_unreachable(&cloudflare_headers(false, true), b"", 522).is_none());
        assert!(
            detect_origin_unreachable(
                &cloudflare_headers(false, true),
                b"<html>worker threw</html>",
                502
            )
            .is_none()
        );
    }

    #[test]
    fn origin_detector_ignores_codes_outside_the_family() {
        // 525/526 are origin-side too, but their fix is a certificate change.
        for code in [525, 526, 200, 404, 503] {
            assert!(
                detect_origin_unreachable(
                    &cloudflare_headers(false, true),
                    CF_TUNNEL_DOWN_530.as_bytes(),
                    code
                )
                .is_none(),
                "{code} must not be attributed"
            );
        }
    }

    #[test]
    fn cloudflare_error_code_reads_the_labelled_one() {
        assert_eq!(
            parse_cloudflare_error_code(&CF_TUNNEL_DOWN_530.to_ascii_lowercase()),
            Some(CLOUDFLARE_TUNNEL_ERROR)
        );
        assert_eq!(parse_cloudflare_error_code("no code here"), None);
        assert_eq!(parse_cloudflare_error_code("errorcode: "), None);
    }

    #[test]
    fn unmatched_counter_stays_off_correct_non_attributions() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("server", hyper::header::HeaderValue::from_static("nginx"));
        let plain = ResponseFingerprint::from_headers(&headers);
        assert_eq!(unmatched_diagnosable_kind(&plain, 502, false, true), None);

        // Relayed: Cloudflare in front, but the origin wrote the response.
        assert_eq!(
            unmatched_diagnosable_kind(&cloudflare_headers(true, true), 530, false, true),
            None
        );

        // An accepted 5xx is not a failure to attribute.
        assert_eq!(
            unmatched_diagnosable_kind(&cloudflare_headers(false, true), 530, true, true),
            None
        );

        // A HEAD probe can never carry the page: a permanent miss, not drift.
        assert_eq!(
            unmatched_diagnosable_kind(&cloudflare_headers(false, true), 530, false, false),
            None
        );

        // Headers say Cloudflare's own page, no signature matched: the drift.
        assert_eq!(
            unmatched_diagnosable_kind(&cloudflare_headers(false, true), 530, false, true),
            Some(CheckDiagnosticKind::OriginUnreachable.as_str())
        );
    }

    #[test]
    fn dispatcher_keeps_access_interference_reachable() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "cf-mitigated",
            hyper::header::HeaderValue::from_static("challenge"),
        );
        let hit = detect_diagnostic(&ResponseFingerprint::from_headers(&headers), b"", 403)
            .expect("challenge header");
        assert_eq!(hit.kind, CheckDiagnosticKind::AccessInterference);
    }

    #[test]
    fn access_detector_matches_only_documented_aws_waf_actions() {
        for action in ["challenge", "captcha", "ChAlLeNgE"] {
            let mut headers = hyper::HeaderMap::new();
            headers.insert(
                "x-amzn-waf-action",
                hyper::header::HeaderValue::from_str(action).unwrap(),
            );
            let fingerprint = ResponseFingerprint::from_headers(&headers);
            let hit =
                detect_access_interference(&fingerprint, b"").expect("documented AWS WAF action");
            assert_eq!(hit.provider, Some(EdgeProvider::AwsWaf));
            assert_eq!(hit.confidence, DiagnosticConfidence::High);
            assert_eq!(hit.evidence, vec![DiagnosticEvidence::ChallengeHeader]);
        }

        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "x-amzn-waf-action",
            hyper::header::HeaderValue::from_static("block"),
        );
        assert!(
            detect_access_interference(&ResponseFingerprint::from_headers(&headers), b"").is_none()
        );
    }

    fn vercel_mitigated(action: &'static str, server: bool) -> ResponseFingerprint {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "x-vercel-mitigated",
            hyper::header::HeaderValue::from_static(action),
        );
        if server {
            headers.insert("server", hyper::header::HeaderValue::from_static("Vercel"));
        }
        ResponseFingerprint::from_headers(&headers)
    }

    #[test]
    fn vercel_deny_is_attributed_from_its_header_alone() {
        let hit = detect_access_interference(&vercel_mitigated("deny", true), b"")
            .expect("a deny is attributed without a page");
        assert_eq!(hit.provider, Some(EdgeProvider::Vercel));
        assert_eq!(hit.confidence, DiagnosticConfidence::Medium);
        assert_eq!(
            hit.evidence,
            vec![
                DiagnosticEvidence::MitigationHeader,
                DiagnosticEvidence::EdgeServer,
            ]
        );

        // A proxy in front rewrites `server` but relays the vendor header.
        let fronted = detect_access_interference(&vercel_mitigated("deny", false), b"")
            .expect("a fronted deny is still attributed");
        assert_eq!(fronted.provider, Some(EdgeProvider::Vercel));
        assert_eq!(fronted.evidence, vec![DiagnosticEvidence::MitigationHeader]);
    }

    #[test]
    fn an_unknown_vercel_action_is_unattributed_but_counted_as_drift() {
        let fingerprint = vercel_mitigated("some-future-action", true);
        assert!(detect_access_interference(&fingerprint, b"").is_none());
        assert_eq!(
            unmatched_diagnosable_kind(&fingerprint, 429, false, false),
            Some(CheckDiagnosticKind::AccessInterference.as_str()),
            "a mitigation we cannot name is the drift this counter exists for"
        );
    }

    #[test]
    fn access_detector_requires_three_vercel_challenge_signals() {
        let fingerprint = |mitigated: bool, server: bool, token: bool| {
            let mut headers = hyper::HeaderMap::new();
            if mitigated {
                headers.insert(
                    "x-vercel-mitigated",
                    hyper::header::HeaderValue::from_static("challenge"),
                );
            }
            if server {
                headers.insert(
                    hyper::header::SERVER,
                    hyper::header::HeaderValue::from_static("Vercel"),
                );
            }
            if token {
                headers.insert(
                    "x-vercel-challenge-token",
                    hyper::header::HeaderValue::from_static("opaque"),
                );
            }
            ResponseFingerprint::from_headers(&headers)
        };

        let hit = detect_access_interference(&fingerprint(true, true, true), b"")
            .expect("three Vercel response signals");
        assert_eq!(hit.provider, Some(EdgeProvider::Vercel));
        assert_eq!(hit.confidence, DiagnosticConfidence::Medium);

        for incomplete in [
            fingerprint(true, false, true),
            fingerprint(true, true, false),
            fingerprint(false, true, true),
        ] {
            assert!(
                detect_access_interference(&incomplete, b"ordinary application page").is_none()
            );
        }

        let page_hit = detect_access_interference(
            &fingerprint(true, true, false),
            b"<title>Vercel Security Checkpoint</title>",
        )
        .expect("checkpoint page is the third signal");
        assert_eq!(page_hit.provider, Some(EdgeProvider::Vercel));
        assert!(page_hit.evidence.contains(&DiagnosticEvidence::BlockPage));
    }

    #[test]
    fn access_detector_matches_azure_only_with_reference_and_block_page() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "x-azure-ref",
            hyper::header::HeaderValue::from_static("ref"),
        );
        let fingerprint = ResponseFingerprint::from_headers(&headers);

        let hit = detect_access_interference(&fingerprint, b"The request is blocked.")
            .expect("Azure block signature");
        assert_eq!(hit.provider, Some(EdgeProvider::AzureFrontDoor));
        assert_eq!(hit.confidence, DiagnosticConfidence::High);

        assert!(detect_access_interference(&fingerprint, b"application says forbidden").is_none());
    }

    /// Nothing corroborates this branch, and every region fetches the same
    /// body, so a loose match reaches the customer's notification as the cause.
    #[test]
    fn access_detector_needs_a_quoted_support_id_not_the_bare_words() {
        let bare = ResponseFingerprint::default();

        // The other rendering of the same page; `..._keeps_shared_appliance_page_generic`
        // covers the bare `Support ID:` form.
        assert!(
            detect_access_interference(
                &bare,
                b"the requested url was rejected. please consult with your administrator. \
                  your support id is: 1234567890",
            )
            .is_some(),
            "an appliance page that spells out the label still matches"
        );

        assert!(
            detect_access_interference(
                &bare,
                b"if the requested url was rejected, quote the support id from the error \
                  page when you open a ticket",
            )
            .is_none(),
            "a customer's own error-help page mentions the id without presenting one"
        );
    }

    #[test]
    fn access_detector_matches_datadome_only_with_header_and_page() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "x-datadome",
            hyper::header::HeaderValue::from_static("protected"),
        );
        let fingerprint = ResponseFingerprint::from_headers(&headers);

        let hit = detect_access_interference(
            &fingerprint,
            b"Complete the device check at captcha-delivery.com",
        )
        .expect("DataDome block signature");
        assert_eq!(hit.provider, Some(EdgeProvider::DataDome));
        assert_eq!(hit.confidence, DiagnosticConfidence::High);

        assert!(
            detect_access_interference(
                &ResponseFingerprint::default(),
                b"Complete the device check at captcha-delivery.com",
            )
            .is_none()
        );
    }

    #[test]
    fn access_detector_keeps_shared_appliance_page_generic() {
        let hit = detect_access_interference(
            &ResponseFingerprint::default(),
            b"The requested URL was rejected. Please consult your administrator. Support ID: 42",
        )
        .expect("generic policy block");
        assert_eq!(hit.provider, None);
        assert_eq!(hit.confidence, DiagnosticConfidence::Medium);
    }

    #[test]
    fn access_detector_does_not_scan_past_its_body_budget() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::SERVER,
            hyper::header::HeaderValue::from_static("AkamaiGHost"),
        );
        let fingerprint = ResponseFingerprint::from_headers(&headers);
        let mut body = vec![b'x'; DIAGNOSTIC_BODY_BYTES];
        body.extend_from_slice(b"Access Denied errors.edgesuite.net");

        assert!(detect_access_interference(&fingerprint, &body).is_none());
    }

    #[test]
    fn the_over_cap_error_names_the_cap_it_hit() {
        assert!(
            OVER_CAP_ERROR.contains(&format!("{} MiB", MAX_RAW_BODY_BYTES >> 20)),
            "{OVER_CAP_ERROR} must name the real cap"
        );
    }

    #[test]
    fn unmatched_metric_requires_a_status_mismatched_403() {
        let plain = ResponseFingerprint::default();
        let access = Some(CheckDiagnosticKind::AccessInterference.as_str());
        assert_eq!(unmatched_diagnosable_kind(&plain, 403, false, true), access);
        assert_eq!(
            unmatched_diagnosable_kind(&plain, 403, false, false),
            access,
            "a challenge header is detectable without a body"
        );
        assert_eq!(
            unmatched_diagnosable_kind(&plain, 403, true, true),
            None,
            "an accepted 403 with a failed body assertion is not signature drift"
        );
        assert_eq!(unmatched_diagnosable_kind(&plain, 401, false, true), None);
        assert_eq!(unmatched_diagnosable_kind(&plain, 500, false, true), None);
    }

    #[test]
    fn classify_outcome_marks_429_with_retry_after_as_degraded() {
        let (s, err) = classify_outcome(429, false, true, Some("30"));
        assert!(matches!(s, CheckStatus::Degraded));
        assert_eq!(err.unwrap(), "rate-limited 429 (Retry-After: 30)");
    }

    #[test]
    fn classify_outcome_marks_503_without_retry_after_as_degraded() {
        let (s, err) = classify_outcome(503, false, true, None);
        assert!(matches!(s, CheckStatus::Degraded));
        assert_eq!(err.unwrap(), "rate-limited 503");
    }

    #[test]
    fn classify_outcome_honors_user_expecting_429() {
        let (s, err) = classify_outcome(429, true, true, Some("60"));
        assert!(matches!(s, CheckStatus::Up));
        assert!(err.is_none());
    }

    #[test]
    fn classify_outcome_real_5xx_stays_down() {
        let (s, err) = classify_outcome(500, false, true, None);
        assert!(matches!(s, CheckStatus::Down));
        assert_eq!(err.unwrap(), "unexpected status 500");
    }

    #[test]
    fn classify_outcome_body_mismatch_is_down() {
        let (s, err) = classify_outcome(200, true, false, None);
        assert!(matches!(s, CheckStatus::Down));
        assert_eq!(err.unwrap(), "body match failed");
    }

    #[test]
    fn redirect_status_set() {
        for c in [301, 302, 303, 307, 308] {
            assert!(is_redirect(c));
        }
        for c in [200, 204, 300, 304, 305, 400, 500] {
            assert!(!is_redirect(c));
        }
    }

    #[test]
    fn request_target_http1_is_origin_form_with_host() {
        let uri: Uri = "https://example.com/status?q=1".parse().unwrap();
        let (target, host) = request_target(&uri, false).unwrap();
        assert_eq!(target.to_string(), "/status?q=1");
        assert_eq!(host.as_deref(), Some("example.com"));
    }

    #[test]
    fn request_target_http1_root_path_defaults_to_slash() {
        let uri: Uri = "https://example.com".parse().unwrap();
        let (target, host) = request_target(&uri, false).unwrap();
        assert_eq!(target.to_string(), "/");
        assert_eq!(host.as_deref(), Some("example.com"));
    }

    #[test]
    fn request_target_h2_keeps_absolute_uri_without_host() {
        let uri: Uri = "https://example.com/status".parse().unwrap();
        let (target, host) = request_target(&uri, true).unwrap();
        assert_eq!(target, uri);
        assert!(host.is_none());
    }

    #[test]
    fn host_header_omits_default_port_and_keeps_custom() {
        assert_eq!(
            host_header_value(&"https://example.com/".parse().unwrap()).as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_header_value(&"http://example.com/".parse().unwrap()).as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_header_value(&"https://example.com:8443/".parse().unwrap()).as_deref(),
            Some("example.com:8443")
        );
        assert_eq!(
            host_header_value(&"http://example.com:8080/".parse().unwrap()).as_deref(),
            Some("example.com:8080")
        );
    }

    #[test]
    fn decode_body_passes_through_when_no_encoding() {
        let raw = HBytes::from_static(b"hello world");
        let out = decode_body(&raw, None).expect("identity must succeed");
        assert_eq!(out.as_ref(), b"hello world");
    }

    #[test]
    fn decode_body_rejects_gzip_bomb_over_decoded_cap() {
        use std::io::Write;
        // Tiny on-wire payload that expands past MAX_DECODED_BODY_BYTES.
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let chunk = vec![0u8; 64 * 1024];
        let mut written = 0usize;
        while written <= MAX_DECODED_BODY_BYTES + chunk.len() {
            encoder.write_all(&chunk).unwrap();
            written += chunk.len();
        }
        let compressed = encoder.finish().unwrap();
        let raw = HBytes::from(compressed);
        assert_eq!(
            decode_body(&raw, Some("gzip")),
            Err(DecodeError::OverCap),
            "a bomb is refused by our cap, not mistaken for mangled bytes"
        );
    }

    #[derive(Debug)]
    struct ChainErr {
        msg: &'static str,
        src: Option<Box<dyn std::error::Error + 'static>>,
    }
    impl std::fmt::Display for ChainErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.msg)
        }
    }
    impl std::error::Error for ChainErr {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.src.as_deref()
        }
    }

    #[test]
    fn has_timeout_in_chain_matches_root_message() {
        let e = ChainErr {
            msg: "operation timed out",
            src: None,
        };
        assert!(has_timeout_in_chain(&e));
    }

    #[test]
    fn has_timeout_in_chain_walks_to_source() {
        let leaf = ChainErr {
            msg: "deadline elapsed",
            src: None,
        };
        let root = ChainErr {
            msg: "hyper error",
            src: Some(Box::new(leaf)),
        };
        assert!(has_timeout_in_chain(&root));
    }

    #[test]
    fn has_timeout_in_chain_returns_false_without_marker() {
        let leaf = ChainErr {
            msg: "connection reset",
            src: None,
        };
        let root = ChainErr {
            msg: "transport error",
            src: Some(Box::new(leaf)),
        };
        assert!(!has_timeout_in_chain(&root));
    }

    #[test]
    fn has_timeout_in_chain_does_not_match_config_field_substring() {
        // `connect_timeout=5s` matched the earlier permissive `"timeout"`
        // substring; the narrowed phrase set does not.
        let e = ChainErr {
            msg: "connect_timeout=5s exceeded budget",
            src: None,
        };
        assert!(!has_timeout_in_chain(&e));
    }

    #[test]
    fn has_timeout_in_chain_detects_io_error_kind() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "x");
        let root = ChainErr {
            msg: "outer",
            src: Some(Box::new(io)),
        };
        assert!(has_timeout_in_chain(&root));
    }

    #[test]
    fn decode_body_accepts_gzip_within_cap() {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder
            .write_all(b"compressible compressible compressible")
            .unwrap();
        let compressed = encoder.finish().unwrap();
        let raw = HBytes::from(compressed);
        let out = decode_body(&raw, Some("gzip")).expect("small gzip must succeed");
        assert!(out.starts_with(b"compressible"));
    }

    /// A well-compressing page clears the raw cap and is refused on decode.
    /// Only that distinction lets the caller leave a status-only check alone.
    #[test]
    fn a_well_compressing_oversized_body_is_over_cap_not_malformed() {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder
            .write_all(&vec![b'a'; MAX_DECODED_BODY_BYTES + 1])
            .unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(
            compressed.len() < MAX_RAW_BODY_BYTES,
            "the raw cap must not be what rejects this body"
        );
        assert_eq!(
            decode_body(&HBytes::from(compressed), Some("gzip")),
            Err(DecodeError::OverCap)
        );
    }

    #[test]
    fn mangled_encoding_stays_a_fault() {
        let raw = HBytes::from_static(b"this is not gzip");
        assert_eq!(decode_body(&raw, Some("gzip")), Err(DecodeError::Malformed));
    }

    #[test]
    fn both_read_caps_name_their_real_size() {
        assert!(OVER_CAP_ERROR.contains(&format!("{} MiB", MAX_RAW_BODY_BYTES >> 20)));
        assert!(OVER_DECODED_CAP_ERROR.contains(&format!("{} MiB", MAX_DECODED_BODY_BYTES >> 20)));
        assert_eq!(
            crate::domain::classify_check_error(OVER_DECODED_CAP_ERROR),
            crate::domain::ErrorClass::BodyOverCap,
            "the decoded cap must classify like the raw one"
        );
    }
}
