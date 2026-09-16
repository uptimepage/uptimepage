//! Live-PG end-to-end coverage for the per-org public status surface served
//! at `{slug}.{base_domain}`. Skipped by default; runs under
//! `--include-ignored` once `DATABASE_URL` is set.
//!
//! Two layers of assertion:
//!
//!  * Storage gate: the public-path resolver `find_public_status_page_by_slug`
//!    resolves only enabled pages, whereas the authenticated `find_id_by_slug`
//!    resolves the org regardless. Swapping them would publish every page
//!    regardless of its published flag.
//!  * HTTP gate: the host-aware `ResolvedStatusPage` extractor admits a request
//!    only for an enabled page slug. A blocked request is a 404 (extractor
//!    rejection); an admitted one renders the page (200). The 200-vs-404
//!    split proves the extractor did the gating. Disabling the page via the
//!    operator storage path flips a previously-served slug back to 404.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{make_user, unique_slug};
use tower::ServiceExt;
use uptimepage::domain::{NewStatusPage, OrgId, StatusPageUpdate, UserId, WriteSource};
use uptimepage::storage::orgs::{find_id_by_slug, find_public_status_page_by_slug};
use uptimepage::storage::{PgStatusPageStore, StatusPageStore, create_org_with_owner};

/// The `public_status.base_domain` config value. Status pages live at
/// `{slug}.{BASE_DOMAIN}` (apex-wildcard shape); see [`status_host`], the
/// only place that contract is spelled so a host can never silently drift
/// from what the parser expects.
const BASE_DOMAIN: &str = "test.local";

/// Build the public-status `Host` for a slug, matching the production
/// `{slug}.{base_domain}` contract the host parser enforces.
fn status_host(slug: &str) -> String {
    format!("{slug}.{BASE_DOMAIN}")
}

/// Create an org owned by a fresh user, plus its public page at slug = org slug
/// (orgs are no longer auto-seeded a page — the owner creates one). Returns the
/// org plus its owner.
async fn seed_org(pool: &sqlx::PgPool, slug: &str, enabled: bool) -> (OrgId, UserId) {
    let user = make_user(pool, "subdomain").await;
    let org = create_org_with_owner(pool, user, slug, slug)
        .await
        .expect("create org")
        .expect("slug not taken");
    PgStatusPageStore::new(pool.clone())
        .create(
            org.id,
            NewStatusPage {
                slug: slug.into(),
                name: slug.into(),
                enabled,
            },
            WriteSource::Ui,
            i64::MAX,
            None,
        )
        .await
        .expect("create page")
        .expect("slug free");
    (org.id, user)
}

/// Flip the org's page published flag through the same store the operator
/// endpoint uses.
async fn set_enabled(pool: &sqlx::PgPool, org: OrgId, enabled: bool) {
    let store = PgStatusPageStore::new(pool.clone());
    let pages = store.list(org).await.expect("list pages");
    let page = pages.first().expect("org has a page");
    let updated = store
        .update(
            org,
            page.id,
            StatusPageUpdate {
                enabled: Some(enabled),
                ..Default::default()
            },
            WriteSource::Ui,
        )
        .await
        .expect("update page");
    assert!(updated.is_some(), "page must exist for update");
}

/// `GET /status` against the SaaS-subdomain app with an explicit `Host`.
async fn get_status(app: &axum::Router, host: Option<&str>) -> StatusCode {
    get_path(app, "/status", host).await
}

/// `GET <path>` against the SaaS-subdomain app with an explicit `Host`.
async fn get_path(app: &axum::Router, path: &str, host: Option<&str>) -> StatusCode {
    get_response(app, path, host).await.status()
}

