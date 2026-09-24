//! The load-bearing decoupling check: every marketing route serves 2xx
//! when no Postgres / ClickHouse handle is in scope. The marketing
//! module takes its own `MarketingCfg` (not `AppState`), so this test
//! constructs that config directly and exercises each route through the
//! returned `axum::Router` — no pool, no client, no `AppState`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::util::ServiceExt;

use uptimepage::domain::check::CheckSpec;
use uptimepage::marketing::config::META_DESCRIPTION;
use uptimepage::marketing::{self, MarketingCfg, blog, changelog, landings, tools};

fn router() -> axum::Router {
    marketing::router(MarketingCfg {
        app_url: "https://app.uptimepage.dev".into(),
        canonical_origin: "https://uptimepage.dev".into(),
        blog_enabled: true,
        mcp_url: Some("https://mcp.uptimepage.dev/mcp".into()),
        checkout_open: false,
        trusted_proxies: Vec::new(),
    })
}

async fn get(path: &str) -> (StatusCode, String, axum::http::HeaderMap) {
    let resp = router()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::HOST, "uptimepage.dev")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router call");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("collect body");
    let body = String::from_utf8(bytes.to_vec()).unwrap_or_default();
    (status, body, headers)
}

#[tokio::test]
async fn landing_renders_without_db() {
    let (status, body, headers) = get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Uptimepage"));
    assert!(
        body.contains("https://app.uptimepage.dev/login"),
        "CTA should link to app_url"
    );
    let cache_control = headers
        .get(header::CACHE_CONTROL)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(
        cache_control.contains("max-age="),
        "marketing landing must set Cache-Control, got {cache_control:?}"
    );
    assert!(
        headers.contains_key(header::ETAG),
        "marketing landing must set a strong ETag"
    );
}

#[tokio::test]
async fn pricing_renders_without_db() {
    let (status, body, headers) = get("/pricing").await;
    assert_eq!(status, StatusCode::OK);
    // Real content, not the render-failure comment fallback served as a 200.
    assert!(body.contains("founding"), "pricing must render the tiers");
    assert!(
        body.contains("1,000"),
        "founding total should render with a thousands separator"
    );
    assert!(
        body.contains("https://app.uptimepage.dev/login"),
        "pricing CTA should link to app_url"
    );
    assert!(
        !body.contains("render failed"),
        "pricing render must not fall back to the error comment"
    );
    assert!(
        headers.contains_key(header::ETAG),
        "pricing must set a strong ETag"
    );
}

#[tokio::test]
async fn architecture_page_renders_and_is_csp_clean() {
    let (status, body, headers) = get("/architecture").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("How Uptimepage is built"),
        "architecture page must render its hero"
    );
    assert!(
        body.contains(r#"id="cols""#) && body.contains(r#"id="wires""#),
        "architecture page must render the map skeleton the script populates"
    );
    assert!(
        body.contains("/static/js/architecture/flows.js")
            && body.contains("/static/architecture/flows.css"),
        "map styles and script must be external assets, not inline"
    );
    // The prod marketing CSP forbids inline styles and scripts. Guard it here
    // so a future edit that inlines either fails the build, not production.
    assert!(
        !body.contains("<script>"),
        "no inline script (CSP: script-src 'self')"
    );
    assert!(
        !body.contains("<style"),
        "no inline style block (CSP: style-src 'self')"
    );
    assert!(
        !body.contains("style="),
        "no inline style attributes (CSP: style-src 'self')"
    );
    assert!(
        body.contains(
            r#"property="og:image" content="https://uptimepage.dev/static/marketing/og-architecture.png""#
        ),
        "og:image must be the dedicated architecture card, rooted at the origin"
    );
    assert!(
        body.contains(r#"href="https://app.uptimepage.dev""#),
        "the Start free CTA must link to app_url"
    );
    assert!(
        !body.contains("render failed"),
        "must not serve the render-error fallback"
    );
    assert!(
        headers.contains_key(header::ETAG),
        "architecture page must set a strong ETag"
    );
}

#[tokio::test]
async fn blog_index_renders_without_db() {
    let (status, body, _) = get("/blog").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("from the workshop"));
}

#[tokio::test]
async fn known_post_renders_without_db() {
    let (status, body, _) = get("/blog/boring-uptime").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Why your uptime monitor should be boring"));
}

#[tokio::test]
async fn post_with_a_figure_loads_only_its_own_script() {
    let (status, body, _) = get("/blog/stop-false-uptime-alerts").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"class="mk-embed-quorum""#),
        "the figure's mount must survive sanitising all the way to the page"
    );
    assert!(
        body.contains("/static/js/marketing/quorum.js"),
        "a post that embeds the figure must load the script that fills it"
    );

    // Two figures on one post: each mount survives sanitising and pulls in
    // exactly the script that fills it.
    let (status, body, _) = get("/blog/cron-jobs-fail-silently").await;
    assert_eq!(status, StatusCode::OK);
    for (mount, script) in [
        ("mk-embed-blind", "blind.js"),
        ("mk-embed-grace", "grace.js"),
    ] {
        assert!(
            body.contains(&format!(r#"class="{mount}""#)),
            "{mount}: the figure's mount must survive sanitising all the way to the page"
        );
        assert!(
            body.contains(&format!("/static/js/marketing/{script}")),
            "{script}: a post that embeds the figure must load the script that fills it"
        );
    }
    assert!(
        !body.contains("quorum.js"),
        "a post must load only the figures it embeds"
    );

    let (status, body, _) = get("/blog/monitor-the-login-not-the-login-page").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"class="mk-embed-flow-break""#),
        "the figure's mount must survive sanitising all the way to the page"
    );
    assert!(
        body.contains("/static/js/marketing/flow_break.js"),
        "a post that embeds the figure must load the script that fills it"
    );

    let (status, body, _) = get("/blog/your-login-test-never-runs-in-production").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"class="mk-embed-ci-vs-prod""#),
        "the figure's mount must survive sanitising all the way to the page"
    );
    assert!(
        body.contains("/static/js/marketing/ci_vs_prod.js"),
        "a post that embeds the figure must load the script that fills it"
    );
    assert!(
        !body.contains("flow_break.js"),
        "a post must load only the figure it embeds"
    );

    let (_, other, _) = get("/blog/boring-uptime").await;
    assert!(
        !other.contains("quorum.js")
            && !other.contains("flow_break.js")
            && !other.contains("ci_vs_prod.js")
            && !other.contains("blind.js")
            && !other.contains("grace.js"),
        "a post without the figure must not pay for its script"
    );
}

