//! Alerts other tools send in: `POST /hooks/alerts/{token}`, or the token as a
//! Bearer header on `POST /hooks/alerts`. Holding the token is the proof, and
//! a source with a signing secret also needs a valid signature. A refusal the
//! sender should retry answers 503, never 429: Alertmanager and Datadog retry
//! only 5xx, so a 429 would lose the page.

use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::oneshot;

use crate::app::AppState;
use crate::config::AlertIntakeConfig;
use crate::domain::{AlertGrouping, AlertSource, AlertSourceKind, OrgId};
use crate::error::{ApiError, ApiErrorBody, codes};
use crate::intake::gate::ReadError;
use crate::intake::signature::{SIGNATURE_HEADER, TIMESTAMP_HEADER, verify_grafana};
use crate::intake::{self, AlertEvent, Unparsed};
use crate::metric_names;
use crate::quotas::ratelimit::{RateLimitCategory, RateLimitKey};
use crate::request::auth::bearer_from_headers;
use crate::request::rate_limit::note_denied;
use crate::storage::alert_ingest::{Applied, Delivery, Ingested, MAX_OPEN_PER_SOURCE, Status};
use crate::storage::alert_sources::TokenMatch;
use crate::storage::capability_token;

/// For a stalled body or a database that did not answer.
const RETRY_AFTER_SECS: u64 = 5;

pub async fn receive_path(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    receive(&state, &token, &headers, body).await
}

pub async fn receive_bearer(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match bearer_from_headers(&headers) {
        Some(token) if !token.is_empty() => receive(&state, token, &headers, body).await,
        _ => Refusal::UnknownToken.into_response(),
    }
}

#[derive(Debug, Clone, Copy)]
enum Refusal {
    UnknownToken,
    Throttled(Duration),
    /// The account's plan budget of events a minute is spent.
    OverBudget(Duration),
    TooLarge,
    Stalled,
    Incomplete,
    BadSignature,
    Held,
    Unavailable,
}

impl Refusal {
    fn label(self) -> &'static str {
        match self {
            Self::UnknownToken => "unknown_token",
            Self::Throttled(_) => "throttled",
            Self::OverBudget(_) => "over_budget",
            Self::TooLarge => "too_large",
            Self::Stalled => "stalled",
            Self::Incomplete => "incomplete",
            Self::BadSignature => "bad_signature",
            Self::Held => "held",
            Self::Unavailable => "unavailable",
        }
    }
}

impl From<ReadError> for Refusal {
    fn from(e: ReadError) -> Self {
        match e {
            ReadError::TooLarge => Self::TooLarge,
            ReadError::Stalled => Self::Stalled,
            ReadError::Broken => Self::Incomplete,
            ReadError::Busy => Self::Unavailable,
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        self.respond(None)
    }
}

impl Refusal {
    fn respond(self, details: Option<serde_json::Value>) -> Response {
        metrics::counter!(metric_names::ALERT_REQUESTS, "outcome" => self.label()).increment(1);
        let (status, code, message, retry) = match self {
            Self::UnknownToken => (
                StatusCode::NOT_FOUND,
                codes::ALERT_TOKEN_UNKNOWN,
                "no alert source has this token",
                None,
            ),
            Self::Throttled(wait) => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::RATE_LIMITED,
                "too many alerts on this token; send again after the delay",
                Some(wait.as_secs_f64().ceil().max(1.0) as u64),
            ),
            Self::OverBudget(wait) => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::RATE_LIMITED,
                "this account sent more alerts this minute than its plan allows; nothing \
                 was applied, send again after the delay",
                Some(wait.as_secs_f64().ceil().max(1.0) as u64),
            ),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                codes::ALERT_BODY_TOO_LARGE,
                "the body is over 8 MiB; nothing was opened",
                None,
            ),
            Self::Stalled => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::ALERT_BODY_STALLED,
                "the body did not arrive in time; nothing was opened",
                Some(RETRY_AFTER_SECS),
            ),
            Self::Incomplete => (
                StatusCode::BAD_REQUEST,
                codes::ALERT_BODY_INCOMPLETE,
                "the body ended early; nothing was opened",
                None,
            ),
            Self::BadSignature => (
                StatusCode::UNAUTHORIZED,
                codes::ALERT_SIGNATURE_INVALID,
                "the signature or its timestamp is missing, stale or wrong",
                None,
            ),
            Self::Held => (
                StatusCode::FORBIDDEN,
                codes::ALERT_SOURCE_HELD,
                "this alert source is over its plan and opens no incidents; a resolve still closes one",
                None,
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                codes::ALERT_INGEST_UNAVAILABLE,
                "try again shortly",
                Some(RETRY_AFTER_SECS),
            ),
        };
        let body = Json(ApiError {
            error: ApiErrorBody {
                details,
                ..ApiErrorBody::new(code, message)
            },
        });
        match retry {
            Some(secs) => (status, [(header::RETRY_AFTER, secs.to_string())], body).into_response(),
            None => (status, body).into_response(),
        }
    }
}