async fn get_response(
    app: &axum::Router,
    path: &str,
    host: Option<&str>,
) -> axum::response::Response {
    let mut req = Request::builder().uri(path);
    if let Some(h) = host {
        req = req.header("host", h);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("oneshot")
}

fn saas_subdomain(cfg: &mut uptimepage::config::AppConfig) {
    cfg.tenancy.subdomain_public_routes = true;
    cfg.tenancy.path_based_public_routes = false;
    cfg.public_status.base_domain = BASE_DOMAIN.into();
    cfg.auth.session.cookie_domain = String::new();
    // Configured provider so /auth/github/login would NOT 404 by itself —
    // the operator-surface leak assertions must measure the host gate only.
    cfg.auth.github.client_id = "test-client".into();
    cfg.auth.github.client_secret = String::from("test-secret").into();
    cfg.auth.github.redirect_url = format!("https://app.{BASE_DOMAIN}/auth/github/callback");
}

#[tokio::test]
#[ignore]
async fn public_lookup_filters_disabled_orgs_but_authed_lookup_does_not() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let on = unique_slug("on");
    let off = unique_slug("off");
    seed_org(&pool, &on, true).await;
    seed_org(&pool, &off, false).await;

    // The public path sees only the opted-in org.
    assert!(
        find_public_status_page_by_slug(&pool, &on)
            .await
            .unwrap()
            .is_some(),
        "enabled org visible to public lookup"
    );
    assert!(
        find_public_status_page_by_slug(&pool, &off)
            .await
            .unwrap()
            .is_none(),
        "disabled org hidden from public lookup"
    );
    assert!(
        find_public_status_page_by_slug(&pool, &unique_slug("ghost"))
            .await
            .unwrap()
            .is_none(),
        "nonexistent slug → None"
    );

    // The authenticated lookup ignores the opt-in flag — proves the gating
    // lives in the public helper, not in slug existence. Substituting it on
    // the public path would expose the disabled org.
    assert!(find_id_by_slug(&pool, &on).await.unwrap().is_some());
    assert!(
        find_id_by_slug(&pool, &off).await.unwrap().is_some(),
        "authed lookup still resolves the disabled org"
    );
}

#[tokio::test]
#[ignore]
async fn subdomain_status_page_gates_on_enabled_and_slug_shape() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let on = unique_slug("on");
    let off = unique_slug("off");
    seed_org(&pool, &on, true).await;
    seed_org(&pool, &off, false).await;

    let (app, _default) = common::build_test_app_with_pg(pool, saas_subdomain).await;

    // Enabled slug: the extractor admits the request, and `/status` hands the
    // page to `/`. The blocked shapes below all 404, so redirect-vs-404
    // isolates the gate.
    let admitted = get_status(&app, Some(&status_host(&on))).await;
    assert_eq!(
        admitted,
        StatusCode::PERMANENT_REDIRECT,
        "enabled slug must reach its page"
    );

    // Every blocked shape collapses to 404 at the extractor.
    for (host, why) in [
        (Some(status_host(&off)), "disabled org"),
        (Some(status_host(&unique_slug("ghost"))), "unknown slug"),
        (Some(format!("a.b.{BASE_DOMAIN}")), "deeper subdomain"),
        (Some(BASE_DOMAIN.to_string()), "bare base domain"),
    ] {
        assert_eq!(
            get_status(&app, host.as_deref()).await,
            StatusCode::NOT_FOUND,
            "{why} must 404"
        );
    }
    assert_eq!(
        get_status(&app, None).await,
        StatusCode::NOT_FOUND,
        "missing Host header must 404"
    );
}

