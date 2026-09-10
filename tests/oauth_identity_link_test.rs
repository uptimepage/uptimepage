//! The link dance authorises on the live session, never on the state alone.
//!
//! `oauth_states.link_user_id` says which account a callback should attach its
//! new identity to. If that were enough on its own, anyone who got hold of a
//! state — browser history, a shared URL — could finish the dance with their
//! own provider account and walk away with a credential on somebody else's
//! account. These tests drive the real router so the check cannot be refactored
//! out without one of them going red.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uptimepage::auth::{oauth_state, session as session_store};
use uptimepage::config::AppConfig;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

/// GitHub configured, or the routes 404 at the enabled check and every guard
/// behind them goes untested.
async fn app(pool: &sqlx::PgPool) -> axum::Router {
    let (router, _) = common::build_test_app_with_pg_store_anon(pool.clone(), |cfg| {
        cfg.auth.enabled_methods = vec!["github_oauth".into()];
        cfg.auth.github.client_id = "test-client".into();
        cfg.auth.github.client_secret = "test-secret".to_string().into();
        cfg.auth.github.redirect_url = "https://app.test/auth/github/callback".into();
    })
    .await;
    router
}

async fn session_cookie(pool: &sqlx::PgPool, user: uptimepage::domain::UserId) -> String {
    let cfg = AppConfig::load().expect("config");
    let created = session_store::create(pool, &cfg.auth.session, user, None, None, None)
        .await
        .expect("session");
    format!("{}={}", cfg.auth.session.cookie_name, created.cookie_token)
}