#[tokio::test]
async fn landing_with_figures_mounts_them_and_loads_only_its_own_scripts() {
    let (status, body, _) = get("/browser-login-monitoring").await;
    assert_eq!(status, StatusCode::OK);
    for mount in [
        "mk-embed-flow-gap",
        "mk-embed-flow-record",
        "mk-embed-flow-evidence",
    ] {
        assert!(
            body.contains(mount),
            "{mount} must reach the page as a mount"
        );
    }
    for script in ["flow_gap.js", "flow_record.js", "flow_evidence.js"] {
        assert!(
            body.contains(script),
            "a page mounting a figure must load the script that fills it: {script}"
        );
    }

    let (_, other, _) = get("/uptime-monitoring-for-developers").await;
    assert!(
        !other.contains("mk-embed-flow-"),
        "a landing without figures must not pay for their scripts"
    );
}

#[tokio::test]
async fn unknown_blog_post_returns_branded_404() {
    let (status, body, _) = get("/blog/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Not Found"));
}

#[tokio::test]
async fn arbitrary_path_returns_branded_404() {
    let (status, body, _) = get("/this-page-does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Not Found"));
}

#[tokio::test]
async fn robots_txt_points_at_sitemap() {
    let (status, body, headers) = get("/robots.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("User-agent: *"));
    assert!(body.contains("https://uptimepage.dev/sitemap.xml"));
    assert!(
        body.contains("Content-Signal: search=yes, ai-input=yes, ai-train=yes"),
        "AI usage preferences must be declared, got {body:?}"
    );
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/plain"), "got {ct:?}");
}

/// A crawler that walks one of these spends our egress on a stranger's host,
/// and the header checker opens a socket per redirect hop.
#[tokio::test]
async fn robots_disallows_every_probe_endpoint() {
    let (status, body, _) = get("/robots.txt").await;
    assert_eq!(status, StatusCode::OK);
    for probe in [
        "/tools/ssl-certificate-checker/probe",
        "/tools/http-header-checker/probe",
    ] {
        assert!(
            body.contains(&format!("Disallow: {probe}")),
            "{probe} must be disallowed, got {body:?}"
        );
    }
}

async fn get_as_markdown(path: &str) -> (StatusCode, String, axum::http::HeaderMap) {
    let resp = router()
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::HOST, "uptimepage.dev")
                .header(header::ACCEPT, "text/markdown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router call");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .expect("collect body");
    (
        status,
        String::from_utf8(bytes.to_vec()).unwrap_or_default(),
        headers,
    )
}

#[tokio::test]
async fn agents_get_the_markdown_source_of_a_doc() {
    let (status, body, headers) = get_as_markdown("/docs/api").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "text/markdown; charset=utf-8"
    );
    assert_eq!(headers.get(header::VARY).unwrap(), "accept");
    assert!(headers.contains_key("x-markdown-tokens"));
    assert!(
        body.starts_with("# REST API"),
        "got {:?}",
        &body[..40.min(body.len())]
    );
    assert!(!body.contains("<html"), "must not be the rendered page");
}

#[tokio::test]
async fn agents_get_markdown_for_posts_and_the_landing_page() {
    let (_, post, _) = get_as_markdown("/blog/boring-uptime").await;
    assert!(post.starts_with("# Why your uptime monitor should be boring"));

    let (_, landing, _) = get_as_markdown("/").await;
    assert!(landing.starts_with("# Uptimepage"));
}

#[tokio::test]
async fn browsers_still_get_html_and_a_vary_header() {
    let (status, body, headers) = get("/docs/api").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<html"), "browsers keep the rendered page");
    assert_eq!(
        headers.get(header::VARY).unwrap(),
        "accept",
        "caches must key both representations"
    );
    assert!(!headers.contains_key("x-markdown-tokens"));
}

#[tokio::test]
async fn compression_does_not_drop_the_accept_vary() {
    let resp = router()
        .oneshot(
            Request::builder()
                .uri("/docs/api")
                .header(header::HOST, "uptimepage.dev")
                .header(header::ACCEPT, "text/markdown")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router call");
    let vary: Vec<_> = resp
        .headers()
        .get_all(header::VARY)
        .iter()
        .map(|v| v.to_str().unwrap().to_ascii_lowercase())
        .collect();
    assert!(
        vary.iter()
            .any(|v| v.contains("accept") && !v.contains("accept-encoding")
                || v.split(',').any(|p| p.trim() == "accept")),
        "Accept dropped from Vary: {vary:?}"
    );
}

#[tokio::test]
async fn api_catalog_is_served_as_a_linkset() {
    let (status, body, headers) = get("/.well-known/api-catalog").await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("application/linkset+json"), "got {ct:?}");
    assert!(ct.contains("rfc9727"), "profile parameter missing: {ct:?}");
    let doc: serde_json::Value = serde_json::from_str(&body).expect("linkset json");
    assert!(doc["linkset"][0]["item"].is_array());
}

