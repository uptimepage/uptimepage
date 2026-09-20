//! Public `/alert-channel/stop` — a mailed alert recipient bows out of one
//! channel. Unauthenticated: the signed `c`+`t` link is the proof. GET renders
//! a confirmation so a link-scanner's prefetch can't disable a live channel;
//! the disable happens only on POST (which also serves the RFC 8058 one-click).

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::storage::notification_channels::verify_channel_stop;
use crate::templates::filters;
use crate::web::error::WebResult;

const STOP_REASON: &str = "recipient stopped delivery";

#[derive(Debug, Deserialize)]
pub struct StopQuery {
    #[serde(default)]
    pub c: String,
    #[serde(default)]
    pub t: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "alert_channel_stop.html")]
pub struct AlertChannelStopPage {
    pub phase: &'static str,
    pub c: String,
    pub t: String,
}

fn resolve(state: &AppState, q: &StopQuery) -> Option<Uuid> {
    if state.alert_channel_stop_secret.is_empty() {
        return None;
    }
    let id = Uuid::parse_str(q.c.trim()).ok()?;
    verify_channel_stop(&state.alert_channel_stop_secret, id, q.t.trim()).then_some(id)
}

fn invalid() -> Response {
    (
        StatusCode::NOT_FOUND,
        AlertChannelStopPage {
            phase: "invalid",
            c: String::new(),
            t: String::new(),
        },
    )
        .into_response()
}

pub async fn confirm(
    State(state): State<AppState>,
    Query(q): Query<StopQuery>,
) -> WebResult<Response> {
    if resolve(&state, &q).is_none() {
        return Ok(invalid());
    }
    Ok(AlertChannelStopPage {
        phase: "confirm",
        c: q.c.trim().to_string(),
        t: q.t.trim().to_string(),
    }
    .into_response())
}

pub async fn stop(
    State(state): State<AppState>,
    Query(q): Query<StopQuery>,
) -> WebResult<Response> {
    let Some(id) = resolve(&state, &q) else {
        return Ok(invalid());
    };
    state
        .notification_channel_store
        .disable_self_service(id, STOP_REASON)
        .await?;
    tracing::info!(channel_id = %id, "alert channel stopped by recipient");
    Ok(AlertChannelStopPage {
        phase: "done",
        c: String::new(),
        t: String::new(),
    }
    .into_response())
}
