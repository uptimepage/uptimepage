//! A path-based deploy serves each of its one org's pages on the operator host
//! under its own `/status/{slug}`, with the page's whole surface below that
//! prefix: archive, incidents, subscribe form, sign-in and public API.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Each test owns a fresh database,
//! since the surface answers only while exactly one org is live.

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::config::AppConfig;
use uptimepage::domain::{
    AccessRule, AccessWrite, OrgId, PageAccess, PageAccessSettings, StatusPageId, UserId,
};
use uptimepage::email::{EmailTemplate, InMemoryEmailSender};
use uptimepage::storage::page_access::settings;
use uuid::Uuid;

use common::{
    SAAS_BASE_DOMAIN, build_saas_router_with_pg_tweaked, drop_test_db, fresh_test_db,
    open_test_pool, unique_slug,
};

const ORIGIN: &str = "https://ops.example.test";

struct Deploy {
    app: Router,
    pool: PgPool,
    db: String,
    mail: Arc<InMemoryEmailSender>,
    owner: UserId,
    org: OrgId,
    one: Page,
    two: Page,
}

struct Page {
    slug: String,
    id: Uuid,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn set_cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{name}=")))
    }
}

fn path_based(cfg: &mut AppConfig) {
    cfg.tenancy.path_based_public_routes = true;
    cfg.tenancy.subdomain_public_routes = false;
    cfg.auth.public_base_url = ORIGIN.into();
}

async fn deploy(mutate: impl FnOnce(&mut AppConfig)) -> Option<Deploy> {
    let (url, db) = fresh_test_db("path_mode").await?;
    let pool = open_test_pool(&url).await;
    let owner = common::make_user(&pool, "path-mode").await;
    let org =
        uptimepage::storage::create_org_with_owner(&pool, owner, &unique_slug("pm"), "Path Org")
            .await
            .unwrap()
            .unwrap();
    let one = seed_page(&pool, org.id, "Front Desk").await;
    let two = seed_page(&pool, org.id, "Back Office").await;
    let mail = Arc::new(InMemoryEmailSender::new());
    let sender = mail.clone();
    let app = build_saas_router_with_pg_tweaked(pool.clone(), mutate, move |mut state| {
        state.email_sender = sender;
        state
    })
    .await;
    Some(Deploy {
        app,
        pool,
        db,
        mail,
        owner,
        org: org.id,
        one,
        two,
    })
}

async fn seed_page(pool: &PgPool, org: OrgId, display: &str) -> Page {
    let slug = unique_slug("pg");
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled, public_display_name) \
         VALUES ($1, $2, $3, true, $3) RETURNING id",
    )
    .bind(org.0)
    .bind(&slug)
    .bind(display)
    .fetch_one(pool)
    .await
    .unwrap();
    Page { slug, id }
}

impl Deploy {
    async fn done(self) {
        self.pool.close().await;
        drop_test_db(&self.db).await;
    }

    async fn send(&self, req: axum::http::request::Builder, body: Body) -> Reply {
        let req = req
            .header(header::HOST, "ops.example.test")
            .body(body)
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
            .await
            .unwrap();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, path: &str) -> Reply {
        self.send(Request::get(path), Body::empty()).await
    }

    async fn get_with(&self, path: &str, cookie: &str) -> Reply {
        self.send(
            Request::get(path).header(header::COOKIE, cookie),
            Body::empty(),
        )
        .await
    }