async fn receive(state: &AppState, token: &str, headers: &HeaderMap, body: Body) -> Response {
    let received_at = Utc::now();
    if state.alert_ingest.is_none() {
        return Refusal::UnknownToken.into_response();
    }
    let gate = &state.alert_gate;
    if let Err(wait) = gate.admit(capability_token::rate_key(token)) {
        return Refusal::Throttled(wait).into_response();
    }
    let Some(lookup) = gate.lookup().await else {
        return Refusal::Unavailable.into_response();
    };
    let found = match state.alert_source_store.find_by_token(token).await {
        Ok(Some(found)) => found,
        Ok(None) => return Refusal::UnknownToken.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "alert ingest: token lookup failed");
            return Refusal::Unavailable.into_response();
        }
    };
    drop(lookup);
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let signature = Signature::from(headers);
    // On its own task, so a sender that hangs up part way through a large
    // body does not cut its events short or lose their log.
    let (answer, answered) = oneshot::channel();
    let state = state.clone();
    tokio::spawn(async move {
        let read = match state.alert_gate.read(found.org.0, body, declared).await {
            Ok(read) => read,
            Err(err) => {
                let _ = answer.send(Refusal::from(err).into_response());
                return;
            }
        };
        // CPU work over up to 8 MiB stays off the workers checks run on.
        let checked = {
            let (found, body) = (found.clone(), read.body.clone());
            tokio::task::spawn_blocking(move || {
                let (kind, grouping) = (found.source.kind, found.source.grouping);
                signature
                    .holds(&found, &body)
                    .then(|| intake::parse(kind, grouping, &body))
            })
            .await
        };
        let parsed = match checked {
            Ok(Some(parsed)) => parsed,
            Ok(None) => {
                tracing::warn!(source_id = %found.source.id, "alert ingest: signature refused");
                let _ = answer.send(Refusal::BadSignature.into_response());
                return;
            }
            Err(err) => {
                tracing::error!(error = %err, "alert ingest: parse task failed");
                let _ = answer.send(Refusal::Unavailable.into_response());
                return;
            }
        };
        let events = parsed.as_ref().map_or(1, Vec::len);
        if let Err(wait) = spend_account_budget(&state, found.org, events).await {
            let _ = answer.send(Refusal::OverBudget(wait).into_response());
            return;
        }
        let (mut lines, failed) = match apply(&state, found.org, &found.source, parsed).await {
            Applying::Done(lines, failed) => (lines, failed),
            Applying::Regrouped(mut lines, grouping) => {
                tracing::warn!(
                    source_id = %found.source.id,
                    "alert ingest: grouping changed while the body was read; parsing it again"
                );
                let source = AlertSource {
                    grouping,
                    ..found.source.clone()
                };
                let again = match parse_off_workers(source.kind, grouping, read.body.clone()).await
                {
                    Some(parsed) => apply(&state, found.org, &source, parsed).await,
                    None => Applying::Done(Vec::new(), true),
                };
                match again {
                    Applying::Done(more, failed) => {
                        lines.extend(more);
                        (lines, failed)
                    }
                    Applying::Regrouped(more, _) => {
                        tracing::warn!(source_id = %found.source.id, "alert ingest: grouping changed again");
                        lines.extend(more);
                        (lines, true)
                    }
                }
            }
        };
        if failed && state.alert_gate.logs_unavailable(found.source.id) {
            lines.push(unavailable(lines.is_empty()));
        }
        for line in lines.iter().filter(|l| l.status != Status::Unavailable) {
            metrics::counter!(metric_names::ALERT_EVENTS, "status" => line.status.as_str())
                .increment(1);
        }
        let _ = answer.send(reply(&lines, failed));
        let sample = sample(&read.body, &lines, failed, &state.cfg.alert_intake);
        drop(read);
        log_deliveries(&state, &found, received_at, &lines, sample).await;
    });
    answered
        .await
        .unwrap_or_else(|_| Refusal::Unavailable.into_response())
}

