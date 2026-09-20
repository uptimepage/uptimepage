//! Public `/incident/ack` — whoever holds the page takes the incident from the
//! notification itself. Unauthenticated: the signed link is the proof, and it
//! names no user, so the ack is recorded as [`Actor::Link`]. GET renders a
//! confirmation so a link scanner's prefetch can't silence a live incident; the
//! acknowledge happens only on POST.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::OrgId;
use crate::storage::incident_ops::verify_incident_ack;
use crate::storage::{Actor, LifecycleOutcome};
use crate::templates::filters;
use crate::web::error::WebResult;

const ACK_NOTE: &str = "Acknowledged from a notification link";

#[derive(Debug, Deserialize)]
pub struct AckQuery {
    #[serde(default)]
    pub o: String,
    #[serde(default)]
    pub i: String,
    #[serde(default)]
    pub c: String,
    #[serde(default)]
    pub g: String,
    #[serde(default)]
    pub e: String,
    #[serde(default)]
    pub t: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "incident_ack.html")]
pub struct IncidentAckPage {
    pub phase: &'static str,
    pub o: String,
    pub i: String,
    pub c: String,
    pub g: String,
    pub e: String,
    pub t: String,
}

struct Link {
    org: OrgId,
    incident_id: Uuid,
    /// Episode the page belonged to; the store refuses one already left.
    generation: i64,
}

/// A bad signature, a malformed id and a lapsed expiry all read the same, so a
/// prober learns nothing from which one it was.
fn resolve(state: &AppState, q: &AckQuery) -> Option<Link> {
    if state.incident_ack_secret.is_empty() {
        return None;
    }
    let org = OrgId(Uuid::parse_str(q.o.trim()).ok()?);
    let incident_id = Uuid::parse_str(q.i.trim()).ok()?;
    let channel_id = Uuid::parse_str(q.c.trim()).ok()?;
    let generation: i64 = q.g.trim().parse().ok()?;
    let expires_at: i64 = q.e.trim().parse().ok()?;
    if expires_at <= Utc::now().timestamp() {
        return None;
    }
    verify_incident_ack(
        &state.incident_ack_secret,
        org,
        incident_id,
        channel_id,
        generation,
        expires_at,
        q.t.trim(),
    )
    .then_some(Link {
        org,
        incident_id,
        generation,
    })
}

/// The status carries the outcome as much as the page does: ntfy's one-tap
/// button reads only the code, and clears the notification on a 2xx. Anything
/// that did not acknowledge must say so, or the responder is told they took an
/// outage that is still running.
fn page(phase: &'static str) -> Response {
    let body = IncidentAckPage {
        phase,
        o: String::new(),
        i: String::new(),
        c: String::new(),
        g: String::new(),
        e: String::new(),
        t: String::new(),
    };
    match phase {
        "invalid" => (StatusCode::NOT_FOUND, body).into_response(),
        "stale" | "resolved" => (StatusCode::CONFLICT, body).into_response(),
        _ => body.into_response(),
    }
}

pub async fn confirm(
    State(state): State<AppState>,
    Query(q): Query<AckQuery>,
) -> WebResult<Response> {
    if resolve(&state, &q).is_none() {
        return Ok(page("invalid"));
    }
    Ok(IncidentAckPage {
        phase: "confirm",
        o: q.o.trim().to_string(),
        i: q.i.trim().to_string(),
        c: q.c.trim().to_string(),
        g: q.g.trim().to_string(),
        e: q.e.trim().to_string(),
        t: q.t.trim().to_string(),
    }
    .into_response())
}

pub async fn ack(State(state): State<AppState>, Query(q): Query<AckQuery>) -> WebResult<Response> {
    let Some(link) = resolve(&state, &q) else {
        return Ok(page("invalid"));
    };
    let outcome = state
        .incident_ops_store
        .acknowledge(
            link.org,
            link.incident_id,
            Actor::Link,
            Some(ACK_NOTE.to_string()),
            Some(link.generation),
        )
        .await?;
    Ok(match outcome {
        LifecycleOutcome::Updated(_) => {
            tracing::info!(
                org_id = %link.org.0,
                incident_id = %link.incident_id,
                "incident acknowledged from a notification link"
            );
            page("done")
        }
        // The responder lost a race with the recovery. Not an error.
        LifecycleOutcome::IllegalTransition(_) => page("resolved"),
        // Reopened since this page went out: the outage running now is not the
        // one this alert was about.
        LifecycleOutcome::Stale => page("stale"),
        LifecycleOutcome::NotFound => page("invalid"),
    })
}