#[tokio::test]
async fn security_txt_answers_on_its_canonical_host() {
    let (status, body, headers) = get("/.well-known/security.txt").await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/plain"), "got {ct:?}");
    let cache = headers
        .get(header::CACHE_CONTROL)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(cache.contains("max-age="), "got {cache:?}");
    assert!(
        body.contains("Canonical: https://uptimepage.dev/.well-known/security.txt"),
        "Canonical must name this host: {body}"
    );
}

#[tokio::test]
async fn apex_points_at_the_mcp_hosts_server_card() {
    let (status, _, headers) = get("/.well-known/mcp/server-card.json").await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        headers.get(header::LOCATION).unwrap(),
        "https://mcp.uptimepage.dev/.well-known/mcp/server-card.json"
    );
}

#[tokio::test]
async fn pages_advertise_the_catalog_over_link() {
    let (_, _, headers) = get("/").await;
    let link = headers
        .get(header::LINK)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    assert!(
        link.contains("<https://uptimepage.dev/.well-known/api-catalog>; rel=\"api-catalog\""),
        "got {link:?}"
    );
    assert!(link.contains("rel=\"service-desc\""), "got {link:?}");
    assert!(link.contains("rel=\"service-doc\""), "got {link:?}");

    let (_, _, docs) = get("/docs/api").await;
    assert!(docs.contains_key(header::LINK), "doc pages need it too");
}

#[tokio::test]
async fn sitemap_lists_blog_and_landing() {
    let (status, body, headers) = get("/sitemap.xml").await;
    assert_eq!(status, StatusCode::OK);
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("application/xml"), "got {ct:?}");
    assert!(body.contains("<urlset"), "sitemap must be a urlset");
    assert!(
        body.contains("<loc>https://uptimepage.dev</loc>"),
        "landing must be in sitemap"
    );
    assert!(
        body.contains("<loc>https://uptimepage.dev/blog</loc>"),
        "blog index must be in sitemap"
    );
    assert!(
        body.contains("<loc>https://uptimepage.dev/pricing</loc>"),
        "pricing page must be in sitemap"
    );
    assert!(
        body.contains("<loc>https://uptimepage.dev/blog/boring-uptime</loc>"),
        "published post must be in sitemap"
    );
}

#[tokio::test]
async fn llms_txt_renders() {
    let (status, body, _) = get("/llms.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("# Uptimepage"));
    assert!(body.contains("## Use cases"), "must list landing pages");
    assert!(
        body.contains("https://uptimepage.dev/status-page-for-saas"),
        "must link a landing page"
    );
    assert!(
        body.contains("https://uptimepage.dev/llms-full.txt"),
        "must point to the full-text companion"
    );
    assert!(
        body.contains("https://mcp.uptimepage.dev/mcp"),
        "must surface the MCP server"
    );
    assert!(
        body.contains("registry.terraform.io/providers/uptimepage/uptimepage"),
        "must surface the Terraform provider"
    );
}

#[tokio::test]
async fn dns_lookup_tool_renders_without_db() {
    let (status, body, _headers) = get("/tools/dns-lookup").await;
    assert_eq!(status, StatusCode::OK);
    // No server-side lookup exists, so the value the page must carry with JS
    // off is the reference: every offered record type and what it answers.
    for ty in ["A", "AAAA", "CNAME", "MX", "NS", "TXT", "SOA", "CAA"] {
        assert!(
            body.contains(&format!(">{ty}<")),
            "missing record type {ty}"
        );
    }
    assert!(
        body.contains("js/marketing/dns_lookup"),
        "must load the lookup script"
    );
    assert!(
        body.contains("WebApplication") && body.contains("isAccessibleForFree"),
        "must carry the free WebApplication schema"
    );
    assert!(
        body.contains("FAQPage"),
        "must carry the FAQ schema the visible copy mirrors"
    );
    assert!(
        !body.contains("cloudflare-dns.com"),
        "resolver endpoints belong in the script, not the cached HTML"
    );
}

#[tokio::test]
async fn uptime_sla_tool_renders_without_db() {
    let (status, body, headers) = get("/tools/uptime-sla-calculator").await;
    assert_eq!(status, StatusCode::OK);
    // Server-rendered default state: 99.9% resolves to these before any JS.
    assert!(
        body.contains("43m 12s"),
        "must render the 99.9% monthly downtime server-side"
    );
    assert!(
        body.contains("8h 45m 36s"),
        "must render the 99.9% yearly downtime in the reference table"
    );
    assert!(
        body.contains("js/marketing/uptime_sla"),
        "must load the calculator script"
    );
    assert!(
        body.contains("uptime percentage calculator"),
        "must cover the secondary keyword"
    );
    assert!(
        body.contains("99.8611%"),
        "reverse widget must render its default uptime server-side"
    );
    assert!(
        body.contains("WebApplication") && body.contains("isAccessibleForFree"),
        "must carry the free WebApplication schema"
    );
    assert!(
        body.contains("https://app.uptimepage.dev/login"),
        "tool CTA should link to app_url"
    );
    assert!(
        headers.contains_key(header::ETAG),
        "tool page must set a strong ETag"
    );
}

#[tokio::test]
async fn cron_tool_renders_without_db() {
    let (status, body, headers) = get("/tools/cron-expression-generator").await;
    assert_eq!(status, StatusCode::OK);
    // Default expression + its authored description render server-side.
    assert!(
        body.contains("*/15 9-17 * * 1-5"),
        "must render the default expression"
    );
    assert!(
        body.contains("Monday through Friday"),
        "must render the default plain-English description server-side"
    );
    assert!(
        body.contains("js/marketing/cron"),
        "must load the cron script"
    );
    assert!(
        body.contains("Every 5 minutes"),
        "must render the reference table"
    );
    assert!(
        body.contains("WebApplication") && body.contains("isAccessibleForFree"),
        "must carry the free WebApplication schema"
    );
    assert!(
        headers.contains_key(header::ETAG),
        "tool page must set a strong ETag"
    );
}

#[tokio::test]
async fn incident_update_tool_renders_without_db() {
    let (status, body, headers) = get("/tools/incident-update-generator").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Incident update message generator"));
    assert!(body.contains("Investigating API issues"));
    assert!(body.contains("js/marketing/incident_update"));
    assert!(body.contains("WebApplication") && body.contains("isAccessibleForFree"));
    assert!(body.contains("Incident communication FAQ"));
    assert!(body.contains("Start with a common incident"));
    assert!(body.contains("Message quality"));
    assert!(body.contains("Email / support"));
    assert!(body.contains("What should customers do?"));
    assert!(body.contains("https://app.uptimepage.dev/login"));
    assert!(headers.contains_key(header::ETAG));
}

#[tokio::test]
async fn og_image_is_rooted_at_origin_on_subpages() {
    for path in [
        "/pricing",
        "/tools/cron-expression-generator",
        "/vs/uptimerobot",
    ] {
        let (status, body, _) = get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            body.contains(
                r#"property="og:image" content="https://uptimepage.dev/static/marketing/og.png""#
            ),
            "{path} must root og:image at the origin, not the page URL"
        );
    }
}