/// The account's event budget, spent per event once the body is parsed, so
/// a body of many alerts costs what it carries. Over it, the sender sends the
/// whole body again later. A plan that cannot be read lets the request
/// through: applying it fails the same way, and answers for itself.
async fn spend_account_budget(state: &AppState, org: OrgId, events: usize) -> Result<(), Duration> {
    let Ok(Some((account, plan))) = state.quotas.account_and_plan(org).await else {
        return Ok(());
    };
    let key = RateLimitKey::Account(account, RateLimitCategory::AlertEvents);
    let n = u32::try_from(events).unwrap_or(u32::MAX);
    state
        .rate_limits
        .check_n(key, "per_account", &plan, n)
        .map_err(|denied| {
            note_denied(
                state.db.clone(),
                &state.rate_limits,
                Some(org),
                None,
                &denied,
            );
            Duration::from_secs(denied.retry_after_secs.into())
        })
}

/// The line a request leaves in its delivery log when it could not be applied
/// in full, so the log says why the sender is retrying.
fn unavailable(nothing_applied: bool) -> Line {
    Line {
        status: Status::Unavailable,
        key: None,
        incident: None,
        note: match nothing_applied {
            true => "not applied; answered 503 for the sender to send it again",
            false => "the rest was not applied; answered 503 for the sender to send it again",
        }
        .into(),
        changed: false,
    }
}

fn reply(lines: &[Line], failed: bool) -> Response {
    if failed {
        return Refusal::Unavailable.into_response();
    }
    if lines.iter().any(|l| l.status == Status::Gone) {
        return Refusal::UnknownToken.into_response();
    }
    let reply = Reply::of(lines);
    if lines.iter().any(|l| l.status == Status::Held) {
        // What else in the body did land, so a resolve it carried is not
        // read as refused with the rest.
        let details = match reply {
            Reply::Many { events, .. } => Some(serde_json::json!({ "events": events })),
            Reply::One(_) => None,
        };
        return Refusal::Held.respond(details);
    }
    metrics::counter!(metric_names::ALERT_REQUESTS, "outcome" => "accepted").increment(1);
    (StatusCode::ACCEPTED, Json(reply)).into_response()
}

/// The signature headers, kept past the request so the body can be read
/// and checked on the request's own task.
struct Signature {
    timestamp: Option<String>,
    signature: Option<String>,
}

impl From<&HeaderMap> for Signature {
    fn from(headers: &HeaderMap) -> Self {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        Self {
            timestamp: header(TIMESTAMP_HEADER),
            signature: header(SIGNATURE_HEADER),
        }
    }
}

impl Signature {
    /// A source without a signing secret needs none. One with a secret the
    /// KEK can no longer open fails, as the store promises.
    fn holds(&self, found: &TokenMatch, body: &[u8]) -> bool {
        if !found.source.signed {
            return true;
        }
        let Some(secret) = found.signing_secret.as_deref() else {
            return false;
        };
        verify_grafana(
            secret,
            self.timestamp.as_deref(),
            self.signature.as_deref(),
            body,
            Utc::now(),
        )
        .is_ok()
    }
}

/// What one event did, as the sender is told and the log keeps it.
struct Line {
    status: Status,
    key: Option<String>,
    incident: Option<uuid::Uuid>,
    note: String,
    /// A repeat that changed what the incident shows.
    changed: bool,
}

/// What applying a body came to, with the lines of what committed.
enum Applying {
    /// And whether the database failed or fell behind part way: what
    /// committed stays, and the sender's retry repeats it harmlessly.
    Done(Vec<Line>, bool),
    /// The source's grouping changed after the body was parsed, to this one,
    /// so the rest has to be parsed again.
    Regrouped(Vec<Line>, AlertGrouping),
}

/// Parses off the async workers: up to 8 MiB of JSON is CPU work.
async fn parse_off_workers(
    kind: AlertSourceKind,
    grouping: AlertGrouping,
    body: Bytes,
) -> Option<Result<Vec<AlertEvent>, Unparsed>> {
    match tokio::task::spawn_blocking(move || intake::parse(kind, grouping, &body)).await {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            tracing::error!(error = %err, "alert ingest: parse task failed");
            None
        }
    }
}

