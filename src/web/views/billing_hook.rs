//! Payment-provider webhook receiver (`/hooks/billing/{provider}`). The
//! provider's signature is the only authentication. Every accepted delivery
//! is answered 200 whatever it changed; only a failure to apply answers 5xx,
//! which is the provider's cue to deliver again.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;

use crate::app::AppState;
use crate::billing::lifecycle::Outcome;
use crate::billing::provider::WebhookRejected;
use crate::observability::metrics::names;

pub async fn webhook(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let (Some(billing), Ok(pool)) = (state.billing.as_ref(), state.require_db()) else {
        return StatusCode::NOT_FOUND;
    };
    if billing.provider.name() != provider {
        return StatusCode::NOT_FOUND;
    }
    let event = match billing.provider.parse_webhook(&headers, &body, Utc::now()) {
        Ok(event) => event,
        Err(WebhookRejected::Signature) => {
            tracing::warn!(provider, "billing webhook rejected: bad signature");
            metrics::counter!(names::BILLING_WEBHOOK_REJECTED, "reason" => "signature")
                .increment(1);
            return StatusCode::FORBIDDEN;
        }
        Err(WebhookRejected::Malformed(detail)) => {
            // A retry replays the same bytes, so acknowledge instead of looping.
            tracing::warn!(provider, detail, "billing webhook: unparseable event body");
            metrics::counter!(names::BILLING_WEBHOOK_REJECTED, "reason" => "malformed")
                .increment(1);
            return StatusCode::OK;
        }
    };
    let event_id = event.event_id.clone();
    let event_type = event.event_type.clone();
    match billing.apply_event(pool, &state.quotas, event).await {
        Ok(outcome) => {
            let label = match outcome {
                Outcome::Applied => "applied",
                Outcome::Duplicate => "duplicate",
                Outcome::Stale => "stale",
                Outcome::Unmatched => "unmatched",
                Outcome::Foreign => "foreign",
                Outcome::Unchanged => "unchanged",
            };
            tracing::info!(
                provider,
                event_id,
                event_type,
                outcome = label,
                "billing webhook"
            );
            metrics::counter!(names::BILLING_WEBHOOKS, "outcome" => label).increment(1);
            StatusCode::OK
        }
        Err(err) => {
            tracing::error!(provider, event_id, event_type, error = %err, "billing webhook: apply failed");
            metrics::counter!(names::BILLING_WEBHOOK_REJECTED, "reason" => "failed").increment(1);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
