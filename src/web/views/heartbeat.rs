//! Inbound heartbeat pings. Unauthenticated: holding the token is the proof,
//! same trust model as `/m/{token}` share links. GET and POST both count, so
//! curl and cron stay one-liners.

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use crate::app::AppState;
use crate::auth::sha256_hex;
use crate::domain::{CheckResult, HeartbeatPingRecord, Ping};
use crate::storage::heartbeats::PingAccepted;

/// Excess is drained and dropped, never refused: a 413 on a success ping would
/// page the customer for a job that ran fine.
const BODY_SAMPLE_BYTES: usize = 4096;

/// Past this, draining is a free upload slot.
const BODY_DRAIN_LIMIT: usize = 256 * 1024;

/// Without this a trickled body is a slowloris on an unauthenticated route.
/// The verdict is already written, so a dropped tail costs output only.
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Shorter than the sink's own retry budget, so a wedged store sheds these
/// rather than accumulating a task per signal that changed a verdict.
const PING_RESULT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// First half of the token's SHA-256, so GCRA runs before any database work.
fn token_key(raw: &str) -> u128 {
    let hex = sha256_hex(raw);
    u128::from_str_radix(&hex[..32], 16).unwrap_or(0)
}

pub async fn ping(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: Body,
) -> Response {
    record(&state, token, Ping::SUCCESS, body).await
}

pub async fn ping_signal(
    State(state): State<AppState>,
    Path((token, segment)): Path<(String, String)>,
    body: Body,
) -> Response {
    let Some(parsed) = Ping::parse(&segment) else {
        return (StatusCode::NOT_FOUND, "unknown signal").into_response();
    };
    record(&state, token, parsed, body).await
}

async fn record(state: &AppState, token: String, ping: Ping, body: Body) -> Response {
    if !state.heartbeat_runtime.allow_ping(token_key(&token)) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "1")],
            "slow down",
        )
            .into_response();
    }
    // Unknown, revoked and deleted-org tokens 404 alike (anti-enumeration).
    let accepted = match state
        .heartbeat_store
        .record_signal_by_token(&token, ping.signal, ping.exit_code)
        .await
    {
        Ok(Some(accepted)) => accepted,
        Ok(None) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(err) => {
            tracing::error!(error = %err, "heartbeat ping: record failed");
            return (StatusCode::SERVICE_UNAVAILABLE, "try again").into_response();
        }
    };
    state
        .heartbeat_runtime
        .record(accepted.target_id, accepted.state);

    if accepted.enabled
        && let Some(result) = ping_result(&accepted, ping)
    {
        spawn_result_write(state, result);
    }

    if let Some(sink) = &state.heartbeat_ping_sink {
        sink.write_ping(&HeartbeatPingRecord {
            org_id: accepted.org_id.0,
            target_id: accepted.target_id,
            received_at: accepted.at,
            signal: ping.signal,
            exit_code: ping.exit_code,
            duration_ms: accepted.run_ms,
            body: body_sample(body).await,
        })
        .await;
    }
    tracing::debug!(
        target_id = %accepted.target_id,
        signal = ping.signal.as_str(),
        "heartbeat ping accepted"
    );
    (StatusCode::OK, "ok").into_response()
}

/// What this signal says on its own: the wiring ping's verdict, or a later
/// one that changed the verdict, so a job that recovers is seen when it
/// speaks rather than at the scheduler's next look.
fn ping_result(accepted: &PingAccepted, ping: Ping) -> Option<CheckResult> {
    if accepted.first {
        return crate::worker::heartbeat::first_ping_result(
            accepted.target_id,
            accepted.org_id.0,
            accepted.at,
            ping.signal,
            ping.exit_code,
        );
    }
    let check = accepted.check.as_ref()?;
    crate::worker::heartbeat::transition_result(
        accepted.target_id,
        accepted.org_id.0,
        accepted.at,
        &accepted.prev,
        &accepted.state,
        check,
    )
}

/// Detached: the sink retries a degraded ClickHouse for up to 30s, and the
/// caller of this route is a cron job holding a `curl` open at the end of its
/// run. The scheduler reports on its own tick if this never lands.
fn spawn_result_write(state: &AppState, result: CheckResult) {
    let sink = state.result_sink.clone();
    let target_id = result.target_id;
    tokio::spawn(async move {
        let write = sink.write_batch(std::slice::from_ref(&result));
        let outcome = match tokio::time::timeout(PING_RESULT_WRITE_TIMEOUT, write).await {
            Ok(Ok(())) => return,
            Ok(Err(err)) => err.to_string(),
            Err(_) => "timed out".to_string(),
        };
        tracing::warn!(
            target_id = %target_id,
            error = %outcome,
            "heartbeat ping result not written; the scheduler reports on its own tick"
        );
    });
}

/// Keeps the first [`BODY_SAMPLE_BYTES`] but reads to the end, so `curl -fsS`
/// doesn't report a broken pipe on a ping already counted.
async fn body_sample(body: Body) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut seen = 0usize;
    let mut stream = body.into_data_stream();
    let deadline = tokio::time::sleep(BODY_READ_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        let chunk = tokio::select! {
            _ = &mut deadline => break,
            next = stream.next() => match next {
                Some(Ok(chunk)) => chunk,
                _ => break,
            },
        };
        seen += chunk.len();
        let room = BODY_SAMPLE_BYTES.saturating_sub(kept.len());
        if room > 0 {
            kept.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        if seen >= BODY_DRAIN_LIMIT {
            break;
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}