#[tokio::test]
#[ignore]
async fn subdomain_root_serves_public_page() {
    // `/` on a `{slug}.{base_domain}` host renders the public page just like
    // `/status` does — competitor parity (Statuspage/BetterStack/Instatus
    // all serve at apex). The route stays at `/status`; the request URI is
    // rewritten before the router matches.
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let slug = unique_slug("root");
    seed_org(&pool, &slug, true).await;
    let (app, _default) = common::build_test_app_with_pg(pool, saas_subdomain).await;

    let host = status_host(&slug);
    assert_eq!(
        get_path(&app, "/", Some(&host)).await,
        StatusCode::OK,
        "subdomain `/` must serve the public page"
    );
    assert_eq!(
        get_path(&app, "/?utm_source=email", Some(&host)).await,
        StatusCode::OK,
        "query string must survive the rewrite"
    );
    // RFC: hostnames are case-insensitive. Browsers always lowercase
    // the authority, but a crafted client (curl `-H Host: …`,
    // misconfigured proxy) can send mixed case. Stored slugs are
    // always lowercase (`validate_slug`), so a case-sensitive resolver
    // would 404 a legitimate tenant on `SLUG.{base}`.
    let upper_host = host.to_ascii_uppercase();
    assert_eq!(
        get_path(&app, "/", Some(&upper_host)).await,
        StatusCode::OK,
        "uppercase Host `{upper_host}` must resolve the same tenant"
    );
    // Blocked host shapes don't trigger the rewrite — `/` falls through to
    // the dashboard route, which requires auth and never returns 200 here.
    assert_ne!(
        get_path(&app, "/", Some(&format!("a.b.{BASE_DOMAIN}"))).await,
        StatusCode::OK,
        "deeper subdomain must NOT be treated as a public surface"
    );
    assert_ne!(
        get_path(&app, "/", Some(BASE_DOMAIN)).await,
        StatusCode::OK,
        "bare base domain must NOT be treated as a public surface"
    );
}