/// Applies every event in the body on its source's turn, each in its own
/// transaction, signalling the engine after each commit.
async fn apply(
    state: &AppState,
    org: OrgId,
    source: &AlertSource,
    parsed: Result<Vec<AlertEvent>, Unparsed>,
) -> Applying {
    let Some(ingest) = state.alert_ingest.as_deref() else {
        return Applying::Done(Vec::new(), true);
    };
    let gate = &state.alert_gate;
    let mut lines = Vec::new();
    let events: Vec<Option<&AlertEvent>> = match &parsed {
        Ok(events) => events.iter().map(Some).collect(),
        Err(_) => vec![None],
    };
    let unparsed = parsed.as_ref().err().map(|u| u.reason.as_str());
    let Some(_turn) = gate.source_turn(source.id).await else {
        tracing::warn!(source_id = %source.id, "alert ingest: no turn for the source in time");
        return Applying::Done(lines, true);
    };
    if events.is_empty() {
        let Some(_write) = gate.write().await else {
            return Applying::Done(lines, true);
        };
        if let Err(err) = ingest.heard(org, source).await {
            tracing::warn!(error = %err, source_id = %source.id, "alert ingest: stamp failed");
        }
        lines.push(Line {
            status: Status::Ignored,
            key: None,
            incident: None,
            note: "nothing in it is an alert we act on".into(),
            changed: false,
        });
        return Applying::Done(lines, false);
    }
    for event in events {
        let Some(_write) = gate.write().await else {
            tracing::warn!(source_id = %source.id, "alert ingest: no write turn in time");
            return Applying::Done(lines, true);
        };
        let applied = match (event, unparsed) {
            (Some(event), _) => ingest.apply(org, source, event).await,
            (None, reason) => {
                ingest
                    .apply_unparsed(org, source, reason.unwrap_or_default())
                    .await
            }
        };
        match applied {
            Ok(Ingested::Regrouped(grouping)) => return Applying::Regrouped(lines, grouping),
            Ok(Ingested::Applied(applied)) => {
                signal(state, org, &applied);
                let gone = applied.status == Status::Gone;
                lines.push(line(event, applied, source, unparsed));
                if gone {
                    break;
                }
            }
            Err(err) => {
                tracing::error!(error = %err, source_id = %source.id, "alert ingest: apply failed");
                return Applying::Done(lines, true);
            }
        }
    }
    Applying::Done(lines, false)
}

fn signal(state: &AppState, org: OrgId, applied: &Applied) {
    for &(incident, reason) in &applied.signals {
        state.signal_incident(org, incident, reason);
    }
}

fn line(
    event: Option<&AlertEvent>,
    applied: Applied,
    source: &AlertSource,
    unparsed: Option<&str>,
) -> Line {
    Line {
        note: note(event, &applied, source, unparsed),
        status: applied.status,
        key: event.map(|e| e.key.clone()),
        incident: applied.incident,
        changed: applied.changed,
    }
}

/// Why an event did what it did, when its status alone does not say.
fn note(
    event: Option<&AlertEvent>,
    applied: &Applied,
    source: &AlertSource,
    unparsed: Option<&str>,
) -> String {
    if let Some(note) = applied.note {
        return note.into();
    }
    let cut = event.map_or(0, |e| e.truncated);
    match applied.status {
        Status::Opened if source.routes_to_nobody => {
            "opened, and paged nobody: the source has no route".into()
        }
        Status::Repeat if event.is_some_and(|e| !e.may_open) => {
            format!("resolved with {cut} alerts cut from it, so kept open")
        }
        Status::Capped => {
            format!("{MAX_OPEN_PER_SOURCE} incidents already open; counted on the summary")
        }
        Status::Paused => "the source is paused".into(),
        Status::Held => "the source is over its plan".into(),
        Status::Unparsed => unparsed.unwrap_or_default().into(),
        _ if cut > 0 => format!("the sender cut {cut} alerts from it"),
        _ => String::new(),
    }
}

/// What the delivery log keeps of a body, taken so the body itself can go
/// before the log is written: at most `body_sample_bytes`, cut on a character
/// boundary. Nothing for a request that only repeated what is already open
/// and changed nothing, unless the operator keeps those: such repeats are
/// most of the traffic, and the body that opened the incident is kept.
fn sample(body: &[u8], lines: &[Line], failed: bool, cfg: &AlertIntakeConfig) -> String {
    let quiet = !failed
        && !lines.is_empty()
        && lines
            .iter()
            .all(|l| l.status == Status::Repeat && !l.changed && l.note.is_empty());
    if quiet && !cfg.keep_repeat_bodies {
        return String::new();
    }
    // A replacement character is 3 bytes, so reading 3 past the cap leaves
    // enough to cut at it.
    let max = cfg.body_sample_bytes;
    let text = String::from_utf8_lossy(&body[..body.len().min(max.saturating_add(3))]);
    text[..text.floor_char_boundary(max)].to_owned()
}