    /// A form post from a browser that also holds an app session, so the CSRF
    /// guard is in play.
    async fn post_form(&self, path: &str, cookie: &str, pairs: &[(&str, &str)]) -> Reply {
        let app_session = format!(
            "{}=stale",
            common::test_config(|_| {}).auth.session.cookie_name
        );
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        self.send(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("{app_session}; {cookie}")),
            Body::from(body),
        )
        .await
    }

    async fn make_private(&self, page: &Page, entries: &[&str]) {
        let mut tx = self.pool.begin().await.unwrap();
        settings::replace_in_tx(
            &mut tx,
            self.org,
            StatusPageId(page.id),
            &AccessWrite {
                access: PageAccess {
                    settings: PageAccessSettings {
                        protected: true,
                        ..Default::default()
                    },
                    rules: entries
                        .iter()
                        .map(|e| AccessRule::parse(e).unwrap())
                        .collect(),
                    password_set_at: None,
                },
                password_hash: None,
                actor: Some(self.owner),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    async fn mailed_code(&self, email: &str) -> Option<(String, String)> {
        for _ in 0..40 {
            let found = self.mail.sent().into_iter().rev().find_map(|m| {
                match (m.to.address == email, m.template) {
                    (true, EmailTemplate::PageAccessCode { code, url, .. }) => Some((code, url)),
                    _ => None,
                }
            });
            if found.is_some() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    }
}

#[tokio::test]
#[ignore]
async fn each_page_answers_under_its_own_path_with_its_own_links() {
    let Some(d) = deploy(path_based).await else {
        return;
    };
    let (one, two) = (&d.one.slug, &d.two.slug);

    let page = d.get(&format!("/status/{one}")).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("Front Desk"));
    assert!(!page.body.contains("Back Office"));

    let page = d.get(&format!("/status/{two}")).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("Back Office"));
    assert!(!page.body.contains("Front Desk"));
    for link in [
        format!(r#"href="/status/{two}/incidents""#),
        format!(r#"action="/status/{two}/subscribe""#),
        format!(r#"href="/status/{two}/api/public/v1/incidents.rss""#),
    ] {
        assert!(page.body.contains(&link), "{link} missing");
    }

    let archive = d.get(&format!("/status/{two}/incidents")).await;
    assert_eq!(archive.status, StatusCode::OK);
    assert!(archive.body.contains("Back Office"));
    assert!(archive.body.contains(&format!(r#"href="/status/{two}""#)));

    // Routed, with the page beside the id: a missed incident, not a bad path.
    let missing = Uuid::now_v7();
    assert_eq!(
        d.get(&format!("/status/{two}/incidents/{missing}"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        d.get(&format!("/status/{two}/api/public/v1/incidents/{missing}"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let api = d.get(&format!("/status/{two}/api/public/v1/status")).await;
    assert_eq!(api.status, StatusCode::OK, "{}", api.body);
    let feed = d
        .get(&format!("/status/{two}/api/public/v1/incidents.rss"))
        .await;
    assert_eq!(feed.status, StatusCode::OK);
    assert!(
        feed.body
            .contains(&format!("<link>{ORIGIN}/status/{two}</link>")),
        "{}",
        feed.body
    );
    d.done().await;
}

#[tokio::test]
#[ignore]
async fn an_unknown_or_unpublished_page_is_not_found_under_any_of_its_paths() {
    let Some(d) = deploy(path_based).await else {
        return;
    };
    let two = &d.two.slug;
    assert_eq!(
        d.get(&format!("/status/{}", unique_slug("nobody")))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // Paths are case-sensitive; only the stored spelling answers.
    assert_eq!(
        d.get(&format!("/status/{}", two.to_ascii_uppercase()))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    sqlx::query("UPDATE status_pages SET enabled = false WHERE id = $1")
        .bind(d.two.id)
        .execute(&d.pool)
        .await
        .unwrap();
    for path in [
        format!("/status/{two}"),
        format!("/status/{two}/incidents"),
        format!("/status/{two}/api/public/v1/status"),
    ] {
        assert_eq!(d.get(&path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        d.get(&format!("/status/{}", d.one.slug)).await.status,
        StatusCode::OK
    );

    // Bytes no slug is made of name no page, rather than the primary one or a
    // lookup the database refuses.
    for path in [
        "/status/%FF/incidents",
        "/status/%FF/api/public/v1/status",
        "/status/%00",
        "/status/%00/api/public/v1/status",
    ] {
        assert_eq!(d.get(path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    d.done().await;
}

#[tokio::test]
#[ignore]
async fn subscribing_under_a_page_s_path_subscribes_to_that_page() {
    let Some(d) = deploy(path_based).await else {
        return;
    };
    let two = &d.two.slug;
    sqlx::query("UPDATE status_pages SET public_locale = 'es' WHERE id = $1")
        .bind(d.two.id)
        .execute(&d.pool)
        .await
        .unwrap();
    let reply = d
        .post_form(
            &format!("/status/{two}/subscribe"),
            "",
            &[("channel", "email"), ("email", "reader@example.org")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.body.contains(&format!(r#"href="/status/{two}""#)));

    let (page,): (Uuid,) = sqlx::query_as(
        "SELECT status_page_id FROM status_page_subscribers WHERE org_id = $1 AND target = $2",
    )
    .bind(d.org.0)
    .bind("reader@example.org")
    .fetch_one(&d.pool)
    .await
    .unwrap();
    assert_eq!(page, d.two.id);

    let confirm = d
        .mail
        .sent()
        .into_iter()
        .find_map(|m| match m.template {
            EmailTemplate::SubscriberConfirm { confirm_url, .. } => Some(confirm_url),
            _ => None,
        })
        .expect("confirm mailed");
    // The base domain is set, as the shipped compose sets it, but pages here
    // have no hosts of their own.
    assert!(
        confirm.starts_with(&format!("{ORIGIN}/subscribe/confirm?token=")),
        "{confirm}"
    );

    // The confirm link names no page; the subscription does.
    let confirmed = d.get(confirm.trim_start_matches(ORIGIN)).await;
    assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.body);
    assert!(
        confirmed.body.contains("Recibirá un aviso"),
        "{}",
        confirmed.body
    );
    assert!(confirmed.body.contains(&format!(r#"href="/status/{two}""#)));
    d.done().await;
}

#[tokio::test]
#[ignore]
async fn a_private_page_signs_in_under_its_own_path_and_scopes_its_session_there() {
    let Some(d) = deploy(path_based).await else {
        return;
    };
    let two = &d.two.slug;
    d.make_private(&d.two, &["example.org"]).await;

    let gated = d.get(&format!("/status/{two}")).await;
    assert_eq!(gated.status, StatusCode::UNAUTHORIZED);
    assert!(
        gated
            .body
            .contains(&format!(r#"action="/status/{two}/access/request""#))
    );

    let asked = d
        .post_form(
            &format!("/status/{two}/access/request"),
            "",
            &[
                ("email", "k@example.org"),
                ("next", &format!("/status/{two}")),
            ],
        )
        .await;
    assert_eq!(asked.status, StatusCode::OK, "{}", asked.body);
    let nonce_cookie = asked.set_cookie("_up_page_code").expect("code nonce");
    assert!(
        nonce_cookie.contains(&format!("Path=/status/{two}/access")),
        "{nonce_cookie}"
    );
    let nonce = nonce_cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches("_up_page_code=");

    let (code, url) = d.mailed_code("k@example.org").await.expect("code mailed");
    assert!(
        url.starts_with(&format!("{ORIGIN}/status/{two}/access/verify?token=")),
        "{url}"
    );

    let done = d
        .post_form(
            &format!("/status/{two}/access/code"),
            &format!("_up_page_code={nonce}"),
            &[("code", &code), ("next", &format!("/status/{two}"))],
        )
        .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
    let session = done.set_cookie("_up_page").expect("session cookie");
    assert!(
        session.contains(&format!("Path=/status/{two}")),
        "{session}"
    );
    let session = session.split(';').next().unwrap();

    let page = d.get_with(&format!("/status/{two}"), session).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(
        page.body
            .contains(&format!(r#"action="/status/{two}/access/sign-out""#))
    );
    d.done().await;
}

/// With both flags on, pages have hosts of their own, so neither a page host
/// nor the operator's serves a page by path.
#[tokio::test]
#[ignore]
async fn no_host_serves_a_page_by_path_where_pages_have_hosts() {
    let Some(d) = deploy(|cfg| {
        cfg.tenancy.path_based_public_routes = true;
        cfg.tenancy.subdomain_public_routes = true;
    })
    .await
    else {
        return;
    };
    let two = &d.two.slug;
    for host in [
        format!("{}.{SAAS_BASE_DOMAIN}", d.one.slug),
        format!("app.{SAAS_BASE_DOMAIN}"),
    ] {
        for path in [
            format!("/status/{two}"),
            format!("/status/{two}/access/request"),
            format!("/status/{two}/api/public/v1/status"),
        ] {
            let req = Request::get(&path)
                .header(header::HOST, &host)
                .body(Body::empty())
                .unwrap();
            let resp = d.app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{host}{path}");
        }
    }
    d.done().await;
}

#[tokio::test]
#[ignore]
async fn the_primary_page_s_old_addresses_lead_to_its_own_path() {
    let Some(d) = deploy(path_based).await else {
        return;
    };
    let one = &d.one.slug;
    let incident = Uuid::now_v7();
    for (old, new) in [
        ("/status".to_owned(), format!("/status/{one}")),
        (
            "/status?fragment=1".to_owned(),
            format!("/status/{one}?fragment=1"),
        ),
        (
            format!("/status/incidents/{incident}"),
            format!("/status/{one}/incidents/{incident}"),
        ),
    ] {
        let reply = d.get(&old).await;
        assert_eq!(reply.status, StatusCode::TEMPORARY_REDIRECT, "{old}");
        assert_eq!(reply.headers[header::LOCATION], new.as_str(), "{old}");
    }
    // An embedded badge or a feed reader may not follow a redirect.
    assert_eq!(d.get("/api/public/v1/status").await.status, StatusCode::OK);
    d.done().await;
}

#[tokio::test]
#[ignore]
async fn a_second_live_org_closes_the_path_based_surface() {
    let Some(d) = deploy(path_based).await else {
        return;
    };
    let other = common::make_user(&d.pool, "path-mode-other").await;
    uptimepage::storage::create_org_with_owner(&d.pool, other, &unique_slug("pm2"), "Other")
        .await
        .unwrap()
        .unwrap();
    for path in [
        "/status".to_owned(),
        format!("/status/{}", d.one.slug),
        format!("/status/{}/api/public/v1/status", d.two.slug),
    ] {
        assert_eq!(d.get(&path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    d.done().await;
}
