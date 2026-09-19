//! CSRF guard for state-changing requests on the cookie-authenticated paths.
//!
//! Rule:
//! - GET / HEAD / OPTIONS pass through.
//! - Bearer-token requests pass through (cross-origin Authorization headers
//!   are not auto-attached by browsers, so no CSRF surface).
//! - Cookie-bearing state-changing requests must carry the custom header
//!   `X-Requested-With: uptimepage`. Browsers will only set custom headers
//!   on same-origin XHR/fetch, so attackers can't forge them from a third-party
//!   page. Missing-or-mismatched → 403 `CSRF_PROTECTION`.
//!
//! Constant-time header comparison (`subtle::ConstantTimeEq`) avoids leaking
//! length/byte-position info, even though the literal we compare against is a
//! public constant. The cost is negligible and the consistency is worth more
//! than the bytes.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::api::error::ApiErrorBody;
use crate::app::AppState;
use crate::error::codes;
use crate::web::auth::bearer_from_headers;

pub const CSRF_HEADER: HeaderName = HeaderName::from_static("x-requested-with");
pub const CSRF_HEADER_VALUE: &str = "uptimepage";

/// Public paths whose authority is a per-request token (in the query, or a
/// double-submit confirmation form), not the session cookie. A forged cross-site
/// POST cannot carry a valid token, so the CSRF header check adds nothing and
/// would only break the plain-HTML confirmation form these serve to a recipient
/// who is also a signed-in user. `/auth/magic-link/verify` carries its own
/// double-submit nonce and `/auth/magic-link/code` a SameSite=Lax cookie a
/// cross-site POST cannot send; the others authenticate via an HMAC query
/// token.
const TOKEN_AUTHENTICATED_PATHS: &[&str] = &[
    "/alert-channel/stop",
    "/incident/ack",
    "/subscribe/unsubscribe",
    "/auth/magic-link/verify",
    "/auth/magic-link/code",
];

/// Tower middleware that enforces the rule documented at module level.
pub async fn middleware(State(state): State<AppState>, req: Request<Body>, next: Next) -> Response {
    if !is_state_changing(req.method()) {
        return next.run(req).await;
    }
    if TOKEN_AUTHENTICATED_PATHS.contains(&req.uri().path()) {
        return next.run(req).await;
    }
    if bearer_from_headers(req.headers()).is_some() {
        return next.run(req).await;
    }
    let cookie_name = state.cfg.auth.session.cookie_name.as_str();
    if !has_session_cookie(&req, cookie_name) {
        // No auth cookie → request will fail auth anyway. CSRF guard exists
        // to protect *authenticated* state-changing requests; skip here so
        // unauthenticated POSTs surface as 401 from the handler rather than
        // a CSRF false-positive.
        return next.run(req).await;
    }
    if header_matches(&req, &CSRF_HEADER, CSRF_HEADER_VALUE) {
        return next.run(req).await;
    }
    reject()
}

fn is_state_changing(method: &Method) -> bool {
    !matches!(
        method,
        &Method::GET | &Method::HEAD | &Method::OPTIONS | &Method::TRACE
    )
}

fn has_session_cookie(req: &Request<Body>, cookie_name: &str) -> bool {
    req.headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|raw| {
            raw.split(';')
                .map(str::trim)
                .any(|kv| kv.starts_with(cookie_name) && kv[cookie_name.len()..].starts_with('='))
        })
}

fn header_matches(req: &Request<Body>, name: &HeaderName, expected: &str) -> bool {
    let Some(value) = req.headers().get(name).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    value.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn reject() -> Response {
    let body = ApiErrorBody::new(
        codes::CSRF_PROTECTION,
        "missing or invalid X-Requested-With",
    );
    let mut resp = (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_changing_classification() {
        assert!(!is_state_changing(&Method::GET));
        assert!(!is_state_changing(&Method::HEAD));
        assert!(!is_state_changing(&Method::OPTIONS));
        assert!(is_state_changing(&Method::POST));
        assert!(is_state_changing(&Method::PUT));
        assert!(is_state_changing(&Method::PATCH));
        assert!(is_state_changing(&Method::DELETE));
    }

    #[test]
    fn cookie_lookup_matches_name_exactly() {
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert("cookie", "_sm_session=tok; other=1".parse().unwrap());
        assert!(has_session_cookie(&req, "_sm_session"));
        assert!(!has_session_cookie(&req, "_sm_session_other"));
        assert!(!has_session_cookie(&req, "_sm"));
    }

    #[test]
    fn token_authenticated_paths_are_exempt() {
        assert!(TOKEN_AUTHENTICATED_PATHS.contains(&"/alert-channel/stop"));
        assert!(TOKEN_AUTHENTICATED_PATHS.contains(&"/incident/ack"));
        assert!(TOKEN_AUTHENTICATED_PATHS.contains(&"/subscribe/unsubscribe"));
        assert!(TOKEN_AUTHENTICATED_PATHS.contains(&"/auth/magic-link/verify"));
        assert!(TOKEN_AUTHENTICATED_PATHS.contains(&"/auth/magic-link/code"));
        assert!(!TOKEN_AUTHENTICATED_PATHS.contains(&"/api/v1/notification-channels"));
    }

    #[test]
    fn header_matches_is_constant_time_correct() {
        let mut req = Request::new(Body::empty());
        req.headers_mut()
            .insert(&CSRF_HEADER, "uptimepage".parse().unwrap());
        assert!(header_matches(&req, &CSRF_HEADER, CSRF_HEADER_VALUE));
        req.headers_mut()
            .insert(&CSRF_HEADER, "something-else".parse().unwrap());
        assert!(!header_matches(&req, &CSRF_HEADER, CSRF_HEADER_VALUE));
    }
}