#[tokio::test]
async fn blog_meta_title_only_changes_the_document_title() {
    let (status, body, _) = get("/blog/is-98-uptime-good").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<title>Is 98% Uptime Good? When It Works and When It Fails</title>"),
        "meta_title must control the document title"
    );
    assert!(
        body.contains(
            "<h1 class=\"mk-display mk-blog-title\">Is 98% uptime good? It allows 7.3 days of downtime a year</h1>"
        ),
        "the visible H1 must keep the post title"
    );
    assert!(
        body.contains(
            r#"property="og:title" content="Is 98% uptime good? It allows 7.3 days of downtime a year""#
        ),
        "OpenGraph must keep the post title"
    );
    assert!(
        body.contains(r#""headline":"Is 98% uptime good? It allows 7.3 days of downtime a year""#),
        "BlogPosting schema must keep the post title"
    );

    let (status, body, _) = get("/blog/error-budgets-explained").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(
            "<title>Error budgets, explained: SLOs, burn rate, when to stop shipping</title>"
        ),
        "posts without meta_title must use the post title"
    );
}

#[tokio::test]
async fn blog_post_og_image_overrides_the_shared_card() {
    let (status, body, _) = get("/blog/is-98-uptime-good").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(
            r#"property="og:image" content="https://uptimepage.dev/static/marketing/og-98-uptime.png""#
        ),
        "og_image front-matter must replace the shared card"
    );
    assert!(
        body.contains(
            r#"name="twitter:image" content="https://uptimepage.dev/static/marketing/og-98-uptime.png""#
        ),
        "twitter:image must follow the override"
    );

    let (status, body, _) = get("/blog/error-budgets-explained").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(
            r#"property="og:image" content="https://uptimepage.dev/static/marketing/og.png""#
        ),
        "posts without og_image must keep the shared card"
    );
}

/// Hand-typed paths let a tool ship without ever reaching the sitemap. Drive
/// the assertion off the registry that mounts them.
#[tokio::test]
async fn sitemap_lists_the_tools() {
    let (status, body, _) = get("/sitemap.xml").await;
    assert_eq!(status, StatusCode::OK);
    for tool in tools::TOOLS {
        assert!(
            body.contains(&format!("https://uptimepage.dev{}", tool.path)),
            "sitemap must list {}",
            tool.path
        );
    }
}

/// Landing resources and doc bodies both have a link guard; blog prose did
/// not, so a renamed path rotted into a 404 while the suite stayed green.
#[tokio::test]
async fn blog_prose_links_resolve() {
    let mut checked = 0;
    for post in blog::list_published() {
        for href in internal_hrefs(&post.body_html) {
            let (status, _, _) = get(&href).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "/blog/{} links to {href}, which does not resolve",
                post.slug
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "extracted no links, so the guard proved nothing"
    );
}

#[tokio::test]
async fn changelog_index_lists_every_entry_in_full() {
    let (status, body, _) = get("/changelog").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("rel=\"alternate\" type=\"application/atom+xml\""));
    assert!(body.contains("<a href=\"/changelog\" class=\"mk-footer-link\">changelog</a>"));
    let entries = changelog::entries();
    assert!(!entries.is_empty());
    for e in entries {
        assert!(
            body.contains(&format!("href=\"{}\"", e.path())),
            "{} unlinked",
            e.slug
        );
        assert!(
            body.contains(&e.body_html),
            "{} body missing from index",
            e.slug
        );
    }
}

#[tokio::test]
async fn changelog_entries_serve_html_and_markdown_and_404_otherwise() {
    for e in changelog::entries() {
        let (status, body, _) = get(&e.path()).await;
        assert_eq!(status, StatusCode::OK, "{}", e.slug);
        assert!(body.contains(&format!(
            "<link rel=\"canonical\" href=\"https://uptimepage.dev{}\">",
            e.path()
        )));
        assert!(
            body.contains("\"@type\":\"Article\""),
            "{}: no Article JSON-LD",
            e.slug
        );
        let (_, md, _) = get_as_markdown(&e.path()).await;
        assert!(
            md.starts_with(&format!("# {}\n", e.title)),
            "{}: {md:.60}",
            e.slug
        );
    }
    let (status, _, _) = get("/changelog/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn changelog_feed_is_atom() {
    let (status, body, headers) = get("/changelog.xml").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/atom+xml; charset=utf-8"
    );
    assert!(body.contains(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:base=\"https://uptimepage.dev/\">"
    ));
    assert_eq!(body.matches("<entry>").count(), changelog::entries().len());
}

