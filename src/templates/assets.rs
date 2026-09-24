//! Static asset serving with content-addressed cache busting.
//!
//! One source of truth: [`url`] maps a logical path to a cache-busting URL.
//! JS is bundled and content-hashed by esbuild (build.rs); the manifest maps
//! `js/ui/x.js` → `js/ui/x-<hash>.js`, so the served filename changes iff its
//! bundle (entry + every module it imports) does. Everything else gets a
//! `?v=<hash>` query off the file's SHA-256. Either way the `immutable`
//! response header is *truthful* rather than a year-long promise the server
//! can't keep.
//!
//! Templates must reference assets only through the `asset` askama filter
//! (`{{ "css/app.css"|asset }}`, registered in `crate::templates::filters`).
//! A raw `/static/...` literal in a template is a bug the
//! `no_raw_static_refs_in_templates` test fails on, so cache-busting
//! can never silently regress.
//!
//! Release bakes the files into the binary; debug reads them from disk, so a
//! `just watch-js` rebuild is served live.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use axum::extract::{Path, RawQuery};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

/// Static assets embedded into the release binary. In debug builds
/// rust-embed reads from the filesystem so edits show up without rebuilding.
#[derive(Embed)]
#[folder = "static/"]
struct StaticAssets;

/// Cache-bust fingerprint for non-JS assets (and JS when there's no manifest).
/// 4 bytes only has to change when the bytes do, not resist collision. The
/// hashed-asset build (`assets_manifest`, set by build.rs in release) caches the
/// map; otherwise it recomputes per call so a `just watch-js` rebuild is picked
/// up live. The cfg is PROFILE-driven, so it always matches the build layout.
#[cfg(assets_manifest)]
static FINGERPRINTS: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    StaticAssets::iter()
        .filter_map(|p| Some((p.to_string(), fingerprint(&p)?)))
        .collect()
});

fn fingerprint(path: &str) -> Option<String> {
    // Reuse rust-embed's baked SHA-256 instead of a second hash pass.
    let hash = StaticAssets::get(path)?.metadata.sha256_hash();
    Some(hex::encode(&hash[..4]))
}

#[cfg(assets_manifest)]
fn fingerprint_cached(path: &str) -> Option<String> {
    FINGERPRINTS.get(path).cloned()
}

#[cfg(not(assets_manifest))]
fn fingerprint_cached(path: &str) -> Option<String> {
    fingerprint(path)
}

/// Logical JS path → hashed served path (release only). Empty in debug, so
/// [`url`] falls back to the fingerprint there.
static MANIFEST: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    StaticAssets::get("js/manifest.json")
        .and_then(|f| serde_json::from_slice(&f.data).ok())
        .unwrap_or_default()
});

/// Manifest values, so [`serve`] can mark hashed bundles `immutable`.
static HASHED_PATHS: LazyLock<HashSet<String>> =
    LazyLock::new(|| MANIFEST.values().cloned().collect());

/// Cache-busting URL for a logical asset path. The only sanctioned way to
/// reference a static asset (enforced by `no_raw_static_refs_in_templates`).
pub fn url(path: &str) -> String {
    if let Some(hashed) = MANIFEST.get(path) {
        return format!("/static/{hashed}");
    }
    match fingerprint_cached(path) {
        Some(fp) => format!("/static/{path}?v={fp}"),
        None => format!("/static/{path}"),
    }
}

async fn favicon_ico() -> Response {
    root_icon("img/favicon.ico")
}

async fn apple_touch_icon() -> Response {
    root_icon("img/apple-touch-icon.png")
}

fn root_icon(asset: &str) -> Response {
    let Some(content) = StaticAssets::get(asset) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mime = mime_guess::from_path(asset).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_owned()),
            (header::CACHE_CONTROL, "public, max-age=86400".to_owned()),
        ],
        content.data,
    )
        .into_response()
}

pub async fn serve(
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let Some(content) = StaticAssets::get(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    let mime = mime_guess::from_path(&path).first_or_octet_stream();

    // `immutable` is only honest for a content-addressed name (`?v=` query or a
    // hashed bundle); a bare URL gets a short cache so a change can't hide.
    let versioned = query
        .as_deref()
        .is_some_and(|q| q.split('&').any(|kv| kv == "v" || kv.starts_with("v=")));
    let cache_control = if versioned || HASHED_PATHS.contains(&path) {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=300"
    };

    let headers_out = [
        (header::CONTENT_TYPE, mime.as_ref().to_owned()),
        (header::CACHE_CONTROL, cache_control.to_owned()),
    ];
    let Some(brotli) = brotli_sibling(&path) else {
        return (headers_out, content.data).into_response();
    };
    let vary = (header::VARY, "accept-encoding".to_owned());
    if accepts_brotli(&headers) {
        let encoding = (header::CONTENT_ENCODING, "br".to_owned());
        (headers_out, [vary, encoding], brotli.data).into_response()
    } else {
        (headers_out, [vary], content.data).into_response()
    }
}

/// Release only: debug reads assets from disk, where a `.br` left by an earlier
/// release build would shadow every later edit.
#[cfg(assets_manifest)]
fn brotli_sibling(path: &str) -> Option<rust_embed::EmbeddedFile> {
    StaticAssets::get(&format!("{path}.br"))
}

#[cfg(not(assets_manifest))]
fn brotli_sibling(_path: &str) -> Option<rust_embed::EmbeddedFile> {
    None
}

/// True when `Accept-Encoding` lists `br` without `q=0`.
fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|item| {
            let mut parts = item.split(';');
            let is_br = parts
                .next()
                .is_some_and(|coding| coding.trim().eq_ignore_ascii_case("br"));
            is_br
                && parts.all(|param| {
                    param
                        .split_once('=')
                        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                        .and_then(|(_, q)| q.trim().parse::<f32>().ok())
                        .is_none_or(|q| q > 0.0)
                })
        })
}

