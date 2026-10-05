//! The OAuth sign-in dance end to end, with GitLab as the provider because its
//! endpoint follows the configured instance: a local stand-in answers the code
//! exchange with whatever claims a test hands it. Covers what a callback
//! decides on its own: who signs in, which account an attested address may
//! open, what a state is good for, and where the browser lands. Logout closes
//! the file, since it is the other half of the same `/auth` surface.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Each test gets its own database so
//! the login-attempt and account rows it counts are its own.

use crate::common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::auth::oauth_state;
use uptimepage::config::AppConfig;
use uptimepage::domain::UserId;
use uptimepage::email::{EmailTemplate, InMemoryEmailSender};
use uptimepage::request::auth::csrf::{CSRF_HEADER, CSRF_HEADER_VALUE};
use uuid::Uuid;

/// Claims the stand-in returns, keyed by the code that asks for them.
type Issued = Arc<Mutex<HashMap<String, Value>>>;

const CLIENT_ID: &str = "gitlab-client";
const CLIENT_SECRET: &str = "gitlab-secret";
const REDIRECT_URL: &str = "https://app.test/auth/gitlab/callback";

/// Answers only the exchange the app must send: the code it was handed, with
/// the configured client and redirect.
async fn token(
    State(issued): State<Issued>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let field = |k: &str| form.get(k).map(String::as_str);
    if field("grant_type") != Some("authorization_code")
        || field("client_id") != Some(CLIENT_ID)
        || field("client_secret") != Some(CLIENT_SECRET)
        || field("redirect_uri") != Some(REDIRECT_URL)
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let claims = issued
        .lock()
        .unwrap()
        .remove(field("code").unwrap_or_default())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let segment = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    Ok(Json(json!({
        "access_token": "gitlab-access",
        "token_type": "Bearer",
        "id_token": format!("{}.{}.sig", segment(&json!({ "alg": "none" })), segment(&claims)),
    })))
}

struct Rig {
    app: Router,
    pool: PgPool,
    mail: Arc<InMemoryEmailSender>,
    issued: Issued,
    gitlab: String,
    db_name: String,
}