#[tokio::test]
async fn changelog_is_in_the_sitemap_and_llms_index() {
    let (_, sitemap, _) = get("/sitemap.xml").await;
    assert!(sitemap.contains("<loc>https://uptimepage.dev/changelog</loc>"));
    let (_, llms, _) = get("/llms.txt").await;
    assert!(llms.contains("## Changelog\n"));
    for e in changelog::entries() {
        assert!(
            sitemap.contains(&format!("<loc>https://uptimepage.dev{}</loc>", e.path())),
            "{}",
            e.slug
        );
        assert!(
            llms.contains(&format!("](https://uptimepage.dev{}): ", e.path())),
            "{}",
            e.slug
        );
    }
}

/// An entry names the docs, tools and setup pages it ships with; a renamed
/// path would rot the whole point of a changelog on the site.
#[tokio::test]
async fn changelog_prose_links_resolve() {
    let mut checked = 0;
    for e in changelog::entries() {
        for href in internal_hrefs(&e.body_html) {
            let (status, _, _) = get(&href).await;
            assert_eq!(status, StatusCode::OK, "{} links to {href}", e.slug);
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "extracted no links, so the guard proved nothing"
    );
}

#[tokio::test]
async fn diagnostic_guides_are_publishable_and_linked_from_related_pages() {
    let (status, sitemap, _) = get("/sitemap.xml").await;
    assert_eq!(status, StatusCode::OK);
    for (slug, sources) in [
        (
            "how-to-monitor-ssl-certificate-expiry",
            [
                "/tools/ssl-certificate-checker",
                "/blog/do-i-need-an-uptime-monitor",
                "/blog/domain-expired-but-site-still-up",
            ],
        ),
        (
            "how-to-debug-redirect-loops",
            [
                "/tools/http-header-checker",
                "/blog/do-i-need-an-uptime-monitor",
                "/blog/monitor-the-login-not-the-login-page",
            ],
        ),
        (
            "why-dns-returns-different-ip-addresses",
            [
                "/tools/dns-lookup",
                "/blog/do-i-need-an-uptime-monitor",
                "/blog/domain-expired-but-site-still-up",
            ],
        ),
    ] {
        let post = blog::all().iter().find(|post| post.slug == slug).unwrap();
        // Debug builds serve drafts too, so a successful GET alone would not
        // catch a link whose destination disappears in production.
        assert!(!post.draft, "{slug} must be visible in production builds");
        let path = format!("/blog/{slug}");
        assert_eq!(get(&path).await.0, StatusCode::OK);
        assert!(sitemap.contains(&format!("<loc>https://uptimepage.dev{path}</loc>")));
        for source in sources {
            let (status, body, _) = get(source).await;
            assert_eq!(status, StatusCode::OK);
            assert!(
                body.contains(&format!("href=\"{path}\"")),
                "{source} must link to {path}"
            );
        }
    }
}

/// Site-relative page links only: assets are served by the static layer this
/// router does not mount, and anchors resolve against the page itself.
fn internal_hrefs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tail in html.split("href=\"").skip(1) {
        let Some(href) = tail.split('"').next() else {
            continue;
        };
        if !href.starts_with('/') || href.starts_with("/static/") {
            continue;
        }
        let path = href.split('#').next().unwrap_or(href);
        if !path.is_empty() && !out.contains(&path.to_string()) {
            out.push(path.to_string());
        }
    }
    out
}

#[tokio::test]
async fn ssl_checker_renders_without_db() {
    let (status, body, _) = get("/tools/ssl-certificate-checker").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("ssl_checker"),
        "page must load its own script"
    );
    assert!(
        body.contains(r#"data-probe="/tools/ssl-certificate-checker/probe""#),
        "the form must carry the probe endpoint it posts to"
    );
}

#[tokio::test]
async fn website_security_checker_renders_and_is_discoverable() {
    let path = "/tools/website-security-checker";
    let (status, body, headers) = get(path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h1 class=\"mk-display\">Website security checker</h1>"));
    assert!(body.contains("security_checker"));
    assert!(body.contains("data-ssl-probe=\"/tools/ssl-certificate-checker/probe\""));
    assert!(body.contains("data-header-probe=\"/tools/http-header-checker/probe\""));
    assert!(body.contains("https://uptimepage.dev/tools/website-security-checker"));
    for schema in ["WebApplication", "FAQPage", "BreadcrumbList", "WebPage"] {
        assert!(body.contains(schema), "missing {schema}");
    }
    assert!(!body.contains("render failed"));
    assert!(!body.contains("<script>"));
    assert!(!body.contains("style="));
    assert!(headers.contains_key(header::ETAG));
    for source in [
        "/tools",
        "/tools/ssl-certificate-checker",
        "/tools/http-header-checker",
        "/sitemap.xml",
        "/llms.txt",
    ] {
        assert!(
            get(source).await.1.contains(path),
            "{source} must link the checker"
        );
    }
}

