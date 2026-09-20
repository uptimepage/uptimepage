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
use crate::error::AppError;
use crate::metric_names;

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
            metrics::counter!(metric_names::BILLING_WEBHOOK_REJECTED, "reason" => "signature")
                .increment(1);
            return StatusCode::FORBIDDEN;
        }
        Err(WebhookRejected::Malformed(detail)) => {
            // A retry replays the same bytes, so acknowledge instead of looping.
            tracing::warn!(provider, detail, "billing webhook: unparseable event body");
            metrics::counter!(metric_names::BILLING_WEBHOOK_REJECTED, "reason" => "malformed")
                .increment(1);
            return StatusCode::OK;
        }
    };
    let event_id = event.event_id.clone();
    let event_type = event.event_type.clone();
    match billing.receive(pool, &state.quotas, event).await {
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
            metrics::counter!(metric_names::BILLING_WEBHOOKS, "outcome" => label).increment(1);
            StatusCode::OK
        }
        // A stall heals itself on the provider's redelivery; only a failure
        // to apply is worth waking anyone for.
        Err(err @ AppError::ServiceUnavailable { .. }) => {
            tracing::warn!(provider, event_id, event_type, error = %err, "billing webhook: not committed in time");
            metrics::counter!(metric_names::BILLING_WEBHOOK_REJECTED, "reason" => "stalled")
                .increment(1);
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(err) => {
            tracing::error!(provider, event_id, event_type, error = %err, "billing webhook: apply failed");
            metrics::counter!(metric_names::BILLING_WEBHOOK_REJECTED, "reason" => "failed")
                .increment(1);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
