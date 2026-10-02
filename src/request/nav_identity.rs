//! Names the user and org a page was rendered for, in a cookie the nav cache
//! (`nav_cache.js`) reads before it replays the previous page's chrome. The
//! active org can change on the server or in another tab, which the replaying
//! tab cannot see.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};
use tower_cookies::cookie::SameSite;
use tower_cookies::{Cookie, Cookies};

use crate::domain::{OrgId, UserId};
use crate::request::host::renders_app_chrome;
use crate::request::{RequestState, Session};

const COOKIE_NAME: &str = "sm_nav_id";

pub async fn middleware(State(state): State<RequestState>, req: Request, next: Next) -> Response {
    if !is_page_load(&req) || !renders_app_chrome(&state, req.headers(), req.uri().path()) {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    let session = Session::from_request_parts(&mut parts, &state)
        .await
        .expect("Session extractor is infallible");
    let id = session
        .user_id()
        .map(|user| identity(user, session.active_org_id));
    // The handler extracts the same session; this saves it the lookup.
    parts.extensions.insert(session);
    let cookies = parts.extensions.get::<Cookies>().cloned();

    let response = next.run(Request::from_parts(parts, body)).await;

    if let (Some(cookies), Some(id)) = (cookies, id)
        && cookies.get(COOKIE_NAME).is_none_or(|c| c.value() != id)
    {
        let mut c = Cookie::new(COOKIE_NAME, id);
        c.set_path("/");
        c.set_same_site(SameSite::Lax);
        c.set_secure(state.cfg.auth.session.cookie_secure);
        cookies.add(c);
    }
    response
}

fn is_page_load(req: &Request) -> bool {
    req.method() == Method::GET
        && !req.headers().contains_key("hx-request")
        && req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|accept| accept.contains("text/html"))
}

fn identity(user: UserId, org: Option<OrgId>) -> String {
    let org = org.map(|o| o.0.to_string()).unwrap_or_default();
    let digest = Sha256::digest(format!("{}:{org}", user.0).as_bytes());
    hex::encode(&digest[..8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use uuid::Uuid;

    #[test]
    fn the_identity_moves_with_the_user_and_the_org() {
        let user = UserId(Uuid::from_u128(1));
        let a = Some(OrgId(Uuid::from_u128(2)));
        let b = Some(OrgId(Uuid::from_u128(3)));
        assert_eq!(identity(user, a), identity(user, a));
        assert_ne!(identity(user, a), identity(user, b));
        assert_ne!(identity(user, a), identity(UserId(Uuid::from_u128(4)), a));
        assert_ne!(identity(user, a), identity(user, None));
    }

    #[test]
    fn only_a_full_page_load_is_stamped() {
        let page = |method: Method, accept: &str, htmx: bool| {
            let mut b = Request::builder()
                .method(method)
                .header(header::ACCEPT, accept);
            if htmx {
                b = b.header("hx-request", "true");
            }
            is_page_load(&b.body(Body::empty()).unwrap())
        };
        assert!(page(Method::GET, "text/html,application/xhtml+xml", false));
        assert!(!page(Method::GET, "text/html", true));
        assert!(!page(Method::GET, "*/*", false));
        assert!(!page(Method::POST, "text/html", false));
    }
}