/// Mints a link-purpose state for `owner`, then completes the callback under
/// whatever cookie `caller` carries.
async fn complete_link_callback(
    pool: &sqlx::PgPool,
    owner: uptimepage::domain::UserId,
    caller: Option<&str>,
) -> (StatusCode, String) {
    let state = oauth_state::generate_state();
    oauth_state::insert(
        pool,
        &state,
        "github",
        oauth_state::StateBinding {
            link_user_id: Some(owner.0),
            ..Default::default()
        },
    )
    .await
    .expect("insert link state");

    let app = app(pool).await;
    let mut req = Request::builder().uri(format!("/auth/github/callback?code=x&state={state}"));
    if let Some(cookie) = caller {
        req = req.header(header::COOKIE, cookie);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    (resp.status(), location)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_link_state_alone_does_not_authorise_the_link() {
    let Some((db, name)) = common::fresh_test_db("identity_link").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = common::make_user(&pool, "owner").await;
    let stranger = common::make_user(&pool, "stranger").await;

    // No session at all: the holder of a leaked state is nobody. The exact
    // redirect matters — a 500 from somewhere else would also be "not OK", and
    // would hide the guard having been removed.
    let (status, location) = complete_link_callback(&pool, owner, None).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "an anonymous holder is bounced, not served"
    );
    assert!(
        location.starts_with("/login"),
        "bounced to sign in, got {location:?}"
    );
    assert!(
        no_identities(&pool, owner).await,
        "nothing may be attached to the owner"
    );

    // A session for someone else: still not the account the state names.
    let theirs = session_cookie(&pool, stranger).await;
    let (status, location) = complete_link_callback(&pool, owner, Some(&theirs)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "another user must not link");
    assert!(location.starts_with("/login"), "got {location:?}");
    assert!(
        no_identities(&pool, owner).await,
        "nothing may be attached to the owner"
    );
    assert!(
        no_identities(&pool, stranger).await,
        "nor to whoever completed it"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

async fn no_identities(pool: &sqlx::PgPool, user: uptimepage::domain::UserId) -> bool {
    uptimepage::storage::oauth_identities::list_for_user(pool, user)
        .await
        .expect("list")
        .is_empty()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_auto_link_is_told_apart_from_a_deliberate_one() {
    let Some((db, name)) = common::fresh_test_db("identity_origin").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let github = uptimepage::auth::oauth_login::RemoteIdentity {
        provider_user_id: "gh-1".into(),
        provider_username: None,
        verified_email: Some("origin@example.test".into()),
        display_name: None,
    };
    let owner = uptimepage::auth::oauth_login::upsert_identity_and_signup_org(
        &pool,
        uptimepage::auth::OauthProvider::Github,
        &github,
        uptimepage::security::Admission::Clear,
        uptimepage::auth::oauth_login::SignupOrg::Create,
    )
    .await
    .expect("signup");

    // Signing up is not a link — nothing to be told about yet.
    assert_eq!(origins(&pool, owner.user_id).await, Vec::<String>::new());

    // A provider letting itself in on an attested address is the one worth
    // investigating, so it must not read the same as a deliberate add.
    let google = uptimepage::auth::oauth_login::RemoteIdentity {
        provider_user_id: "g-1".into(),
        provider_username: None,
        verified_email: Some("origin@example.test".into()),
        display_name: None,
    };
    let linked = uptimepage::auth::oauth_login::upsert_identity_and_signup_org(
        &pool,
        uptimepage::auth::OauthProvider::Google,
        &google,
        uptimepage::security::Admission::Clear,
        uptimepage::auth::oauth_login::SignupOrg::Create,
    )
    .await
    .expect("email match");
    assert!(linked.newly_linked);

    uptimepage::storage::oauth_identities::record_event(
        &pool,
        owner.user_id,
        uptimepage::storage::oauth_identities::CredentialEvent {
            provider: uptimepage::auth::OauthProvider::Google.as_db_str(),
            provider_user_id: "g-1",
            action: uptimepage::auth::CredentialAction::Linked,
            origin: uptimepage::auth::CredentialOrigin::EmailMatch,
            ip_hash: Some("iphash"),
            user_agent_hash: Some("uahash"),
        },
    )
    .await;
    uptimepage::storage::oauth_identities::record_event(
        &pool,
        owner.user_id,
        uptimepage::storage::oauth_identities::CredentialEvent {
            provider: uptimepage::auth::OauthProvider::Gitlab.as_db_str(),
            provider_user_id: "gl-1",
            action: uptimepage::auth::CredentialAction::Linked,
            origin: uptimepage::auth::CredentialOrigin::Session,
            ip_hash: None,
            user_agent_hash: None,
        },
    )
    .await;

    let mut seen = origins(&pool, owner.user_id).await;
    seen.sort();
    assert_eq!(seen, vec!["email_match".to_string(), "session".to_string()]);

    let (with_ip,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM credential_events WHERE user_id = $1 AND ip_hash = 'iphash'",
    )
    .bind(owner.user_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(with_ip, 1, "\"was this me?\" needs the address");

    pool.close().await;
    common::drop_test_db(&name).await;
}

async fn origins(pool: &sqlx::PgPool, user: uptimepage::domain::UserId) -> Vec<String> {
    sqlx::query_as::<_, (String,)>("SELECT origin FROM credential_events WHERE user_id = $1")
        .bind(user.0)
        .fetch_all(pool)
        .await
        .expect("origins")
        .into_iter()
        .map(|(o,)| o)
        .collect()
}

// ── Starting a link dance ────────────────────────────────────────────────────
//
// `POST /auth/{provider}/link` mints state bound to the caller and hands back
// an authorize URL. Three things gate it, and each is load-bearing: the method
// (a GET is forgeable from any page), the CSRF header (which a plain form
// cannot set), and the session (which decides whose account grows a
// credential).

async fn link_start(app: &axum::Router, cookie: Option<&str>, csrf: bool) -> (StatusCode, String) {
    let mut req = Request::builder().method("POST").uri("/auth/github/link");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    if csrf {
        req = req.header("X-Requested-With", "uptimepage");
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn starting_a_link_needs_a_post_the_csrf_header_and_a_session() {
    let Some((db, name)) = common::fresh_test_db("link_start").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "linker").await;
    let cookie = session_cookie(&pool, user).await;
    let app = app(&pool).await;

    // A GET would let any page force a signed-in visitor into a link dance by
    // navigation or an iframe, because the CSRF guard exempts safe methods.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auth/github/link")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "GET must not start a dance"
    );

    let (status, body) = link_start(&app, Some(&cookie), false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "no CSRF header");
    assert!(body.contains("CSRF"), "got {body}");

    let (status, _) = link_start(&app, None, true).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "no session");

    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM oauth_states")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a refused start must mint no state");

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_signed_in_start_binds_the_state_to_that_user() {
    let Some((db, name)) = common::fresh_test_db("link_bind").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "binder").await;
    let cookie = session_cookie(&pool, user).await;
    let app = app(&pool).await;

    let (status, body) = link_start(&app, Some(&cookie), true).await;
    assert_eq!(status, StatusCode::OK);
    // JSON, not a 302: the CSRF header has to ride a fetch, and a redirect
    // would never reach the browser.
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json");
    let url = parsed["url"].as_str().expect("url");
    assert!(
        url.starts_with("https://github.com/login/oauth/authorize"),
        "got {url}"
    );
    assert!(url.contains("client_id=test-client"));

    let (bound,): (Option<uuid::Uuid>,) = sqlx::query_as("SELECT link_user_id FROM oauth_states")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        bound,
        Some(user.0),
        "the callback must know whose account this is"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_disabled_provider_offers_no_link() {
    let Some((db, name)) = common::fresh_test_db("link_off").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "hopeful").await;
    let cookie = session_cookie(&pool, user).await;
    let app = app(&pool).await;

    // Only github is enabled by `app`, so gitlab must refuse rather than mint
    // a dance nothing can complete.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/gitlab/link")
                .header(header::COOKIE, &cookie)
                .header("X-Requested-With", "uptimepage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn cancelling_a_link_is_not_a_failed_sign_in() {
    let Some((db, name)) = common::fresh_test_db("link_cancel").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "quitter").await;
    let cookie = session_cookie(&pool, user).await;

    let state = oauth_state::generate_state();
    oauth_state::insert(
        &pool,
        &state,
        "github",
        oauth_state::StateBinding {
            link_user_id: Some(user.0),
            ..Default::default()
        },
    )
    .await
    .expect("insert");

    // "Cancel" at the provider: no code, an error instead.
    let resp = app(&pool)
        .await
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/auth/github/callback?error=access_denied&state={state}"
                ))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap(),
        "/settings/account",
        "a signed-in user backing out belongs on the page they started from"
    );

    let (attempts,): (i64,) = sqlx::query_as("SELECT count(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "this was never a sign-in attempt");

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_link_callback_never_mints_a_session() {
    let Some((db, name)) = common::fresh_test_db("link_nosession").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "already-in").await;
    let cookie = session_cookie(&pool, user).await;

    let (before,): (i64,) = sqlx::query_as("SELECT count(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();

    let state = oauth_state::generate_state();
    oauth_state::insert(
        &pool,
        &state,
        "github",
        oauth_state::StateBinding {
            link_user_id: Some(user.0),
            ..Default::default()
        },
    )
    .await
    .expect("insert");

    let resp = app(&pool)
        .await
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/auth/github/callback?error=access_denied&state={state}"
                ))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Adding a credential is not signing in: the caller already had a session
    // and must leave with the same one, not a fresh row.
    let sets_session = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.starts_with("_sm_session="));
    assert!(!sets_session, "no session cookie may be issued here");

    let (after,): (i64,) = sqlx::query_as("SELECT count(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after, "no session row may be created");

    pool.close().await;
    common::drop_test_db(&name).await;
}