/// After the answer: the log is history, and a slow ClickHouse must not hold
/// a sender waiting to hear its alert landed. Bounded, so a ClickHouse outage
/// during a storm drops log rows rather than piling up writes.
async fn log_deliveries(
    state: &AppState,
    found: &TokenMatch,
    received_at: DateTime<Utc>,
    lines: &[Line],
    sample: String,
) {
    let Some(sink) = state.alert_event_sink.as_deref() else {
        return;
    };
    let Some(_slot) = state.alert_gate.log_write() else {
        metrics::counter!(metric_names::STORAGE_DROPPED, "reason" => "alert_event_buffer_full")
            .increment(lines.len() as u64);
        return;
    };
    let mut sample = Some(sample);
    let rows: Vec<Delivery> = lines
        .iter()
        .map(|l| Delivery {
            org_id: found.org.0,
            source_id: found.source.id,
            received_at,
            status: l.status,
            dedup_key: l.key.clone().unwrap_or_default(),
            incident_id: l.incident,
            note: l.note.clone(),
            body: sample.take().unwrap_or_default(),
        })
        .collect();
    sink.write_deliveries(&rows).await;
}

#[derive(Serialize)]
struct EventReply<'a> {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dedup_key: Option<&'a str>,
}

/// One event answers as itself, so a sender written for a one-event API
/// reads its key where it expects; several list each one.
#[derive(Serialize)]
#[serde(untagged)]
enum Reply<'a> {
    One(EventReply<'a>),
    Many {
        status: &'static str,
        events: Vec<EventReply<'a>>,
    },
}

impl<'a> Reply<'a> {
    fn of(lines: &'a [Line]) -> Self {
        let mut events: Vec<EventReply<'a>> = lines
            .iter()
            .map(|l| EventReply {
                status: l.status.as_str(),
                dedup_key: l.key.as_deref(),
            })
            .collect();
        if events.len() == 1 {
            return Self::One(events.remove(0));
        }
        Self::Many {
            status: "accepted",
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repeat(changed: bool) -> Line {
        Line {
            status: Status::Repeat,
            key: Some("k".into()),
            incident: None,
            note: String::new(),
            changed,
        }
    }

    fn cfg(bytes: usize, keep_repeats: bool) -> AlertIntakeConfig {
        AlertIntakeConfig {
            body_sample_bytes: bytes,
            keep_repeat_bodies: keep_repeats,
        }
    }

    #[test]
    fn a_quiet_repeat_keeps_no_body_and_anything_else_does() {
        let body = b"{\"event_action\":\"trigger\"}";
        let on = cfg(4096, false);
        assert_eq!(sample(body, &[repeat(false)], false, &on), "");
        assert_eq!(sample(body, &[repeat(true)], false, &on).as_bytes(), body);
        assert_eq!(
            sample(body, &[repeat(false)], true, &on).as_bytes(),
            body,
            "a request that failed part way keeps its body"
        );
        let cut = Line {
            note: "resolved with 2 alerts cut from it, so kept open".into(),
            ..repeat(false)
        };
        assert_eq!(sample(body, &[cut], false, &on).as_bytes(), body);
        let opened = Line {
            status: Status::Opened,
            ..repeat(false)
        };
        assert_eq!(
            sample(body, &[repeat(false), opened], false, &on).as_bytes(),
            body
        );
        assert_eq!(
            sample(body, &[repeat(false)], false, &cfg(4096, true)).as_bytes(),
            body,
            "an operator can keep repeats"
        );
    }

    #[test]
    fn the_sample_is_capped_on_a_character_boundary() {
        let opened = [Line {
            status: Status::Opened,
            ..repeat(false)
        }];
        assert_eq!(
            sample(b"0123456789abc", &opened, false, &cfg(10, false)),
            "0123456789"
        );
        assert_eq!(
            sample(b"anything", &opened, false, &cfg(0, false)),
            "",
            "0 keeps none"
        );
        let accented = "caf\u{e9} au lait".as_bytes();
        assert_eq!(sample(accented, &opened, false, &cfg(4, false)), "caf");
        let invalid = [b'a', 0xff, 0xfe, b'b', b'c'];
        assert!(sample(&invalid, &opened, false, &cfg(4, false)).len() <= 4);
    }
}
