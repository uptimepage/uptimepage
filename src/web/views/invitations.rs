//! Browser landing pages for the emailed invitation links. Accept requires a
//! session (mail-scanner prefetch is cookieless → bounced to /login, zero
//! mutation); decline is possession-based but MUST NOT mutate on GET — the
//! page confirms, the button POSTs.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::api::handlers::invitations::accept_for_user;
use crate::app::AppState;
use crate::auth::invitations as inv;
use crate::auth::url::url_encode;
use crate::error::{AppError, Result};
use crate::request::Session;
use crate::templates::filters;

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub token: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "invitations/status.html")]
struct InvitationStatusPage {
    title: &'static str,
    message: String,
}

fn status_page(status: StatusCode, title: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        InvitationStatusPage {
            title,
            message: message.into(),
        },
    )
        .into_response()
}

fn invalid_invitation_page() -> Response {
    status_page(
        StatusCode::GONE,
        "invitation invalid",
        "This invitation link is invalid, already settled, or has expired. \
         Ask the organization's owner to send a new one.",
    )
}

#[derive(Template, WebTemplate)]
#[template(path = "invitations/decline.html")]
struct DeclinePage {
    org_name: String,
    token: String,
}

/// GET /invitations/accept?token= — the emailed link. With a session the
/// invitation is redeemed right here (clicking the link is the consent);
/// without one the visitor bounces to /login which carries the token through
/// whichever sign-in method they pick.
pub async fn accept_landing(
    State(state): State<AppState>,
    session: Session,
    Query(q): Query<TokenQuery>,
) -> Result<Response> {
    let pool = state.require_db()?;
    let token = q.token.trim();
    let Some(user) = session.user.as_ref() else {
        return Ok(
            Redirect::to(&format!("/login?invitation={}", url_encode(token))).into_response(),
        );
    };
    let Some(row) = inv::find_pending_by_token(pool, token).await? else {
        return Ok(invalid_invitation_page());
    };
    match accept_for_user(&state, user.id, row).await {
        Ok(accepted) => {
            // Rotate the live session into the joined org — without this the
            // redirect lands on the OLD org's dashboard and the joined
            // banner (gated on active-org slug) never shows.
            if let Some(hash) = session.session_id_hash.as_deref()
                && let Err(err) =
                    crate::auth::session::set_active_org_by_hash(pool, hash, accepted.org_id).await
            {
                tracing::warn!(error = %err, "active-org rotation after accept failed");
            }
            Ok(
                Redirect::to(&format!("/?joined={}", url_encode(&accepted.org_slug)))
                    .into_response(),
            )
        }
        Err(AppError::Forbidden | AppError::ForbiddenCoded { .. }) => Ok(status_page(
            StatusCode::FORBIDDEN,
            "different address",
            "This invitation was sent to a different email address. Sign out \
             and sign in with the invited address to accept it.",
        )),
        Err(AppError::QuotaExceeded { .. }) => Ok(status_page(
            StatusCode::CONFLICT,
            "organization is full",
            "The organization has reached its member limit. Your invitation \
             stays valid — try again once a seat frees up.",
        )),
        Err(err) => {
            tracing::warn!(error = %err, "invitation accept landing failed");
            Ok(invalid_invitation_page())
        }
    }
}

/// GET /invitations/decline?token= — render-only (scanner-safe); the page's
/// button POSTs the actual decline.
pub async fn decline_landing(
    State(state): State<AppState>,
    Query(q): Query<TokenQuery>,
) -> Result<Response> {
    let pool = state.require_db()?;
    let token = q.token.trim();
    let Some(row) = inv::find_pending_by_token(pool, token).await? else {
        return Ok(invalid_invitation_page());
    };
    let org_name = crate::storage::orgs::get_org(pool, row.org_id)
        .await?
        .map(|o| o.name)
        .unwrap_or_else(|| "an organization".to_string());
    Ok(DeclinePage {
        org_name,
        token: token.to_string(),
    }
    .into_response())
}