async fn rig(prefix: &str, methods: &[&str]) -> Option<Rig> {
    let (db_url, db_name) = common::fresh_test_db(prefix).await?;
    let pool = common::open_test_pool(&db_url).await;
    let issued: Issued = Arc::default();
    let addr = common::spawn_router(
        Router::new()
            .route("/oauth/token", post(token))
            .with_state(issued.clone()),
    )
    .await;
    let gitlab = format!("http://{addr}");
    let configure = |cfg: &mut AppConfig| {
        cfg.auth.enabled_methods = methods.iter().map(|m| m.to_string()).collect();
        cfg.auth.public_base_url = "https://app.test".into();
        cfg.auth.gitlab.client.client_id = CLIENT_ID.into();
        cfg.auth.gitlab.client.client_secret = CLIENT_SECRET.to_string().into();
        cfg.auth.gitlab.client.redirect_url = REDIRECT_URL.into();
        cfg.auth.gitlab.base_url = gitlab.clone();
    };
    let mail = Arc::new(InMemoryEmailSender::new());
    let sender = mail.clone();
    let (app, _) =
        common::build_test_app_with_pg_store_anon_tweaked(pool.clone(), configure, |mut state| {
            state.email_sender = sender;
            state
        })
        .await;
    Some(Rig {
        app,
        pool,
        mail,
        issued,
        gitlab,
        db_name,
    })
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

impl Rig {
    async fn send(&self, req: Request<Body>) -> Reply {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        Reply {
            status: resp.status(),
            headers: resp.headers().clone(),
        }
    }

    async fn get(&self, path: &str) -> Reply {
        self.send(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    /// What GitLab would attest for its user `sub`.
    fn claims(&self, sub: &str, email: &str, verified: bool) -> Value {
        json!({
            "iss": self.gitlab,
            "sub": sub,
            "email": email,
            "email_verified": verified,
            "preferred_username": "olena",
            "name": "Olena Kovalenko",
        })
    }

    /// The state the start minted, read off the authorize redirect.
    async fn start(&self, query: &str) -> String {
        let started = self.get(&format!("/auth/gitlab/login{query}")).await;
        assert!(started.status.is_redirection(), "{}", started.status);
        let to = url::Url::parse(started.location()).unwrap();
        assert!(
            to.as_str()
                .starts_with(&format!("{}/oauth/authorize?", self.gitlab)),
            "{to}"
        );
        to.query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("state on the authorize url")
    }

    async fn callback(&self, state: &str, claims: Value) -> Reply {
        let code = Uuid::new_v4().simple().to_string();
        self.issued.lock().unwrap().insert(code.clone(), claims);
        self.get(&format!("/auth/gitlab/callback?code={code}&state={state}"))
            .await
    }

    /// A whole dance: start with `query`, come back with `claims`.
    async fn sign_in(&self, query: &str, claims: Value) -> Reply {
        let state = self.start(query).await;
        self.callback(&state, claims).await
    }

    async fn user_by_email(&self, email: &str) -> Option<Uuid> {
        sqlx::query_scalar("SELECT id FROM users WHERE email = $1")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .unwrap()
    }

    async fn email_of(&self, user: UserId) -> String {
        sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
            .bind(user.0)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn identities(&self, user: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT provider_user_id FROM oauth_identities WHERE user_id = $1 AND provider = 'gitlab'",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn link_origins(&self, user: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT origin FROM credential_events WHERE user_id = $1 AND action = 'linked'",
        )
        .bind(user)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn sessions(&self, user: Uuid) -> i64 {
        common::session_count(&self.pool, user).await
    }

    async fn failures(&self, reason: &str) -> i64 {
        common::login_failures(&self.pool, "gitlab_oauth", reason).await
    }

    async fn done(self) {
        self.pool.close().await;
        common::drop_test_db(&self.db_name).await;
    }
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_first_sign_in_opens_a_new_account_and_its_state_works_once() {
    let Some(rig) = rig("oauth_new", &["gitlab_oauth"]).await else {
        return;
    };
    let email = format!("olena.{}@example.com", Uuid::new_v4().simple());
    let state = rig.start("").await;

    let signed_in = rig.callback(&state, rig.claims("42", &email, true)).await;

    assert!(signed_in.status.is_redirection(), "{}", signed_in.status);
    assert_eq!(signed_in.location(), "/");
    assert!(
        common::issued_session(&signed_in.headers).is_some(),
        "{:?}",
        signed_in.headers
    );
    let user = rig.user_by_email(&email).await.expect("account created");
    assert_eq!(
        rig.identities(user).await,
        vec![format!("{}/42", rig.gitlab)]
    );
    assert_eq!(rig.link_origins(user).await, vec!["signup".to_string()]);
    let owned: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memberships WHERE user_id = $1 AND role = 'owner'",
    )
    .bind(user)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert_eq!(owned, 1, "a signup comes with its own organization");

    let replayed = rig.callback(&state, rig.claims("42", &email, true)).await;
    assert_eq!(replayed.status, StatusCode::BAD_REQUEST);
    assert_eq!(rig.failures("invalid_state").await, 1);
    assert_eq!(rig.sessions(user).await, 1);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_returning_identity_lands_where_the_start_asked_if_it_is_on_this_site() {
    let Some(rig) = rig("oauth_back", &["gitlab_oauth"]).await else {
        return;
    };
    let email = format!("olena.{}@example.com", Uuid::new_v4().simple());
    rig.sign_in("", rig.claims("7", &email, true)).await;
    let user = rig.user_by_email(&email).await.unwrap();

    for (asked, landed) in [
        ("/settings/account", "/settings/account"),
        ("https://evil.test/", "/"),
        ("//evil.test/", "/"),
    ] {
        let query = format!(
            "?redirect_after={}",
            url::form_urlencoded::byte_serialize(asked.as_bytes()).collect::<String>()
        );
        let back = rig.sign_in(&query, rig.claims("7", &email, true)).await;
        assert_eq!(back.location(), landed, "{asked}");
        assert!(common::issued_session(&back.headers).is_some());
    }

    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = $1")
        .bind(&email)
        .fetch_one(&rig.pool)
        .await
        .unwrap();
    assert_eq!(accounts, 1);
    assert_eq!(rig.identities(user).await.len(), 1);
    assert_eq!(rig.sessions(user).await, 4);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_attested_address_opens_its_account_and_tells_the_owner() {
    let Some(rig) = rig("oauth_match", &["gitlab_oauth"]).await else {
        return;
    };
    let existing = common::make_user(&rig.pool, "olena").await;
    let email = rig.email_of(existing).await;

    let signed_in = rig.sign_in("", rig.claims("99", &email, true)).await;

    assert_eq!(signed_in.location(), "/");
    assert_eq!(rig.identities(existing.0).await.len(), 1);
    assert_eq!(
        rig.link_origins(existing.0).await,
        vec!["email_match".to_string()]
    );
    assert_eq!(rig.sessions(existing.0).await, 1);
    for _ in 0..50 {
        if !rig.mail.sent().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Long enough for a second, separately spawned send to land too.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let told = rig.mail.sent();
    assert_eq!(
        told.len(),
        1,
        "the owner hears about a provider letting itself in"
    );
    assert_eq!(told[0].to.address, email);
    assert!(matches!(
        told[0].template,
        EmailTemplate::IdentityLinked { .. }
    ));

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_unconfirmed_address_opens_nothing() {
    let Some(rig) = rig("oauth_unverified", &["gitlab_oauth"]).await else {
        return;
    };
    let existing = common::make_user(&rig.pool, "olena").await;
    let email = rig.email_of(existing).await;

    let refused = rig.sign_in("", rig.claims("13", &email, false)).await;

    assert_eq!(refused.location(), "/login");
    assert!(common::issued_session(&refused.headers).is_none());
    assert!(rig.identities(existing.0).await.is_empty());
    assert_eq!(rig.sessions(existing.0).await, 0);
    assert_eq!(rig.failures("no_verified_email").await, 1);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn an_identity_from_another_instance_is_refused() {
    let Some(rig) = rig("oauth_issuer", &["gitlab_oauth"]).await else {
        return;
    };
    let email = format!("olena.{}@example.com", Uuid::new_v4().simple());
    let mut claims = rig.claims("42", &email, true);
    claims["iss"] = json!("https://gitlab.com");

    let refused = rig.sign_in("", claims).await;

    assert!(
        refused.status.is_server_error() || refused.location() == "/login",
        "{}",
        refused.status
    );
    assert!(common::issued_session(&refused.headers).is_none());
    assert!(rig.user_by_email(&email).await.is_none());
    assert_eq!(rig.failures("oauth_upstream_failed").await, 1);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_state_is_good_for_one_answer_from_its_own_provider() {
    let Some(rig) = rig("oauth_state", &["gitlab_oauth"]).await else {
        return;
    };
    let email = format!("olena.{}@example.com", Uuid::new_v4().simple());

    let github = oauth_state::generate_state();
    oauth_state::insert(&rig.pool, &github, "github", Default::default())
        .await
        .unwrap();
    for state in [github, oauth_state::generate_state()] {
        let refused = rig.callback(&state, rig.claims("42", &email, true)).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(rig.failures("invalid_state").await, 2);

    let state = rig.start("").await;
    let denied = rig
        .get(&format!(
            "/auth/gitlab/callback?error=access_denied&state={state}"
        ))
        .await;
    assert_eq!(denied.location(), "/login");
    assert_eq!(rig.failures("oauth_denied").await, 1);
    let late = rig.callback(&state, rig.claims("42", &email, true)).await;
    assert_eq!(
        late.status,
        StatusCode::BAD_REQUEST,
        "a denial still spends the state"
    );

    assert!(rig.user_by_email(&email).await.is_none());

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_switched_off_provider_neither_starts_nor_finishes() {
    let Some(rig) = rig("oauth_off", &["magic_link"]).await else {
        return;
    };
    let state = oauth_state::generate_state();
    oauth_state::insert(&rig.pool, &state, "gitlab", Default::default())
        .await
        .unwrap();

    assert_eq!(
        rig.get("/auth/gitlab/login").await.status,
        StatusCode::NOT_FOUND
    );
    let email = format!("olena.{}@example.com", Uuid::new_v4().simple());
    let finished = rig.callback(&state, rig.claims("42", &email, true)).await;
    assert_eq!(
        finished.status,
        StatusCode::NOT_FOUND,
        "a state minted before the switch-off finishes nothing"
    );
    assert!(rig.user_by_email(&email).await.is_none());

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn logout_ends_this_session_and_logout_all_ends_every_one() {
    let Some(rig) = rig("oauth_logout", &["gitlab_oauth"]).await else {
        return;
    };
    let user = common::make_user(&rig.pool, "olena").await;
    let mut cookies = Vec::new();
    for _ in 0..3 {
        cookies.push(common::session_cookie(&rig.pool, user).await);
    }
    let post = |path: &str, cookie: &str, csrf: bool| {
        let mut req = Request::post(path).header(header::COOKIE, cookie);
        if csrf {
            req = req.header(CSRF_HEADER, CSRF_HEADER_VALUE);
        }
        req.body(Body::empty()).unwrap()
    };

    let forged = rig.send(post("/auth/logout-all", &cookies[0], false)).await;
    assert_eq!(
        forged.status,
        StatusCode::FORBIDDEN,
        "a cross-site POST ends nothing"
    );
    assert_eq!(rig.sessions(user.0).await, 3);

    let out = rig.send(post("/auth/logout", &cookies[0], true)).await;
    assert_eq!(out.location(), "/login");
    assert!(common::cleared_session(&out.headers), "{:?}", out.headers);
    assert_eq!(
        rig.sessions(user.0).await,
        2,
        "only this browser's session ends"
    );

    let all = rig.send(post("/auth/logout-all", &cookies[1], true)).await;
    assert_eq!(all.location(), "/login");
    assert_eq!(rig.sessions(user.0).await, 0);

    rig.done().await;
}
