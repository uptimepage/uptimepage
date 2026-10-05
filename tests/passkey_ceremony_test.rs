//! Both passkey ceremonies through the router, answered by a software
//! authenticator: a registered passkey signs its account in, a challenge
//! answers once and only the ceremony and account it was issued to, and the
//! user handle in an assertion is never trusted on its own.
//!
//! The soft authenticator keeps no resident keys, so each sign-in names the
//! credential and attaches the account id as a discoverable one would. Neither
//! is signed, so the server still verifies a real assertion.
//!
//! Live-PG ignored: needs `DATABASE_URL`. Each test gets its own database so
//! the login-attempt rows it counts are its own.

use crate::common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uptimepage::config::AppConfig;
use uptimepage::domain::UserId;
use uptimepage::request::auth::csrf::{CSRF_HEADER, CSRF_HEADER_VALUE};
use uptimepage::storage::passkeys;
use webauthn_authenticator_rs::prelude::{
    CreationChallengeResponse, PublicKeyCredential, RegisterPublicKeyCredential,
    RequestChallengeResponse, Url, WebauthnAuthenticator,
};
use webauthn_authenticator_rs::softpasskey::SoftPasskey;

const BASE: &str = "https://app.test";

struct Rig {
    app: Router,
    pool: PgPool,
    db_name: String,
}

async fn rig(prefix: &str, methods: &[&str]) -> Option<Rig> {
    let (db_url, db_name) = common::fresh_test_db(prefix).await?;
    let pool = common::open_test_pool(&db_url).await;
    let configure = |cfg: &mut AppConfig| {
        cfg.auth.enabled_methods = methods.iter().map(|m| m.to_string()).collect();
        cfg.auth.public_base_url = BASE.into();
    };
    let (app, _) = common::build_test_app_with_pg_store_anon(pool.clone(), configure).await;
    Some(Rig { app, pool, db_name })
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or_default()
    }
}

impl Rig {
    async fn signed_in(&self, user: UserId) -> String {
        common::session_cookie(&self.pool, user).await
    }

    async fn post(&self, path: &str, cookie: Option<&str>, body: Value) -> Reply {
        let mut req = Request::post(path).header(header::CONTENT_TYPE, "application/json");
        if let Some(cookie) = cookie {
            req = req
                .header(header::COOKIE, cookie)
                .header(CSRF_HEADER, CSRF_HEADER_VALUE);
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let text = common::body_text(resp).await;
        Reply {
            status,
            headers,
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
        }
    }

    /// `(handle, options)` of a fresh registration for whoever `cookie` is.
    async fn start_registration(&self, cookie: &str) -> (String, CreationChallengeResponse) {
        let started = self
            .post("/auth/passkey/register/start", Some(cookie), json!({}))
            .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
        (
            started.body["handle"].as_str().unwrap().to_string(),
            serde_json::from_value(started.body["options"].clone()).unwrap(),
        )
    }

    async fn finish_registration(
        &self,
        cookie: &str,
        handle: &str,
        credential: &RegisterPublicKeyCredential,
    ) -> Reply {
        self.post(
            "/auth/passkey/register/finish",
            Some(cookie),
            json!({ "handle": handle, "nickname": "  Laptop  ", "credential": credential }),
        )
        .await
    }

    /// Registers a passkey on `user` and returns the credential it minted.
    async fn register(
        &self,
        user: UserId,
        authenticator: &mut SoftPasskey,
    ) -> RegisterPublicKeyCredential {
        let cookie = self.signed_in(user).await;
        let (handle, options) = self.start_registration(&cookie).await;
        let credential = authenticator
            .do_registration(origin(), options)
            .expect("authenticator registers");
        let finished = self
            .finish_registration(&cookie, &handle, &credential)
            .await;
        assert_eq!(finished.status, StatusCode::NO_CONTENT, "{}", finished.body);
        credential
    }

    /// `(handle, options)` of a fresh sign-in.
    async fn start_login(&self, body: Value) -> (String, Value) {
        let started = self.post("/auth/passkey/login/start", None, body).await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
        (
            started.body["handle"].as_str().unwrap().to_string(),
            started.body["options"].clone(),
        )
    }

    async fn finish_login(&self, handle: &str, credential: &PublicKeyCredential) -> Reply {
        self.post(
            "/auth/passkey/login/finish",
            None,
            json!({ "handle": handle, "credential": credential }),
        )
        .await
    }

    async fn failures(&self, reason: &str) -> i64 {
        common::login_failures(&self.pool, "passkey", reason).await
    }

    async fn sessions(&self, user: UserId) -> i64 {
        common::session_count(&self.pool, user.0).await
    }

    async fn done(self) {
        self.pool.close().await;
        common::drop_test_db(&self.db_name).await;
    }
}

fn origin() -> Url {
    Url::parse(BASE).unwrap()
}

/// What a discoverable authenticator holding `credential` for `account` sends.
fn assertion_for(
    authenticator: &mut SoftPasskey,
    mut options: Value,
    credential: &RegisterPublicKeyCredential,
    account: UserId,
) -> PublicKeyCredential {
    options["publicKey"]["allowCredentials"] =
        json!([{ "type": "public-key", "id": credential.id }]);
    let options: RequestChallengeResponse = serde_json::from_value(options).unwrap();
    let mut assertion = authenticator
        .do_authentication(origin(), options)
        .expect("authenticator signs");
    assertion.response.user_handle = Some(account.0.as_bytes().to_vec());
    assertion
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_registered_passkey_signs_its_account_in() {
    let Some(rig) = rig("pkc_signin", &["passkey"]).await else {
        return;
    };
    let user = common::make_user(&rig.pool, "olena").await;
    let mut authenticator = SoftPasskey::new(true);
    let credential = rig.register(user, &mut authenticator).await;

    let stored = passkeys::list_for_user(&rig.pool, user).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].nickname.as_deref(), Some("Laptop"));
    assert_eq!(stored[0].rp_id, "app.test");
    let registered_at = stored[0].last_used_at;
    let sessions_before = rig.sessions(user).await;