#[tokio::test]
async fn domain_expiry_checker_renders_and_is_discoverable() {
    let path = "/tools/domain-expiry-checker";
    let (status, body, _) = get(path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("domain_expiry.js"));
    assert!(body.contains(r#"data-probe="/tools/domain-expiry-checker/probe""#));
    assert!(body.contains("application/ld+json"));
    assert!(body.contains("WebApplication"));
    assert!(body.contains("FAQPage"));
    assert!(body.contains(r#"role="status""#));
    assert!(body.contains("does not fall back to WHOIS"));
    for href in internal_hrefs(&body) {
        assert_eq!(get(&href).await.0, StatusCode::OK, "{href}");
    }
    for source in [
        "/tools",
        "/blog/domain-expired-but-site-still-up",
        "/blog/do-i-need-an-uptime-monitor",
    ] {
        assert!(
            get(source).await.1.contains(&format!("href=\"{path}\"")),
            "{source}"
        );
    }
    assert!(
        get("/sitemap.xml")
            .await
            .1
            .contains(&format!("<loc>https://uptimepage.dev{path}</loc>"))
    );
    assert!(get("/llms.txt").await.1.contains(path));
    assert!(
        get("/robots.txt")
            .await
            .1
            .contains("Disallow: /tools/domain-expiry-checker/probe")
    );
    let (_, _, headers) = get("/start?kind=domain_expiry&url=example.com").await;
    assert!(
        headers["location"]
            .to_str()
            .unwrap()
            .contains("domain_expiry")
    );
}

#[tokio::test]
async fn domain_expiry_probe_rejects_bad_inputs_without_outbound_requests() {
    for query in [
        "",
        "domain=",
        "domain=localhost",
        "domain=127.0.0.1",
        "domain=169.254.169.254",
        "domain=%5B%3A%3A1%5D",
        "domain=co.uk",
        "domain=example.internal",
        "domain=example.com%3A43",
        "domain=a%0D%0A.com",
        "domain=example..com",
        "domain=one.com&domain=two.com",
    ] {
        let (status, body, headers) =
            get(&format!("/tools/domain-expiry-checker/probe?{query}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert!(body.contains(r#""ok":false"#));
        assert_eq!(headers["x-robots-tag"], "noindex");
        assert_eq!(headers["cache-control"], "no-store, private");
    }
}

/// The one marketing route that opens a socket. Each of these must be refused
/// before any connection is attempted, so the test needs no network.
#[tokio::test]
async fn ssl_probe_refuses_anything_but_a_public_hostname() {
    for query in [
        "host=127.0.0.1",
        "host=localhost",
        "host=acme.com&port=22",
        "host=",
    ] {
        let (status, body, _) = get(&format!("/tools/ssl-certificate-checker/probe?{query}")).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{query} must be refused, got {body}"
        );
        assert!(
            body.contains(r#""ok":false"#),
            "the page branches on ok, not on the status line: {body}"
        );
    }
}

#[tokio::test]
async fn header_checker_renders_without_db() {
    let (status, body, _) = get("/tools/http-header-checker").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("header_checker"),
        "page must load its own script"
    );
    assert!(
        body.contains(r#"data-probe="/tools/http-header-checker/probe""#),
        "the form must carry the probe endpoint it posts to"
    );
}

/// This probe follows redirects, so it can be pointed at more hosts than any
/// other marketing route. Each of these must be refused before a connection is
/// attempted, so the test needs no network.
#[tokio::test]
async fn header_probe_refuses_anything_but_a_public_web_url() {
    for query in [
        "url=http://127.0.0.1/",
        "url=http://localhost/",
        "url=http://169.254.169.254/latest/meta-data/",
        "url=http://[::1]/",
        "url=file:///etc/passwd",
        "url=gopher://acme.com",
        "url=https://acme.com:22/",
        "url=https://acme.com:5432/",
        "url=",
    ] {
        let (status, body, _) = get(&format!("/tools/http-header-checker/probe?{query}")).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{query} must be refused, got {body}"
        );
        assert!(
            body.contains(r#""ok":false"#),
            "the page branches on ok, not on the status line: {body}"
        );
    }
}

#[tokio::test]
async fn in_market_posts_offer_a_tracked_signup_cta() {
    let (status, body, _) = get("/blog/how-much-downtime-is-99-9-uptime").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Measure your own 43 minutes"),
        "the post's own CTA copy must render, not a generic label"
    );
    assert!(
        body.contains(r#"data-umami-event="signup-start""#)
            && body.contains(r#"data-umami-event-position="blog-closing""#),
        "the CTA must carry the tracking attributes or the conversion stays unmeasurable"
    );
    assert!(
        body.contains("https://app.uptimepage.dev/login"),
        "the CTA must point at the app, not another marketing page"
    );

    let (status, engineering, _) = get("/blog/clickhouse-system-tables-filled-disk").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !engineering.contains(r#"data-umami-event-position="blog-closing""#),
        "an engineering write-up must not end in a sales box"
    );
}

/// Broken front matter skips a post silently, leaving every other test green.
#[tokio::test]
async fn sla_guides_render_and_the_calculator_links_to_them() {
    let guides = [
        (
            "/blog/how-much-downtime-is-99-9-uptime",
            "43 minutes 12 seconds",
        ),
        (
            "/blog/how-much-downtime-is-99-95-uptime",
            "21 minutes 36 seconds",
        ),
        (
            "/blog/how-much-downtime-is-99-99-uptime",
            "4 minutes 19 seconds",
        ),
    ];
    let (calc_status, calculator, _) = get("/tools/uptime-sla-calculator").await;
    assert_eq!(calc_status, StatusCode::OK);
    let (sitemap_status, sitemap, _) = get("/sitemap.xml").await;
    assert_eq!(sitemap_status, StatusCode::OK);
    for (path, budget) in guides {
        let (status, body, _) = get(path).await;
        assert_eq!(status, StatusCode::OK, "{path} must render");
        assert!(
            body.contains(budget),
            "{path} must state its monthly budget of {budget}"
        );
        assert!(
            calculator.contains(&format!("href=\"{path}\"")),
            "the calculator must hand {path} the query it cannot rank for itself"
        );
        assert!(
            sitemap.contains(&format!("https://uptimepage.dev{path}")),
            "sitemap must list {path}"
        );
    }
}

/// Spot-checking landings by hand-typed path lets a retired page rot a test
/// instead of failing it. Drive the render off the table that mounts them.
#[tokio::test]
async fn every_landing_renders_and_its_links_resolve() {
    for l in landings::LANDINGS {
        let (status, _, _) = get(l.path).await;
        assert_eq!(status, StatusCode::OK, "{}", l.path);

        for r in l.resources {
            if !r.href.starts_with('/') {
                continue;
            }
            let (status, _, _) = get(r.href).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{} links to {}, which does not resolve",
                l.path,
                r.href
            );
        }
    }
}

#[tokio::test]
async fn retired_automation_path_redirects_to_terraform() {
    let (status, _, headers) = get("/automation").await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        headers.get(header::LOCATION).and_then(|h| h.to_str().ok()),
        Some("/terraform-uptime-monitoring"),
        "the retired path must keep its equity on the page that absorbed it"
    );
}

#[tokio::test]
async fn terraform_landing_renders() {
    let (status, body, _) = get("/terraform-uptime-monitoring").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Uptime monitoring you declare in Terraform"));
    assert!(
        body.contains("uptimepage/uptimepage"),
        "must name the provider"
    );
    assert!(
        body.contains("uptimepage_target") && body.contains("expected_status"),
        "must show the HCL snippet"
    );
    assert!(
        body.contains("href=\"https://registry.terraform.io/providers/uptimepage/uptimepage\""),
        "must link the Terraform Registry"
    );
}

#[tokio::test]
async fn llms_full_txt_renders() {
    let (status, body, _) = get("/llms-full.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("# Uptimepage"));
    assert!(body.contains("## Facts"), "must include the facts table");
    assert!(
        body.contains("## Blog: "),
        "must inline published blog posts"
    );
    assert!(
        body.contains("A status page your SaaS customers actually trust"),
        "must inline landing-page copy"
    );
}

#[tokio::test]
async fn etag_is_stable_and_returns_304() {
    let (_, _, headers) = get("/").await;
    let etag = headers
        .get(header::ETAG)
        .expect("ETag present")
        .to_str()
        .expect("ETag ascii")
        .to_string();
    let resp = router()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "uptimepage.dev")
                .header(header::IF_NONE_MATCH, etag.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router call");
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn marketing_serves_fingerprinted_assets() {
    // The dispatcher routes the whole apex/www host to the marketing
    // router, so marketing must own its own /static/{*path} route.
    // Without it, every <link href="/static/css/marketing.css?v=...">
    // emitted by a marketing template falls through to the marketing
    // 404 — page renders unstyled.
    let (status, _, headers) = get("/static/css/marketing.css").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "static assets must be served on the marketing host"
    );
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    assert!(ct.starts_with("text/css"), "got content-type {ct:?}");
}

#[tokio::test]
async fn legal_pages_render_without_db() {
    for route in uptimepage::marketing::legal::ROUTES {
        let (status, body, headers) = get(route.path).await;
        assert_eq!(status, StatusCode::OK, "{}", route.path);
        let heading = format!("<h1>{}</h1>", route.title);
        assert!(
            body.contains(&heading),
            "{} body missing rendered heading {heading:?}",
            route.path
        );
        assert!(
            headers.contains_key(header::ETAG),
            "{} must set a strong ETag",
            route.path
        );
    }
}

#[tokio::test]
async fn sitemap_lists_legal_routes() {
    let (status, body, _) = get("/sitemap.xml").await;
    assert_eq!(status, StatusCode::OK);
    for route in uptimepage::marketing::legal::ROUTES {
        let loc = format!("<loc>https://uptimepage.dev{}</loc>", route.path);
        assert!(body.contains(&loc), "sitemap missing {loc}");
    }
}

#[tokio::test]
async fn every_doc_page_renders_without_db() {
    for doc in uptimepage::marketing::docs::DOCS {
        let path = doc.path();
        let (status, body, _) = get(&path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.contains(doc.title), "{path} missing its title");
        assert!(
            body.contains("mk-doc-nav__link"),
            "{path} rendered without the docs sidebar"
        );
    }
}

#[tokio::test]
async fn docs_index_renders_without_db() {
    let (status, body, _) = get("/docs").await;
    assert_eq!(status, StatusCode::OK);
    for doc in uptimepage::marketing::docs::DOCS {
        assert!(
            body.contains(&format!("href=\"{}\"", doc.path())),
            "docs index missing {}",
            doc.slug
        );
    }
}

#[tokio::test]
async fn unknown_doc_page_returns_branded_404() {
    let (status, body, _) = get("/docs/not-a-page").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Uptimepage"));
}

#[tokio::test]
async fn sitemap_and_llms_list_every_doc() {
    let (_, sitemap, _) = get("/sitemap.xml").await;
    let (_, llms, _) = get("/llms.txt").await;
    for doc in uptimepage::marketing::docs::DOCS {
        let loc = format!("<loc>https://uptimepage.dev{}</loc>", doc.path());
        assert!(sitemap.contains(&loc), "sitemap missing {loc}");
        assert!(
            llms.contains(&format!("https://uptimepage.dev{}", doc.path())),
            "llms.txt missing {}",
            doc.slug
        );
    }
}

#[tokio::test]
async fn llms_full_inlines_product_docs_but_not_self_hosting() {
    let (status, body, _) = get("/llms-full.txt").await;
    assert_eq!(status, StatusCode::OK);
    for doc in uptimepage::marketing::docs::DOCS {
        let heading = format!("## Docs: {}", doc.title);
        let self_hosting = doc.section == uptimepage::marketing::docs::Section::SelfHosting;
        assert_eq!(
            body.contains(&heading),
            !self_hosting,
            "{}: unexpected llms-full inlining",
            doc.slug
        );
    }
    // A distinctive line from a guide body, proving the source is inlined
    // rather than just the heading.
    assert!(
        body.contains("A monitor going down is only useful if it reaches someone"),
        "llms-full must carry doc bodies"
    );
}

#[tokio::test]
async fn cookie_does_not_change_response_body() {
    // Cookie isolation: marketing serves identical bytes whether or not
    // a `_sm_session` cookie tags along. No Vary: Cookie, no
    // Set-Cookie. Without this the apex CDN cache would be fractured by
    // session ID — a privacy + cacheability failure mode.
    let plain = router()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "uptimepage.dev")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let with_cookie = router()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "uptimepage.dev")
                .header(header::COOKIE, "_sm_session=fake-session-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(plain.status(), with_cookie.status());
    assert!(
        with_cookie.headers().get(header::SET_COOKIE).is_none(),
        "marketing must not Set-Cookie"
    );
    let vary = with_cookie
        .headers()
        .get(header::VARY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        !vary.to_ascii_lowercase().contains("cookie"),
        "marketing must not Vary: Cookie, got {vary:?}"
    );
    let plain_bytes = axum::body::to_bytes(plain.into_body(), 1 << 20)
        .await
        .unwrap();
    let with_bytes = axum::body::to_bytes(with_cookie.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(plain_bytes, with_bytes);
}

/// The sidebar is baked into every page's cached render, so a nav that
/// differs between pages means a stale cache somewhere rather than a
/// content change. Also pins exactly one entry marked current.
#[tokio::test]
async fn docs_nav_is_identical_across_pages() {
    let mut seen: Option<(String, String)> = None;
    for doc in uptimepage::marketing::docs::DOCS {
        let (_, body, _) = get(&doc.path()).await;
        let start = body.find("mk-doc-nav__body").expect("sidebar");
        let end = body[start..].find("</nav>").expect("sidebar end") + start;
        let sidebar = &body[start..end];
        let active = sidebar.matches("is-active").count();
        assert_eq!(active, 1, "{}: {active} active nav entries", doc.slug);
        // Exactly-one-active would still hold if every page marked the same
        // wrong entry, so pin which one.
        let active_href = sidebar
            .split("<a ")
            .find(|tag| tag.contains("is-active"))
            .and_then(|tag| tag.split("href=\"").nth(1))
            .and_then(|rest| rest.split('"').next())
            .expect("active nav entry carries an href");
        assert_eq!(
            active_href,
            doc.path(),
            "{}: sidebar marks the wrong entry current",
            doc.slug
        );
        let neutral = sidebar
            .replace(" is-active", "")
            .replace(" aria-current=\"page\"", "");
        let count = sidebar.matches("mk-doc-nav__link").count();
        match &seen {
            None => {
                assert_eq!(
                    count,
                    uptimepage::marketing::docs::DOCS.len(),
                    "sidebar must link every published page"
                );
                seen = Some((neutral, doc.slug.to_string()));
            }
            Some((first, first_slug)) => assert_eq!(
                &neutral, first,
                "{} nav differs from {first_slug}",
                doc.slug
            ),
        }
    }
}

#[tokio::test]
async fn architecture_carries_structured_data() {
    let (status, body, _) = get("/architecture").await;
    assert_eq!(status, StatusCode::OK);
    let blocks: Vec<&str> = body
        .split(r#"<script type="application/ld+json">"#)
        .skip(1)
        .filter_map(|s| s.split("</script>").next())
        .collect();
    assert_eq!(
        blocks.len(),
        2,
        "expected a breadcrumb and an article block"
    );
    let types: Vec<String> = blocks
        .iter()
        .map(|b| {
            let v: serde_json::Value = serde_json::from_str(b).expect("JSON-LD must parse");
            v["@type"].as_str().unwrap_or_default().to_string()
        })
        .collect();
    assert!(
        types.contains(&"BreadcrumbList".to_string()),
        "got {types:?}"
    );
    assert!(types.contains(&"TechArticle".to_string()), "got {types:?}");
    let article: serde_json::Value = serde_json::from_str(
        blocks
            .iter()
            .find(|b| b.contains("TechArticle"))
            .expect("article block"),
    )
    .unwrap();
    // The citation-relevant edges: who wrote it and what it is about.
    assert!(article["author"].is_object(), "TechArticle needs an author");
    assert_eq!(
        article["about"]["@id"], "https://uptimepage.dev/#software",
        "the article must point at the product node"
    );
}

#[tokio::test]
async fn architecture_serves_the_map_content_as_html() {
    let (status, body, _) = get("/architecture").await;
    assert_eq!(status, StatusCode::OK);
    for node in ["Operator browser", "Region agent process"] {
        assert!(body.contains(node), "node {node:?} must be server-rendered");
    }
    for flow in [
        "Scheduled HTTP check",
        "Control plane probes a monitor in its own region",
    ] {
        assert!(body.contains(flow), "flow {flow:?} must be server-rendered");
    }
    assert!(
        body.contains("Full re-list of enabled targets for this region"),
        "step prose must be server-rendered, not left for flows.js"
    );
    assert!(
        body.matches(r#"class="node""#).count() >= 50,
        "every node must reach the HTML, got {}",
        body.matches(r#"class="node""#).count()
    );

    // The map carries all 17 flows; the written reference is curated, or the
    // page turns back into a wall of generated text nobody reads.
    let written = body.matches(r#"class="mk-faq arch-ref__flow""#).count();
    assert_eq!(written, 5, "the reference must stay curated, got {written}");
    assert!(
        body.contains("Domain expiry with sticky last-good"),
        "an unwritten flow must still be selectable on the map"
    );
    assert!(
        !body.contains("Hard floor is twelve hours"),
        "an unwritten flow must not have its steps in the reference"
    );
}

/// A count rather than a list: all eight kinds do not fit the length a search
/// snippet survives. Lives here because `src/marketing/` may not reach
/// `crate::domain`.
#[test]
fn the_site_description_counts_every_check_kind() {
    const WORDS: [&str; 9] = [
        "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    let real = CheckSpec::ALL_KINDS.len();
    let text = META_DESCRIPTION.to_lowercase();
    let (claimed, word) = WORDS
        .iter()
        .enumerate()
        .find(|(_, w)| text.contains(&format!("{w} check")))
        .map(|(i, w)| (i + 2, *w))
        .expect("META_DESCRIPTION should say how many check kinds there are");
    assert_eq!(
        claimed, real,
        "META_DESCRIPTION says {word} check types; CheckSpec has {real}"
    );
}