// ── Flash handling on the account page ───────────────────────────────────────
//
// The link outcome banners ride the one-shot `_sm_flash` cookie, which is
// shared with the dashboard's "account restored" and "invitation no longer
// valid" notices. `take` clears the whole cookie, so the account page has to
// put back what it does not render.

async fn account_page_flash(pool: &sqlx::PgPool, cookie: &str, flash: &str) -> Vec<String> {
    let resp = app(pool)
        .await
        .oneshot(
            Request::builder()
                .uri("/settings/account")
                .header(header::COOKIE, format!("{cookie}; _sm_flash={flash}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "account page must render");
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter(|v| v.starts_with("_sm_flash="))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_detour_through_account_settings_keeps_another_pages_banner() {
    let Some((db, name)) = common::fresh_test_db("flash_carry").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "flasher").await;
    let cookie = session_cookie(&pool, user).await;

    // `restored` is the dashboard's to render. Landing here first must not be
    // what loses it.
    let set = account_page_flash(&pool, &cookie, "restored").await;
    assert!(
        set.iter().any(|c| c.contains("_sm_flash=restored")),
        "restored must be staged again, got {set:?}"
    );

    // Both at once: the page renders its own and hands the other on.
    let set = account_page_flash(&pool, &cookie, "restored,identity_linked:github").await;
    assert!(
        set.iter().any(|c| c.contains("_sm_flash=restored")),
        "got {set:?}"
    );
    assert!(
        !set.iter().any(|c| c.contains("identity_linked")),
        "a banner this page rendered must not fire twice: {set:?}"
    );

    // Its own alone: nothing to carry, so the cookie only gets cleared.
    let set = account_page_flash(&pool, &cookie, "identity_linked:github").await;
    assert!(
        set.iter().all(|c| c.contains("_sm_flash=;")
            || c.contains("_sm_flash=\"\"")
            || c.contains("Max-Age=0")),
        "expected only a clear, got {set:?}"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

// ── Removing a sign-in method over the API ───────────────────────────────────

async fn unlink_req(
    app: &axum::Router,
    cookie: Option<&str>,
    csrf: bool,
    path: &str,
) -> StatusCode {
    let mut req = Request::builder().method("DELETE").uri(path);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    if csrf {
        req = req.header("X-Requested-With", "uptimepage");
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn removing_a_method_needs_the_csrf_header_and_a_session() {
    let Some((db, name)) = common::fresh_test_db("unlink_guard").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "remover").await;
    let cookie = session_cookie(&pool, user).await;
    let app = app(&pool).await;
    let path = "/api/v1/me/sign-in-methods/github?provider_user_id=x";

    assert_eq!(
        unlink_req(&app, Some(&cookie), false, path).await,
        StatusCode::FORBIDDEN,
        "no CSRF header"
    );
    assert_eq!(
        unlink_req(&app, None, true, path).await,
        StatusCode::UNAUTHORIZED,
        "no session"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn removing_a_method_records_it_and_leaves_the_others() {
    let Some((db, name)) = common::fresh_test_db("unlink_api").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = uptimepage::auth::oauth_login::upsert_identity_and_signup_org(
        &pool,
        uptimepage::auth::OauthProvider::Github,
        &uptimepage::auth::oauth_login::RemoteIdentity {
            provider_user_id: "gh-keep".into(),
            provider_username: None,
            verified_email: Some("remove@example.test".into()),
            display_name: None,
        },
        uptimepage::security::Admission::Clear,
        uptimepage::auth::oauth_login::SignupOrg::Create,
    )
    .await
    .expect("signup");
    uptimepage::auth::oauth_login::link_identity_to_user(
        &pool,
        uptimepage::auth::OauthProvider::Gitlab,
        &uptimepage::auth::oauth_login::RemoteIdentity {
            provider_user_id: "gl-drop".into(),
            provider_username: None,
            verified_email: None,
            display_name: None,
        },
        owner.user_id,
    )
    .await
    .expect("link gitlab");

    let cookie = session_cookie(&pool, owner.user_id).await;
    let status = unlink_req(
        &app(&pool).await,
        Some(&cookie),
        true,
        "/api/v1/me/sign-in-methods/gitlab?provider_user_id=gl-drop",
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let left = uptimepage::storage::oauth_identities::list_for_user(&pool, owner.user_id)
        .await
        .expect("list");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].provider, "github", "only the named one goes");

    // The row outlives the identity, which is the point: a removal has to stay
    // answerable for after the credential itself is gone.
    let (provider, origin, has_ip): (String, String, bool) = sqlx::query_as(
        "SELECT provider, origin, ip_hash IS NOT NULL FROM credential_events \
          WHERE user_id = $1 AND action = 'unlinked'",
    )
    .bind(owner.user_id.0)
    .fetch_one(&pool)
    .await
    .expect("event recorded");
    assert_eq!(provider, "gitlab");
    assert_eq!(origin, "session");
    assert!(
        has_ip,
        "the address the removal came from is the first question"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn removing_a_method_that_was_never_there_is_a_404() {
    let Some((db, name)) = common::fresh_test_db("unlink_404").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = uptimepage::auth::oauth_login::upsert_identity_and_signup_org(
        &pool,
        uptimepage::auth::OauthProvider::Github,
        &uptimepage::auth::oauth_login::RemoteIdentity {
            provider_user_id: "gh-only".into(),
            provider_username: None,
            verified_email: Some("only@example.test".into()),
            display_name: None,
        },
        uptimepage::security::Admission::Clear,
        uptimepage::auth::oauth_login::SignupOrg::Create,
    )
    .await
    .expect("signup");
    let cookie = session_cookie(&pool, owner.user_id).await;

    // Not "you would lock yourself out" — that answer would be a lie about a
    // method the account never had.
    let status = unlink_req(
        &app(&pool).await,
        Some(&cookie),
        true,
        "/api/v1/me/sign-in-methods/google?provider_user_id=nope",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    pool.close().await;
    common::drop_test_db(&name).await;
}
