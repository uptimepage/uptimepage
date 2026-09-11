//! The in-app help form's relay.
//!
//! Identity, org, plan and build come from server state, never the body, so a
//! request cannot misreport who sent it. Delivery is synchronous because a
//! dropped mail would silently break the reply the page promises.

use crate::api::json::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::error::codes;
use crate::app::AppState;
use crate::email::templates::support_request::short_ref;
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::error::{AppError, Result};
use crate::storage::orgs as orgs_store;
use crate::web::auth::{CurrentOrg, Session};

/// Closed set: keeps the subject sortable and stops a second unbounded body.
const TOPICS: [&str; 5] = ["question", "bug", "feature", "regions", "billing"];

const MAX_MESSAGE: usize = 4_000;
const MAX_PAGE_URL: usize = 300;
const UNKNOWN: &str = "unknown";

#[derive(Debug, Deserialize)]
pub struct SupportRequestBody {
    pub topic: String,
    pub message: String,
    /// Dropped rather than rejected: a convenience field, not an assertion.
    #[serde(default)]
    pub page_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SupportRequestAck {
    /// Short form; the canonical id is in the mail headers and the relay log.
    pub reference: String,
}

/// Same-origin paths only, so the inbox never renders an attacker-chosen link
/// as if it were our own.
fn sanitize_page_url(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim();
    let same_origin_path = value.starts_with('/') && !value.starts_with("//");
    if !same_origin_path
        || value.len() > MAX_PAGE_URL
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    Some(value.to_string())
}

/// Kept out of the published OpenAPI surface on purpose: app-internal.
pub async fn create(
    State(state): State<AppState>,
    session: Session,
    CurrentOrg(org): CurrentOrg,
    Json(body): Json<SupportRequestBody>,
) -> Result<(StatusCode, Json<SupportRequestAck>)> {
    let support_address = state.cfg.email.support_address.trim();
    if support_address.is_empty() {
        return Err(AppError::ServiceUnavailable {
            code: codes::SUPPORT_UNAVAILABLE,
            message: "this deployment has no support address configured".into(),
        });
    }
    let user = session.user.as_ref().ok_or(AppError::Unauthorized)?;

    let topic = body.topic.trim().to_ascii_lowercase();
    if !TOPICS.contains(&topic.as_str()) {
        return Err(AppError::bad_request_field(
            codes::INVALID_TOPIC,
            format!("topic must be one of: {}", TOPICS.join(", ")),
            "topic",
        ));
    }
    let message = body.message.trim();
    crate::api::handlers::validation::check_required_text(
        message,
        "message",
        MAX_MESSAGE,
        codes::EMPTY_MESSAGE,
        codes::MESSAGE_TOO_LONG,
    )?;

    // Context, not preconditions: a lookup hiccup must not turn away a help
    // request, and the org id in the mail still identifies the tenant.
    let org_slug = match state.db.as_ref() {
        Some(pool) => orgs_store::get_org(pool, org)
            .await
            .ok()
            .flatten()
            .map(|o| o.slug),
        None => None,
    };
    let plan = state.quotas.limit_for_org(org).await.ok();
    // Minted here, not in the template, so mail, log and screen share one id.
    let request_id = Uuid::now_v7().to_string();

    let email = TransactionalEmail {
        to: EmailAddress::new(support_address, state.cfg.email.from_name.clone()),
        from: EmailAddress::new(
            state.cfg.email.from_address.clone(),
            state.cfg.email.from_name.clone(),
        ),
        template: EmailTemplate::SupportRequest {
            request_id: request_id.clone(),
            topic: topic.clone(),
            message: message.to_string(),
            from_email: user.email.clone(),
            org_slug: org_slug.unwrap_or_else(|| UNKNOWN.to_string()),
            org_id: org.0.to_string(),
            plan: plan.map_or_else(|| UNKNOWN.to_string(), |p| p.name.clone()),
            page_url: sanitize_page_url(body.page_url.as_deref()),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    };

    state.email_sender.send(email).await.map_err(|e| {
        tracing::error!(
            request_id = %request_id,
            org = %org.0,
            topic = %topic,
            error = %e,
            "support request delivery failed"
        );
        AppError::ServiceUnavailable {
            code: codes::SUPPORT_DELIVERY_FAILED,
            message: "could not deliver your message, please try again shortly".into(),
        }
    })?;
    // No message body: it is customer prose and the logs outlive the ticket.
    tracing::info!(
        request_id = %request_id,
        org = %org.0,
        user = %user.id.0,
        topic = %topic,
        "support request relayed"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(SupportRequestAck {
            reference: short_ref(&request_id).to_string(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::sanitize_page_url;

    #[test]
    fn keeps_a_same_origin_path() {
        assert_eq!(
            sanitize_page_url(Some("/targets/42?range=24h")),
            Some("/targets/42?range=24h".to_string())
        );
    }

    #[test]
    fn drops_anything_that_could_point_off_site() {
        for raw in [
            "https://evil.test/phish",
            "//evil.test/phish",
            "targets/42",
            "javascript:alert(1)",
        ] {
            assert_eq!(sanitize_page_url(Some(raw)), None, "must reject {raw}");
        }
    }

    #[test]
    fn drops_control_characters_and_overlong_paths() {
        assert_eq!(sanitize_page_url(Some("/a\nb")), None);
        assert_eq!(
            sanitize_page_url(Some(&format!("/{}", "a".repeat(400)))),
            None
        );
    }

    #[test]
    fn absent_stays_absent() {
        assert_eq!(sanitize_page_url(None), None);
        assert_eq!(sanitize_page_url(Some("   ")), None);
    }
}