/// Mount the fingerprinted static-asset route on the given router. Two
/// callers need an identical declaration — the operator app router and
/// the marketing router — and they must agree on the path and handler so
/// `{{ "css/app.css"|asset }}` resolves on both hosts. Funnel both
/// through here so the path can never drift.
pub fn mount_static<S>(router: axum::Router<S>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    use axum::routing::get;
    // A hashed-asset build with an empty manifest means the esbuild step did not
    // run — every JS asset would 404. Fail fast at startup instead.
    #[cfg(assets_manifest)]
    assert!(
        !MANIFEST.is_empty(),
        "JS asset manifest is empty — the esbuild build step did not run"
    );
    router
        .route("/static/{*path}", get(serve))
        .route("/favicon.ico", get(favicon_ico))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn url_is_version_pinned_for_known_assets() {
        let u = url("css/app.css");
        assert!(
            u.starts_with("/static/css/app.css?v="),
            "expected a ?v= fingerprint, got {u}"
        );
        // Same bytes → same fingerprint (stable across calls).
        assert_eq!(u, url("css/app.css"));
    }

    #[test]
    fn url_unknown_asset_falls_back_to_bare_path() {
        assert_eq!(url("does/not/exist.js"), "/static/does/not/exist.js");
    }

    /// Split a `url()` result into the `serve` path and optional query.
    fn split_served(u: &str) -> (String, Option<String>) {
        let rest = u.strip_prefix("/static/").expect("static url");
        match rest.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (rest.to_string(), None),
        }
    }

    #[tokio::test]
    async fn js_resolves_to_an_existing_immutable_asset() {
        // Profile-agnostic: release serves a hashed name, debug a `?v=` query;
        // either way it must point at a real file and cache as immutable.
        let (path, query) = split_served(&url("js/ui/api_form.js"));
        assert!(
            StaticAssets::get(&path).is_some(),
            "JS url points at missing asset {path}"
        );
        let resp = serve(Path(path), RawQuery(query), HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable",
        );
    }

    /// Every `{{ "js/…"|asset }}` reference in a template must resolve to a file
    /// that actually exists — catches a missing manifest entry or an unbuilt
    /// bundle before it 404s in the browser.
    #[test]
    fn every_template_js_ref_resolves() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/templates");
        let re = regex::Regex::new(r#""(js/[^"]+\.js)"\s*\|\s*asset"#).unwrap();
        let mut missing = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read templates dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("html") {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("read template");
                for cap in re.captures_iter(&body) {
                    let logical = &cap[1];
                    let (served, _) = split_served(&url(logical));
                    if StaticAssets::get(&served).is_none() {
                        missing.push(format!("{logical} (in {})", path.display()));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "unresolvable JS asset refs: {missing:?}"
        );
    }

    #[tokio::test]
    async fn versioned_request_is_immutable() {
        let resp = serve(
            Path("css/app.css".into()),
            RawQuery(Some("v=deadbeef".into())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/css",
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable",
        );
        let body = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert!(std::str::from_utf8(&body).unwrap().contains("tailwindcss"));
    }

    #[test]
    fn accepts_brotli_reads_accept_encoding() {
        let with = |v: &'static str| {
            let mut h = HeaderMap::new();
            h.insert(header::ACCEPT_ENCODING, v.parse().unwrap());
            accepts_brotli(&h)
        };
        assert!(with("gzip, deflate, br, zstd"));
        assert!(with("br;q=0.5"));
        assert!(with("BR"));
        assert!(!with("gzip, zstd"));
        assert!(!with("br;q=0"));
        assert!(!with("br; Q=0"));
        assert!(!with("brotli"));
        assert!(!accepts_brotli(&HeaderMap::new()));
    }

    #[tokio::test]
    async fn unversioned_request_is_short_lived() {
        let resp = serve(Path("css/app.css".into()), RawQuery(None), HeaderMap::new()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300",
        );
    }

    #[tokio::test]
    async fn serves_htmx_js() {
        let resp = serve(
            Path("js/htmx.min.js".into()),
            RawQuery(None),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let mime = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(mime.starts_with("text/javascript") || mime.starts_with("application/javascript"));
    }

    #[tokio::test]
    async fn root_icons_serve_image_bytes() {
        for asset in ["img/favicon.ico", "img/apple-touch-icon.png"] {
            assert!(
                StaticAssets::get(asset).is_some(),
                "root icon {asset} not embedded — was it committed?"
            );
        }
        let ico = favicon_ico().await;
        assert_eq!(ico.status(), StatusCode::OK);
        assert!(
            ico.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("image/"),
        );
        let apple = apple_touch_icon().await;
        assert_eq!(apple.status(), StatusCode::OK);
        assert_eq!(
            apple.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png",
        );
    }

    #[tokio::test]
    async fn missing_asset_returns_404() {
        let resp = serve(
            Path("does/not/exist".into()),
            RawQuery(None),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Cache-busting only works if every template asset reference goes
    /// through the `asset` filter. A raw `/static/...` literal bypasses the
    /// fingerprint, so fail loudly the moment one appears.
    #[test]
    fn no_raw_static_refs_in_templates() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/templates");
        let mut offenders = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read templates dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("html") {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("read template");
                if body.contains("/static/") {
                    offenders.push(path.display().to_string());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "templates must reference assets via the `asset` filter, not a \
             raw /static/ path. Offenders: {offenders:?}"
        );
    }
}
