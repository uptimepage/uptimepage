//! XHR follows redirects transparently, so htmx swaps a sign-in 303 into the
//! fragment it was refreshing, and it drops a 401 without a word. On the
//! operator surface both are answered with `HX-Redirect` to sign-in instead.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::auth::url::{safe_redirect_target, url_encode};
use crate::request::RequestState;
use crate::request::host::renders_app_chrome;

const SIGN_IN_PREFIX: &str = "/login?redirect_after=";
const HX_REQUEST: HeaderName = HeaderName::from_static("hx-request");
const HX_CURRENT_URL: HeaderName = HeaderName::from_static("hx-current-url");
const HX_REDIRECT: HeaderName = HeaderName::from_static("hx-redirect");

pub async fn middleware(State(state): State<RequestState>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    // A private status page answers 401 too, and its readers have no operator
    // account to sign in to.
    if !headers.contains_key(HX_REQUEST) || !renders_app_chrome(&state, headers, req.uri().path()) {
        return next.run(req).await;
    }
    let page = page_on_screen(headers);
    to_whole_tab(next.run(req).await, page)
}

fn to_whole_tab(mut resp: Response, page: Option<String>) -> Response {
    if !signed_out(&resp) {
        return resp;
    }
    // The handler's own return path names the fragment, which renders bare.
    let target = match page {
        Some(page) => format!("{SIGN_IN_PREFIX}{}", url_encode(&page)),
        None => "/login".to_owned(),
    };
    let Ok(target) = HeaderValue::try_from(target) else {
        return resp;
    };
    // Edited in place: the Set-Cookie clearing an expired session rides on it.
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    let headers = resp.headers_mut();
    headers.remove(header::LOCATION);
    headers.insert(HX_REDIRECT, target);
    resp
}

fn signed_out(resp: &Response) -> bool {
    resp.status() == StatusCode::UNAUTHORIZED
        || resp.status().is_redirection()
            && resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|l| l.starts_with(SIGN_IN_PREFIX))
}

fn page_on_screen(headers: &HeaderMap) -> Option<String> {
    let url = headers.get(HX_CURRENT_URL)?.to_str().ok()?;
    let uri = Uri::try_from(url).ok()?;
    let path = uri.path_and_query()?.as_str();
    safe_redirect_target(path).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::{IntoResponse, Redirect};

    fn page(current: &str) -> Option<String> {
        let mut headers = HeaderMap::new();
        headers.insert(HX_CURRENT_URL, HeaderValue::from_str(current).unwrap());
        page_on_screen(&headers)
    }

    fn sign_in() -> Response {
        Redirect::to("/login?redirect_after=%2Ftargets%2F1%2Flive").into_response()
    }

    #[test]
    fn the_return_path_is_the_page_on_screen() {
        assert_eq!(
            page("https://app.test/targets/1?range=7d").as_deref(),
            Some("/targets/1?range=7d")
        );
        assert_eq!(page("//evil.test/x"), None);
        assert_eq!(page_on_screen(&HeaderMap::new()), None);
    }

    #[test]
    fn a_sign_in_redirect_or_a_401_sends_the_tab_to_sign_in() {
        for resp in [sign_in(), StatusCode::UNAUTHORIZED.into_response()] {
            let out = to_whole_tab(resp, Some("/targets/1?range=7d".into()));
            assert_eq!(out.status(), StatusCode::UNAUTHORIZED);
            assert!(out.headers().get(header::LOCATION).is_none());
            assert_eq!(
                out.headers()[HX_REDIRECT],
                "/login?redirect_after=%2Ftargets%2F1%3Frange%3D7d"
            );
        }
    }

    #[test]
    fn the_handlers_other_headers_survive() {
        let mut resp = sign_in();
        resp.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_static("_sm_session=; Max-Age=0"),
        );
        let out = to_whole_tab(resp, None);
        assert_eq!(out.headers()[header::SET_COOKIE], "_sm_session=; Max-Age=0");
    }

    #[test]
    fn without_a_page_on_screen_it_signs_in_with_no_return_path() {
        let out = to_whole_tab(sign_in(), None);
        assert_eq!(out.headers()[HX_REDIRECT], "/login");
    }

    #[test]
    fn other_answers_pass_through() {
        for resp in [
            Redirect::to("/login").into_response(),
            Redirect::to("/targets/1").into_response(),
            StatusCode::FORBIDDEN.into_response(),
            StatusCode::OK.into_response(),
        ] {
            let status = resp.status();
            let out = to_whole_tab(resp, Some("/".into()));
            assert_eq!(out.status(), status);
            assert!(out.headers().get(HX_REDIRECT).is_none());
        }
    }
}
