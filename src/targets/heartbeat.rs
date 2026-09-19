//! What a heartbeat monitor shows about itself: its ping URL and the state its
//! signals last reported. Shared by the JSON endpoint and the detail page.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{CadenceAdvice, HeartbeatCheck, ObservedCadence, OrgId};
use crate::error::Result;
use crate::storage::HeartbeatMonitor;

/// A heartbeat's ping URL and what its signals last reported.
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeartbeatInfo {
    /// `null` when the stored token can't be decrypted (KEK rotated out).
    pub ping_url: Option<String>,
    /// Last success. A `/start` opens a run, it does not report one.
    pub last_ping_at: Option<chrono::DateTime<chrono::Utc>>,
    /// First ping of any signal. `null` while the job has never spoken.
    pub first_ping_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Waiting for that first ping: not evaluated, not alerting, no data yet.
    pub pending: bool,
    /// When the wait started, and what the three-day reminder counts from.
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_start_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_fail_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_exit_code: Option<u8>,
    /// What the job printed on that failure, while inside its window.
    pub last_failure_output: Option<String>,
    /// `None` while pending, paused, or already failing on the job's own report.
    pub due_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When silence starts reading as down. An incident follows a confirmation
    /// or two later, so this is the earlier instant.
    pub down_at: Option<chrono::DateTime<chrono::Utc>>,
    pub declared_period_secs: u64,
    /// Median gap between successes, `null` until a second one gives it a gap.
    pub observed_period_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cadence_advice: Option<CadenceAdviceView>,
    pub rotated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// While set, the pre-rotation URL still pings, and dies at this instant.
    pub previous_url_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Evidence the old URL is still carried. `null` once the overlap ends.
    pub previous_url_last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CadenceAdviceView {
    /// `too_tight` or `too_loose`.
    pub kind: String,
    pub suggested_period_secs: u64,
}

/// Wide enough for a daily job to clear the sample floor, narrow enough that a
/// schedule changed last week stops counting.
const CADENCE_WINDOW_DAYS: u16 = 14;

/// Commentary on state Postgres already holds, so a ClickHouse outage costs
/// the commentary rather than the caller.
pub async fn observed_cadence(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
) -> Option<ObservedCadence> {
    state
        .results_store
        .heartbeat_cadence(org, target_id, CADENCE_WINDOW_DAYS)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "heartbeat cadence unavailable");
            None
        })
}

impl From<CadenceAdvice> for CadenceAdviceView {
    fn from(a: CadenceAdvice) -> Self {
        let (kind, period) = match a {
            CadenceAdvice::TooTight { suggested_period } => ("too_tight", suggested_period),
            CadenceAdvice::TooLoose { suggested_period } => ("too_loose", suggested_period),
        };
        Self {
            kind: kind.to_string(),
            suggested_period_secs: period.as_secs(),
        }
    }
}

/// Shared by the API handler and the detail page. Never mints, so `ping_url`
/// is `None` while the row is still provisioning.
pub async fn heartbeat_info(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
    check: &HeartbeatCheck,
    enabled: bool,
) -> Result<HeartbeatInfo> {
    let hb = state.heartbeat_store.get(org, target_id).await?;
    Ok(heartbeat_info_from(state, org, target_id, check, enabled, hb).await)
}

/// Infallible on purpose: a writer that has committed must not lose its
/// response to a read, since the retry supersedes the token it just minted.
pub async fn heartbeat_info_from(
    state: &AppState,
    org: OrgId,
    target_id: Uuid,
    check: &HeartbeatCheck,
    enabled: bool,
    hb: Option<HeartbeatMonitor>,
) -> HeartbeatInfo {
    let observed = observed_cadence(state, org, target_id).await;
    let last_failure_output = match hb.as_ref().and_then(|h| h.last_fail_at) {
        Some(at) => state
            .results_store
            .heartbeat_failure_output(org, target_id, at)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "heartbeat failure output unavailable");
                None
            }),
        None => None,
    };
    let (due_at, down_at) = match hb.as_ref().filter(|h| enabled && !h.is_pending()) {
        Some(h) if h.ping_state().failing().is_none() => {
            let (due, down) = check.window(h.ping_state().success_at);
            (Some(due), Some(down))
        }
        _ => (None, None),
    };
    HeartbeatInfo {
        ping_url: hb.as_ref().and_then(|h| h.token.as_deref()).map(|t| {
            format!(
                "{}/ping/{t}",
                state.cfg.auth.public_base_url.trim_end_matches('/')
            )
        }),
        last_ping_at: hb.as_ref().and_then(|h| h.last_ping_at),
        first_ping_at: hb.as_ref().and_then(|h| h.first_ping_at),
        // A missing row is still provisioning, which is pending all the same.
        pending: hb.as_ref().is_none_or(HeartbeatMonitor::is_pending),
        created_at: hb.as_ref().map(|h| h.created_at),
        last_start_at: hb.as_ref().and_then(|h| h.last_start_at),
        last_fail_at: hb.as_ref().and_then(|h| h.last_fail_at),
        rotated_at: hb.as_ref().and_then(|h| h.token_rotated_at),
        previous_url_expires_at: hb.as_ref().and_then(|h| h.open_overlap()),
        previous_url_last_used_at: hb
            .as_ref()
            .and_then(|h| h.open_overlap().and(h.prev_token_last_used_at)),
        last_exit_code: hb.and_then(|h| h.last_exit_code),
        last_failure_output,
        due_at,
        down_at,
        declared_period_secs: check.period.as_secs(),
        observed_period_secs: observed.map(|o| o.median_gap.as_secs()),
        cadence_advice: observed
            .and_then(|o| o.advice(check.period + check.grace))
            .map(Into::into),
    }
}