#[tokio::test]
#[ignore]
async fn subdomain_status_path_redirects_to_root() {
    // One page reachable at two URLs splits its search ranking and gives
    // visitors two addresses to share, so `/status` hands off to `/`.
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let slug = unique_slug("alias");
    seed_org(&pool, &slug, true).await;
    let (app, _default) = common::build_test_app_with_pg(pool, saas_subdomain).await;
    let host = status_host(&slug);

    let res = get_response(&app, "/status", Some(&host)).await;
    assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(res.headers().get("location").unwrap(), "/");

    // The auto-refresh poll of an already-open tab still asks here, and a
    // redirect that dropped the query would answer it with the whole page.
    let res = get_response(&app, "/status?fragment=1", Some(&host)).await;
    assert_eq!(res.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(res.headers().get("location").unwrap(), "/?fragment=1");
}

#[tokio::test]
#[ignore]
async fn rss_links_stay_on_the_tenant_host() {
    // A reader keeps these links and follows them days later, so they have to
    // name the page's own host, not whatever address the process bound.
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let slug = unique_slug("feed");
    seed_org(&pool, &slug, true).await;
    let (app, _default) = common::build_test_app_with_pg(pool, saas_subdomain).await;
    let host = status_host(&slug);

    let res = get_response(&app, "/api/public/v1/incidents.rss", Some(&host)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20)
        .await
        .unwrap();
    let xml = String::from_utf8(bytes.to_vec()).expect("utf-8");
    assert!(
        xml.contains(&format!("<link>https://{host}</link>")),
        "channel link must be the tenant page:\n{xml}"
    );
    assert!(
        !xml.contains("127.0.0.1") && !xml.contains("0.0.0.0"),
        "feed must not publish the listen address:\n{xml}"
    );
}

#[tokio::test]
#[ignore]
async fn tenant_host_isolates_operator_surface() {
    // Default-deny: a tenant subdomain (`{slug}.{base}`) must only
    // serve the public-status allow-list. Operator UI (`/login`),
    // auth flows
    // (`/auth/github/*`), settings, targets, and the private API must
    // 404 — not 302 to /login, not 401, not render the operator form
    // (a phishing-friendly clone). The session cookie is host-scoped
    // so this isn't a credential-hijack vector; it's a surface gate
    // that keeps the operator UI single-host.
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let slug = unique_slug("isolate");
    seed_org(&pool, &slug, true).await;
    let (app, _default) = common::build_test_app_with_pg(pool, saas_subdomain).await;
    let tenant_host = status_host(&slug);
    let operator_host = format!("app.{BASE_DOMAIN}");

    // Operator surface remains reachable on `app.{base}` — these
    // assertions guard against the middleware over-firing.
    assert_eq!(
        get_path(&app, "/login", Some(&operator_host)).await,
        StatusCode::OK,
        "/login must still render on the operator host"
    );

    // All of these expose operator surface and MUST 404 on tenant hosts.
    for path in [
        "/login",
        "/auth/github/login",
        "/invitations/accept?token=x",
        "/settings/team",
        "/web/partials/settings/team",
        "/web/partials/nav",
        "/targets",
        "/settings/account",
        "/settings/sessions",
        "/api/v1/targets",
    ] {
        assert_eq!(
            get_path(&app, path, Some(&tenant_host)).await,
            StatusCode::NOT_FOUND,
            "{path} must 404 on tenant host (operator-surface leak)"
        );
    }

    // Allow-list paths still resolve on tenant hosts. `/` rewrites to
    // the public-status page; `/status` redirects there.
    assert_eq!(
        get_path(&app, "/", Some(&tenant_host)).await,
        StatusCode::OK,
        "tenant host `/` must still serve the public page"
    );
    assert_eq!(
        get_path(&app, "/status", Some(&tenant_host)).await,
        StatusCode::PERMANENT_REDIRECT,
        "tenant host `/status` must still reach the public page"
    );

    // Health probes are exempt — Caddy active-health may carry an
    // opaque Host that classifies as tenant, and 404ing it would mark
    // the upstream down.
    assert_eq!(
        get_path(&app, "/healthz", Some(&tenant_host)).await,
        StatusCode::OK,
        "/healthz must bypass tenant isolation"
    );
}

#[tokio::test]
#[ignore]
async fn mcp_host_serves_only_the_connector_surface() {
    // `mcp.{base}` is an operator label, so without its own gate it would
    // carry the whole operator surface: a login clone on a second host,
    // and a second `Host` every per-IP edge limit on `app.{base}` could be
    // sidestepped through. Only the transport and discovery may answer.
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |cfg| {
        saas_subdomain(cfg);
        cfg.mcp.enabled = true;
        cfg.mcp.oauth_enabled = true;
        cfg.mcp.resource_uri = format!("https://mcp.{BASE_DOMAIN}/mcp");
        cfg.auth.public_base_url = format!("https://app.{BASE_DOMAIN}");
    })
    .await;
    let mcp_host = format!("mcp.{BASE_DOMAIN}");
    let operator_host = format!("app.{BASE_DOMAIN}");

    // Bearer auth answers, so the transport is reachable.
    assert_eq!(
        get_path(&app, "/mcp", Some(&mcp_host)).await,
        StatusCode::UNAUTHORIZED,
        "/mcp must reach the transport on the MCP host"
    );
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
        "/.well-known/oauth-authorization-server",
        "/.well-known/mcp/server-card.json",
    ] {
        assert_eq!(
            get_path(&app, path, Some(&mcp_host)).await,
            StatusCode::OK,
            "{path} must be served on the MCP host"
        );
    }

    for path in [
        "/",
        "/login",
        "/auth/github/login",
        "/oauth/authorize",
        "/invitations/accept?token=x",
        "/settings/account",
        "/api/v1/targets",
        "/m/token",
    ] {
        assert_eq!(
            get_path(&app, path, Some(&mcp_host)).await,
            StatusCode::NOT_FOUND,
            "{path} must 404 on the MCP host (operator-surface leak)"
        );
    }
    // The AS endpoints live on the issuer; a registration aimed at the MCP
    // host must not create a client row.
    let register = |host: &str| {
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/oauth/register")
                .header("host", host)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"redirect_uris":["https://client.example/cb"]}"#,
                ))
                .unwrap(),
        )
    };
    assert_eq!(
        register(&mcp_host).await.expect("oneshot").status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        register(&operator_host).await.expect("oneshot").status(),
        StatusCode::CREATED,
        "the same registration must still work on the issuer host"
    );

    assert_eq!(
        get_path(&app, "/healthz", Some(&mcp_host)).await,
        StatusCode::OK,
        "/healthz must bypass host isolation"
    );

    // The shipped self-host compose sets the base domain with the subdomain
    // surface off; the MCP gate must not hinge on the tenant one.
    let (self_host, _default) = common::build_test_app_with_pg(pool, |cfg| {
        saas_subdomain(cfg);
        cfg.tenancy.subdomain_public_routes = false;
        cfg.tenancy.path_based_public_routes = true;
        cfg.mcp.enabled = true;
        cfg.mcp.resource_uri = format!("https://mcp.{BASE_DOMAIN}/mcp");
    })
    .await;
    assert_eq!(
        get_path(&self_host, "/login", Some(&mcp_host)).await,
        StatusCode::NOT_FOUND,
        "/login must 404 on the MCP host with the subdomain surface off"
    );
    assert_eq!(
        get_path(&self_host, "/mcp", Some(&mcp_host)).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_path(&self_host, "/login", Some(&operator_host)).await,
        StatusCode::OK
    );
}

