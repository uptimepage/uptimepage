//! Agent authentication for the `/api/agent/*` surface. Resolves the bearer
//! token against the `agents` table (prefix-narrow + argon2 verify), rejects a
//! disabled agent, and yields the region + stable agent id the handlers stamp
//! onto results. The identity comes from the token, never the request body, so
//! an agent cannot claim another region or spoof its id.

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;

use crate::app::AppState;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::request::auth::bearer_from_headers;
use crate::storage::operator::{AgentAuth, OperatorRepo};

/// An authenticated regional agent.
pub struct AgentIdentity {
    /// Region the agent serves; scopes its config pull and result ingest.
    pub region: String,
    /// Stable agent id (the `agents` row id), stamped onto submitted results.
    pub agent_id: String,
}

impl<S> FromRequestParts<S> for AgentIdentity
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        let app = AppState::from_ref(state);
        let pool = app.require_db()?.clone();
        let raw = bearer_from_headers(&parts.headers).ok_or(AppError::Unauthorized)?;
        let repo = OperatorRepo::new(pool.clone());
        match repo.authenticate_agent(raw).await? {
            AgentAuth::Active { id, region } => {
                // last_seen heartbeat, debounced so a 5s-flush agent doesn't
                // UPDATE its row constantly.
                if app.agent_seen_debounce.get(&id).is_none() {
                    app.agent_seen_debounce.insert(id, ());
                    tokio::spawn(async move {
                        if let Err(err) = OperatorRepo::new(pool).touch_agent_seen(id).await {
                            tracing::warn!(error = %err, "agent last_seen update failed");
                        }
                    });
                }
                Ok(AgentIdentity {
                    region,
                    agent_id: id.to_string(),
                })
            }
            AgentAuth::Disabled => Err(AppError::forbidden_code(
                codes::AGENT_NOT_FOUND,
                "agent is disabled",
            )),
            AgentAuth::Invalid => Err(AppError::Unauthorized),
        }
    }
}