    let (handle, options) = rig
        .start_login(json!({ "redirect_after": "/settings/account" }))
        .await;
    let assertion = assertion_for(&mut authenticator, options, &credential, user);
    let signed_in = rig.finish_login(&handle, &assertion).await;

    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    assert_eq!(signed_in.body["redirect"], "/settings/account");
    assert!(
        common::issued_session(&signed_in.headers).is_some(),
        "{:?}",
        signed_in.headers
    );
    assert_eq!(rig.sessions(user).await, sessions_before + 1);
    let used = passkeys::list_for_user(&rig.pool, user).await.unwrap();
    assert!(
        used[0].last_used_at > registered_at,
        "a sign-in stamps its use"
    );

    let replayed = rig.finish_login(&handle, &assertion).await;
    assert_eq!(replayed.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        replayed.code(),
        "PASSKEY_CHALLENGE_SPENT",
        "an answer works once"
    );
    assert_eq!(rig.sessions(user).await, sessions_before + 1);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_redirect_off_the_site_is_dropped_at_the_start() {
    let Some(rig) = rig("pkc_redirect", &["passkey"]).await else {
        return;
    };
    let user = common::make_user(&rig.pool, "olena").await;
    let mut authenticator = SoftPasskey::new(true);
    let credential = rig.register(user, &mut authenticator).await;

    for target in ["https://evil.test/", "//evil.test/", "javascript:alert(1)"] {
        let (handle, options) = rig.start_login(json!({ "redirect_after": target })).await;
        let assertion = assertion_for(&mut authenticator, options, &credential, user);
        let signed_in = rig.finish_login(&handle, &assertion).await;
        assert_eq!(
            signed_in.status,
            StatusCode::OK,
            "{target}: {}",
            signed_in.body
        );
        assert_eq!(signed_in.body["redirect"], "/", "{target}");
    }

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn the_user_handle_alone_signs_nobody_in() {
    let Some(rig) = rig("pkc_handle", &["passkey"]).await else {
        return;
    };
    let mallory = common::make_user(&rig.pool, "mallory").await;
    let victim = common::make_user(&rig.pool, "victim").await;
    let bare = common::make_user(&rig.pool, "bare").await;
    let mut mallorys = SoftPasskey::new(true);
    let mut victims = SoftPasskey::new(true);
    let credential = rig.register(mallory, &mut mallorys).await;
    rig.register(victim, &mut victims).await;
    let mut sessions_before = Vec::new();
    for account in [victim, bare, mallory] {
        sessions_before.push(rig.sessions(account).await);
    }

    let (handle, options) = rig.start_login(json!({})).await;
    let claimed = assertion_for(&mut mallorys, options, &credential, victim);
    let refused = rig.finish_login(&handle, &claimed).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.code(), "PASSKEY_CHALLENGE_SPENT");
    assert!(common::issued_session(&refused.headers).is_none());
    assert_eq!(rig.failures("assertion_rejected").await, 1);

    let (handle, options) = rig.start_login(json!({})).await;
    let claimed = assertion_for(&mut mallorys, options, &credential, bare);
    let refused = rig.finish_login(&handle, &claimed).await;
    assert_eq!(refused.code(), "PASSKEY_CHALLENGE_SPENT");
    assert_eq!(rig.failures("no_passkey_on_account").await, 1);

    let (handle, options) = rig.start_login(json!({})).await;
    let mut anonymous = assertion_for(&mut mallorys, options, &credential, mallory);
    anonymous.response.user_handle = None;
    let refused = rig.finish_login(&handle, &anonymous).await;
    assert_eq!(refused.code(), "PASSKEY_CHALLENGE_SPENT");
    assert_eq!(rig.failures("unidentifiable_credential").await, 1);

    for (account, before) in [victim, bare, mallory].into_iter().zip(sessions_before) {
        assert_eq!(rig.sessions(account).await, before, "no session opened");
    }

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_challenge_answers_only_the_ceremony_and_account_it_was_issued_to() {
    let Some(rig) = rig("pkc_owner", &["passkey"]).await else {
        return;
    };
    let olena = common::make_user(&rig.pool, "olena").await;
    let taras = common::make_user(&rig.pool, "taras").await;
    let (olena_cookie, taras_cookie) = (rig.signed_in(olena).await, rig.signed_in(taras).await);
    let mut authenticator = SoftPasskey::new(true);

    let (handle, options) = rig.start_registration(&taras_cookie).await;
    let credential = authenticator.do_registration(origin(), options).unwrap();
    let hijacked = rig
        .finish_registration(&olena_cookie, &handle, &credential)
        .await;
    assert_eq!(hijacked.code(), "PASSKEY_CHALLENGE_SPENT");
    let late = rig
        .finish_registration(&taras_cookie, &handle, &credential)
        .await;
    assert_eq!(
        late.code(),
        "PASSKEY_CHALLENGE_SPENT",
        "a refused answer still burns it"
    );

    let (handle, options) = rig.start_registration(&olena_cookie).await;
    let credential = authenticator.do_registration(origin(), options).unwrap();
    let as_login = rig
        .post(
            "/auth/passkey/login/finish",
            None,
            json!({ "handle": handle, "credential": credential }),
        )
        .await;
    assert_eq!(as_login.code(), "PASSKEY_CHALLENGE_SPENT");
    assert_eq!(rig.failures("registration_handle").await, 1);
    let late = rig
        .finish_registration(&olena_cookie, &handle, &credential)
        .await;
    assert_eq!(late.code(), "PASSKEY_CHALLENGE_SPENT");

    let (handle, _) = rig.start_login(json!({})).await;
    let as_registration = rig
        .finish_registration(&olena_cookie, &handle, &credential)
        .await;
    assert_eq!(as_registration.code(), "PASSKEY_CHALLENGE_SPENT");

    for user in [olena, taras] {
        assert!(
            passkeys::list_for_user(&rig.pool, user)
                .await
                .unwrap()
                .is_empty()
        );
    }

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_registration_that_cannot_sign_in_is_refused() {
    let Some(rig) = rig("pkc_refuse", &["passkey"]).await else {
        return;
    };
    let user = common::make_user(&rig.pool, "olena").await;
    let cookie = rig.signed_in(user).await;
    let mut authenticator = SoftPasskey::new(true);

    let (handle, options) = rig.start_registration(&cookie).await;
    let credential = authenticator.do_registration(origin(), options).unwrap();
    let mut not_resident = serde_json::to_value(&credential).unwrap();
    not_resident["extensions"]["credProps"] = json!({ "rk": false });
    let refused = rig
        .post(
            "/auth/passkey/register/finish",
            Some(&cookie),
            json!({ "handle": handle, "credential": not_resident }),
        )
        .await;
    assert_eq!(
        refused.code(),
        "PASSKEY_NOT_DISCOVERABLE",
        "{}",
        refused.body
    );

    let (handle, _) = rig.start_registration(&cookie).await;
    let malformed = rig
        .post(
            "/auth/passkey/register/finish",
            Some(&cookie),
            json!({ "handle": handle, "credential": { "id": "nope" } }),
        )
        .await;
    assert_eq!(malformed.code(), "PASSKEY_MALFORMED");

    let (handle, options) = rig.start_registration(&cookie).await;
    let mut credential = authenticator.do_registration(origin(), options).unwrap();
    credential.response.client_data_json = b"{}".to_vec();
    let unverified = rig.finish_registration(&cookie, &handle, &credential).await;
    assert_eq!(unverified.code(), "PASSKEY_REJECTED");

    assert!(
        passkeys::list_for_user(&rig.pool, user)
            .await
            .unwrap()
            .is_empty()
    );

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn a_second_registration_excludes_the_passkeys_already_held() {
    let Some(rig) = rig("pkc_exclude", &["passkey"]).await else {
        return;
    };
    let user = common::make_user(&rig.pool, "olena").await;
    let first = rig.register(user, &mut SoftPasskey::new(true)).await;

    let (_, options) = rig.start_registration(&rig.signed_in(user).await).await;
    let excluded: Vec<_> = options
        .public_key
        .exclude_credentials
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(excluded, vec![first.raw_id]);

    rig.done().await;
}

#[tokio::test]
#[ignore = "needs live Postgres (DATABASE_URL)"]
async fn no_ceremony_starts_without_passkeys_switched_on_or_a_session() {
    let Some(off) = rig("pkc_off", &["magic_link"]).await else {
        return;
    };
    let user = common::make_user(&off.pool, "olena").await;
    let cookie = off.signed_in(user).await;

    let register = off
        .post("/auth/passkey/register/start", Some(&cookie), json!({}))
        .await;
    assert_eq!(register.status, StatusCode::NOT_FOUND, "not mounted");
    let login = off.post("/auth/passkey/login/start", None, json!({})).await;
    assert_eq!(login.status, StatusCode::NOT_FOUND);
    off.done().await;

    let Some(on) = rig("pkc_anon", &["passkey"]).await else {
        return;
    };
    let anonymous = on
        .post("/auth/passkey/register/start", None, json!({}))
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    on.done().await;
}
