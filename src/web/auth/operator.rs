//! Operator (instance-admin) authentication. A static bearer secret gates the
//! cross-tenant `/operator` surface — region + agent management. There is no
//! user or session: operator actions are infrastructure, not tenant data, so a
//! configured secret (env only) is the whole credential. An empty secret
//! disables the surface, which then 404s as if it did not exist.

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use secrecy::ExposeSecret;
use subtle::ConstantTimeEq;

use crate::app::AppState;
use crate::auth::sha256_hex;
use crate::error::codes;
use crate::error::{AppError, Result};
use crate::web::auth::bearer_from_headers;

/// Proof the request carried the operator admin token. A unit extractor — its
/// presence in a handler signature is the authorization.
pub struct OperatorAuth;

impl<S> FromRequestParts<S> for OperatorAuth
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self> {
        let app = AppState::from_ref(state);
        let expected = app.cfg.operator.admin_token.expose_secret();
        if expected.is_empty() {
            return Err(AppError::not_found(
                codes::OPERATOR_DISABLED,
                "operator surface is disabled",
            ));
        }
        let presented = bearer_from_headers(&parts.headers).unwrap_or("");
        // Compare fixed-length digests: `ct_eq` short-circuits on a length
        // mismatch, so hashing both sides first keeps the compare constant-time
        // regardless of the presented length (no secret-length timing leak).
        if bool::from(
            sha256_hex(presented)
                .as_bytes()
                .ct_eq(sha256_hex(expected).as_bytes()),
        ) {
            Ok(OperatorAuth)
        } else {
            Err(AppError::Unauthorized)
        }
    }
}