#[tokio::test]
#[ignore]
async fn root_routes_operator_host_to_dashboard_non_operator_slugs_to_404() {
    // The two security-relevant branches:
    //  * `app.{base_domain}` (operator label) → dashboard. No session cookie
    //    → 303 to /login, proving the dispatcher reached the dashboard path.
    //  * Slug-shaped non-operator labels (e.g. `www`, `hetzner`) → public
    //    dispatcher → 404 via `StatusPageOrg` (no org owns the slug). This
    //    is the load-bearing property: a phishing-style host that *looks*
    //    like a tenant page (real cert, real domain) must NOT expose the
    //    operator login surface.
    //
    // Malformed shapes (bare base, deeper subdomain, missing Host) all fail
    // `extract_status_slug` and fall through to the dashboard branch (303).
    // Acceptable: bare base isn't routable in prod (no apex A record), the
    // deeper-subdomain shape isn't a useful phishing surface (no realistic
    // single-label confusion target), and missing Host is an HTTP/1.0-style
    // anomaly. The asserted property is "slug-shaped attacker-controlled
    // labels don't get the operator surface."
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let (app, _default) = common::build_test_app_with_pg(pool, saas_subdomain).await;

    assert_eq!(
        get_path(&app, "/", Some(&format!("app.{BASE_DOMAIN}"))).await,
        StatusCode::SEE_OTHER,
        "operator host `app.{BASE_DOMAIN}` must reach the dashboard branch"
    );
    // Case-insensitive match — Host headers are case-insensitive per RFC,
    // and `is_operator_label` uses `eq_ignore_ascii_case`. A future
    // "simplification" to `==` would fail this and route `APP.{base}` to
    // the public dispatcher instead of the operator dashboard.
    assert_eq!(
        get_path(&app, "/", Some(&format!("APP.{BASE_DOMAIN}"))).await,
        StatusCode::SEE_OTHER,
        "uppercase `APP.{BASE_DOMAIN}` must match the operator label"
    );

    for (host, why) in [
        (
            format!("www.{BASE_DOMAIN}"),
            "reserved label that isn't an operator subdomain",
        ),
        (format!("hetzner.{BASE_DOMAIN}"), "broader reserved label"),
        (
            format!("admin.{BASE_DOMAIN}"),
            "phishing-bait label (admin)",
        ),
    ] {
        assert_eq!(
            get_path(&app, "/", Some(&host)).await,
            StatusCode::NOT_FOUND,
            "non-operator slug-shaped `/` ({why}) must 404 via public dispatcher, not leak operator surface"
        );
    }
}

#[tokio::test]
#[ignore]
async fn disabling_via_operator_path_takes_the_page_offline() {
    let Some(pool) = common::pg_pool_from_env().await else {
        return;
    };
    let slug = unique_slug("flip");
    let (org, _user) = seed_org(&pool, &slug, true).await;

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), saas_subdomain).await;
    let host = status_host(&slug);

    assert_eq!(
        get_path(&app, "/", Some(&host)).await,
        StatusCode::OK,
        "enabled org renders its page"
    );

    // Operator turns the page off through the real storage path.
    set_enabled(&pool, org, false).await;

    assert_eq!(
        get_path(&app, "/", Some(&host)).await,
        StatusCode::NOT_FOUND,
        "disabled org no longer resolves on its subdomain"
    );
}
