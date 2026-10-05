//! Public status-page subscriptions through the SaaS router on live Postgres:
//! the double opt-in round trip on the page's own host, the daily mail cap
//! behind one unchanging "check your inbox" answer, addresses refused before
//! anything is stored, and the signed unsubscribe link that only a POST acts on.
//!
//! Live-PG ignored: needs `DATABASE_URL`.

use crate::common;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::domain::{NewSubscriber, SubscriberChannel};
use uptimepage::email::{EmailTemplate, InMemoryEmailSender};
use uptimepage::storage::subscribers::{
    self, PER_SUBSCRIBER_DAILY_CAP, unsubscribe_token, unsubscribe_url,
};
use uuid::Uuid;

use common::{SAAS_BASE_DOMAIN, build_saas_router_with_pg_tweaked, pg_pool_from_env, unique_slug};

const SECRET: &str = "subscribe-test-unsubscribe-secret";

struct Rig {
    app: Router,
    pool: PgPool,
    mail: Arc<InMemoryEmailSender>,
    org: Uuid,
    page: Uuid,
    host: String,
}

async fn rig() -> Option<Rig> {
    rig_with_secret(SECRET).await
}

async fn rig_with_secret(secret: &str) -> Option<Rig> {
    let pool = pg_pool_from_env().await?;
    let (org,): (Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Acme', a.id FROM a RETURNING id",
    )
    .bind(unique_slug("sub-org"))
    .fetch_one(&pool)
    .await
    .unwrap();
    let slug = unique_slug("subs");
    let (page,): (Uuid,) = sqlx::query_as(
        "INSERT INTO status_pages (org_id, slug, name, enabled) \
         VALUES ($1, $2, 'Acme Status', true) RETURNING id",
    )
    .bind(org)
    .bind(&slug)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mail = Arc::new(InMemoryEmailSender::new());
    let sender = mail.clone();
    let secret = secret.to_string();
    let app = build_saas_router_with_pg_tweaked(
        pool.clone(),
        |_| {},
        move |mut state| {
            state.email_sender = sender;
            state.with_subscription_unsubscribe_secret(secret)
        },
    )
    .await;
    Some(Rig {
        app,
        pool,
        mail,
        org,
        page,
        host: format!("{slug}.{SAAS_BASE_DOMAIN}"),
    })
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Unique per run: the mint caps count every page's tokens for an address.
fn address(tag: &str) -> String {
    format!("olena.{}@example.com", unique_slug(tag))
}

impl Rig {
    async fn call(
        &self,
        host: &str,
        req: axum::http::request::Builder,
        body: Body,
    ) -> (StatusCode, String) {
        let resp = self
            .app
            .clone()
            .oneshot(req.header(header::HOST, host).body(body).unwrap())
            .await
            .unwrap();
        (resp.status(), common::body_text(resp).await)
    }

    async fn subscribe_on(&self, host: &str, pairs: &[(&str, &str)]) -> (StatusCode, String) {
        self.call(
            host,
            Request::post("/subscribe")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            Body::from(form(pairs)),
        )
        .await
    }

    async fn subscribe(&self, email: &str) -> (StatusCode, String) {
        self.subscribe_on(&self.host, &[("email", email)]).await
    }

    async fn send(&self, method: Method, path_and_query: &str) -> (StatusCode, String) {
        self.call(
            &self.host,
            Request::builder().method(method).uri(path_and_query),
            Body::empty(),
        )
        .await
    }

    /// `(verified, target)` of every subscriber on this rig's page.
    async fn subscribers(&self) -> Vec<(bool, String)> {
        sqlx::query_as(
            "SELECT verified_at IS NOT NULL, target FROM status_page_subscribers \
             WHERE status_page_id = $1 ORDER BY created_at",
        )
        .bind(self.page)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// A pending subscriber written straight to the store, for the
    /// unsubscribe tests that need a row but not the mail round trip.
    async fn pending_subscriber(&self, email: &str) -> Uuid {
        subscribers::subscribe(
            &self.pool,
            &NewSubscriber {
                status_page_id: self.page,
                org_id: self.org,
                channel: SubscriberChannel::Email,
                target: email.into(),
                config: serde_json::json!({}),
            },
        )
        .await
        .unwrap()
        .id
    }

    fn origin(&self) -> String {
        format!("https://{}", self.host)
    }

    async fn cleanup(&self) {
        common::delete_org_and_account(&self.pool, self.org).await;
    }
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_mailed_link_confirms_the_subscription_on_the_page_host() {
    let Some(rig) = rig().await else {
        return;
    };
    let stored = address("sub-rt");
    let typed = stored.to_ascii_uppercase();

    let (status, html) = rig.subscribe(&typed).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Almost there"));
    assert_eq!(
        rig.subscribers().await,
        vec![(false, stored.clone())],
        "pending until the link is followed, case folded so variants share one row"
    );

    let sent = rig.mail.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to.address, stored);
    let EmailTemplate::SubscriberConfirm {
        confirm_url,
        unsubscribe_url,
        ..
    } = &sent[0].template
    else {
        panic!(
            "expected a subscriber confirmation, got {:?}",
            sent[0].template
        );
    };
    let confirm_prefix = format!("{}/subscribe/confirm?token=", rig.origin());
    assert!(
        confirm_url.starts_with(&confirm_prefix),
        "links point at the page's own host, never the app: {confirm_url}"
    );
    assert!(unsubscribe_url.starts_with(&format!("{}/subscribe/unsubscribe?s=", rig.origin())));

    let path = confirm_url.strip_prefix(&rig.origin()).unwrap();
    let (status, _) = rig.send(Method::GET, path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rig.subscribers().await, vec![(true, stored)]);

    let (again, html) = rig.send(Method::GET, path).await;
    assert_eq!(
        again,
        StatusCode::NOT_FOUND,
        "a confirm token is single-use"
    );
    assert!(html.contains("Link expired"));

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn repeats_past_the_daily_cap_mail_nothing_and_answer_the_same() {
    let Some(rig) = rig().await else {
        return;
    };
    let email = address("sub-cap");
    let attempts = PER_SUBSCRIBER_DAILY_CAP as usize + 2;

    let (first_status, first_html) = rig.subscribe(&email).await;
    for n in 1..attempts {
        let (status, html) = rig.subscribe(&email).await;
        assert_eq!(status, first_status, "attempt {n}");
        assert_eq!(
            html, first_html,
            "attempt {n} must not reveal membership or the cap"
        );
    }

    assert_eq!(rig.mail.len(), PER_SUBSCRIBER_DAILY_CAP as usize);
    assert_eq!(
        rig.subscribers().await.len(),
        1,
        "one row however often it is asked"
    );

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_address_that_must_not_be_mailed_is_refused_before_anything_is_stored() {
    let Some(rig) = rig().await else {
        return;
    };
    for email in [
        "",
        "not-an-address",
        "postmaster@example.com",
        "no-reply@example.com",
    ] {
        let (status, html) = rig.subscribe(email).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{email:?}");
        assert!(html.contains("Check the address"), "{email:?}");
    }
    assert!(rig.subscribers().await.is_empty());
    assert_eq!(rig.mail.len(), 0);

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn only_a_published_page_takes_subscriptions() {
    let Some(rig) = rig().await else {
        return;
    };
    let email = address("sub-host");
    let unknown = format!("{}.{SAAS_BASE_DOMAIN}", unique_slug("nopage"));

    let (status, _) = rig.subscribe_on(&unknown, &[("email", &email)]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    sqlx::query("UPDATE status_pages SET enabled = false WHERE id = $1")
        .bind(rig.page)
        .execute(&rig.pool)
        .await
        .unwrap();
    let (status, _) = rig.subscribe(&email).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a disabled page is not public"
    );

    assert!(rig.subscribers().await.is_empty());
    assert_eq!(rig.mail.len(), 0);

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_webhook_must_be_https() {
    let Some(rig) = rig().await else {
        return;
    };
    for url in ["http://hooks.example.com/status", "not a url", ""] {
        let (status, html) = rig
            .subscribe_on(&rig.host, &[("channel", "webhook"), ("url", url)])
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url:?}");
        assert!(html.contains("Check the URL"), "{url:?}");
    }
    assert!(rig.subscribers().await.is_empty());

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_unsubscribe_link_asks_first_and_only_a_post_removes() {
    let Some(rig) = rig().await else {
        return;
    };
    let id = rig.pending_subscriber(&address("sub-unsub")).await;
    let link = unsubscribe_url(SECRET, &rig.origin(), id);
    let path = link.strip_prefix(&rig.origin()).unwrap();

    let (status, html) = rig.send(Method::GET, path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(&format!("action=\"{}\"", path.replace('&', "&amp;"))),
        "the confirmation posts the same signed link back"
    );
    assert_eq!(
        rig.subscribers().await.len(),
        1,
        "a prefetch must not unsubscribe"
    );

    let (status, html) = rig.send(Method::POST, path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Unsubscribed"));
    assert!(rig.subscribers().await.is_empty());

    let (status, _) = rig.send(Method::POST, path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a second click is a no-op, not an error"
    );

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_forged_unsubscribe_link_removes_nothing() {
    let Some(rig) = rig().await else {
        return;
    };
    let id = rig.pending_subscriber(&address("sub-forge")).await;
    let other = rig.pending_subscriber(&address("sub-forge-b")).await;
    let forged = [
        format!("s={id}&t={}", "0".repeat(64)),
        format!("s={id}&t={}", unsubscribe_token(SECRET, other)),
        format!("s={id}&t={}", unsubscribe_token("another-secret", id)),
        format!("s={id}&t="),
        format!("s=not-a-uuid&t={}", unsubscribe_token(SECRET, id)),
        String::new(),
    ];

    for query in &forged {
        for method in [Method::GET, Method::POST] {
            let path = format!("/subscribe/unsubscribe?{query}");
            let (status, html) = rig.send(method.clone(), &path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {query}");
            assert!(html.contains("Link invalid"), "{method} {query}");
        }
    }
    assert_eq!(rig.subscribers().await.len(), 2);

    rig.cleanup().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn without_a_secret_no_unsubscribe_link_is_valid() {
    let Some(rig) = rig_with_secret("").await else {
        return;
    };
    let id = rig.pending_subscriber(&address("sub-nosecret")).await;
    let link = unsubscribe_url("", &rig.origin(), id);
    let path = link.strip_prefix(&rig.origin()).unwrap();

    for method in [Method::GET, Method::POST] {
        let (status, _) = rig.send(method.clone(), path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
    }
    assert_eq!(rig.subscribers().await.len(), 1);

    rig.cleanup().await;
}
