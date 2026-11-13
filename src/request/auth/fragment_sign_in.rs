//! XHR follows redirects transparently, so htmx swaps a sign-in 303 into the
//! fragment it was refreshing. This answers it with `HX-Redirect` instead.

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::url::{safe_redirect_target, url_encode};

const SIGN_IN_PREFIX: &str = "/login?redirect_after=";
const HX_REQUEST: HeaderName = HeaderName::from_static("hx-request");
const HX_CURRENT_URL: HeaderName = HeaderName::from_static("hx-current-url");
const HX_REDIRECT: HeaderName = HeaderName::from_static("hx-redirect");

pub async fn middleware(req: Request, next: Next) -> Response {
    if !req.headers().contains_key(HX_REQUEST) {
        return next.run(req).await;
    }
    let page = page_on_screen(req.headers());
    let resp = next.run(req).await;
    if !resp.status().is_redirection() {
        return resp;
    }
    let to_sign_in = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|l| l.starts_with(SIGN_IN_PREFIX));
    if !to_sign_in {
        return resp;
    }
    // The handler's own return path names the fragment, which renders bare.
    let target = match page {
        Some(page) => format!("{SIGN_IN_PREFIX}{}", url_encode(&page)),
        None => "/login".to_owned(),
    };
    match HeaderValue::try_from(target) {
        Ok(v) => (StatusCode::UNAUTHORIZED, [(HX_REDIRECT, v)]).into_response(),
        Err(_) => resp,
    }
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
    use axum::Router;
    use axum::body::Body;
    use axum::response::Redirect;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route(
                "/targets/{id}/live",
                get(|| async { Redirect::to("/login?redirect_after=%2Ftargets%2F1%2Flive") }),
            )
            .route("/auth/logout", get(|| async { Redirect::to("/login") }))
            .route("/form", get(|| async { Redirect::to("/targets/1") }))
            .layer(axum::middleware::from_fn(middleware))
    }

    async fn send(path: &str, headers: &[(&str, &str)]) -> Response {
        let mut req = Request::builder().uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        app()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn hx(current: &str) -> [(&'static str, &str); 2] {
        [("hx-request", "true"), ("hx-current-url", current)]
    }

    #[tokio::test]
    async fn a_fragment_without_a_session_sends_the_tab_to_sign_in_for_its_page() {
        let resp = send(
            "/targets/1/live",
            &hx("https://app.test/targets/1?range=7d"),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().get(header::LOCATION).is_none());
        assert_eq!(
            resp.headers()[HX_REDIRECT],
            "/login?redirect_after=%2Ftargets%2F1%3Frange%3D7d"
        );
    }

    #[tokio::test]
    async fn a_page_load_keeps_the_plain_redirect() {
        let resp = send("/targets/1/live", &[]).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(resp.headers().get(HX_REDIRECT).is_none());
    }

    #[tokio::test]
    async fn a_missing_or_foreign_current_url_signs_in_without_a_return_path() {
        for headers in [
            vec![("hx-request", "true")],
            vec![("hx-request", "true"), ("hx-current-url", "//evil.test/x")],
        ] {
            let resp = send("/targets/1/live", &headers).await;
            assert_eq!(resp.headers()[HX_REDIRECT], "/login");
        }
    }

    #[tokio::test]
    async fn other_redirects_pass_through() {
        for path in ["/auth/logout", "/form"] {
            let resp = send(path, &hx("https://app.test/targets/1")).await;
            assert_eq!(resp.status(), StatusCode::SEE_OTHER, "{path}");
            assert!(resp.headers().get(HX_REDIRECT).is_none(), "{path}");
        }
    }
}
