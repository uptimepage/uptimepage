//! The slice of application state the request pipeline reads. Extractors and
//! middleware take this instead of the full `AppState`, so the pipeline sits
//! below the composition root; `AppState::request_state` builds it.

use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache;
use sqlx::PgPool;

use crate::auth::api_tokens::ApiTokenLastUsedDebounce;
use crate::auth::session::LastUsedDebounce;
use crate::config::AppConfig;
use crate::custom_domains::CustomDomains;
use crate::error::{AppError, Result};
use crate::quotas::{QuotaService, RateLimitService};

/// Per-agent "last_seen written recently" set, so a chatty agent doesn't UPDATE
/// its row on every pull/push.
pub type AgentSeenDebounce = Cache<uuid::Uuid, ()>;

pub fn build_agent_seen_debounce() -> AgentSeenDebounce {
    Cache::builder()
        .time_to_live(Duration::from_secs(30))
        .max_capacity(10_000)
        .build()
}

#[derive(Clone)]
pub struct RequestState {
    pub cfg: Arc<AppConfig>,
    pub db: Option<PgPool>,
    pub quotas: Arc<QuotaService>,
    pub rate_limits: Arc<RateLimitService>,
    pub custom_domains: Arc<CustomDomains>,
    pub session_debounce: Arc<LastUsedDebounce>,
    pub api_token_debounce: Arc<ApiTokenLastUsedDebounce>,
    pub agent_seen_debounce: AgentSeenDebounce,
}

impl RequestState {
    pub fn require_db(&self) -> Result<&PgPool> {
        require_pool(self.db.as_ref())
    }
}

/// `None` is permitted only for in-memory test fixtures; any production path
/// that observes it is an internal error.
pub fn require_pool(db: Option<&PgPool>) -> Result<&PgPool> {
    db.ok_or_else(|| {
        AppError::Other(anyhow::anyhow!(
            "tenancy enabled but the database handle is None"
        ))
    })
}
