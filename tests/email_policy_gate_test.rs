//! Which surface refuses, which records, and who is left alone.
//!
//! `require_mx` is off throughout — the MX half is a live DNS query, covered in
//! `disposable_sources_live_test`. Needs `DATABASE_URL`; each test gets its own
//! throwaway database, so the corpus it installs is its own.

use crate::common;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::auth::magic_link;
use uptimepage::config::AppConfig;
use uptimepage::domain::{OrgId, UserId, generate_signup_slug};
use uptimepage::storage::orgs::create_signup_org_with_owner_in_tx;
use uuid::Uuid;

use crate::common::MIGRATOR;

const BURNER: &str = "ghost@mailinator.test";

/// A router whose live corpus lists `mailinator.test`, under `signup_policy`.
async fn app_with_corpus(
    tag: &str,
    signup_policy: &str,
) -> Option<(axum::Router, sqlx::PgPool, String)> {
    let (db, name) = common::fresh_test_db(tag).await?;
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let policy = signup_policy.to_string();
    let (app, _org, state): (_, _, AppState) =
        common::build_test_app_with_pg_state(pool.clone(), move |cfg: &mut AppConfig| {
            cfg.email_policy.enabled = true;
            cfg.email_policy.require_mx = false;
            cfg.email_policy.signup_policy = policy;
        })
        .await;
    state.email_policy.install(HashSet::from_iter([
        "mailinator.test".to_string(),
        "burner.test".to_string(),
    ]));
    Some((app, pool, name))
}

/// First Set-Cookie value for `name`, stripped of attributes.
fn cookie_value(resp: &axum::response::Response, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix(&prefix))
        .and_then(|rest| rest.split(';').next())
        .map(str::to_string)
}

/// GET the confirm page for the nonce, then POST the token back with it.
async fn redeem_response(app: &axum::Router, token: &str) -> axum::response::Response {
    let get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/auth/magic-link/verify?token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let Some(nonce) = cookie_value(&get, "_sm_ml_confirm") else {
        return get;
    };
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/magic-link/verify")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("_sm_ml_confirm={nonce}"))
                .body(Body::from(format!("token={token}&csrf={nonce}")))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn redeem(app: &axum::Router, token: &str) -> StatusCode {
    redeem_response(app, token).await.status()
}

async fn mint(pool: &sqlx::PgPool, email: &str) -> String {
    magic_link::create(
        pool,
        magic_link::NewMagicLink {
            email,
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .token
}

async fn email_risk(pool: &sqlx::PgPool, email: &str) -> Option<Option<String>> {
    sqlx::query_scalar::<_, Option<String>>("SELECT email_risk FROM users WHERE email = $1::citext")
        .bind(email)
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn flag_opens_the_account_and_records_why_it_was_suspect() {
    let Some((app, pool, name)) = app_with_corpus("epol_flag", "flag").await else {
        return;
    };
    let token = mint(&pool, BURNER).await;

    assert_eq!(redeem(&app, &token).await, StatusCode::SEE_OTHER);
    assert_eq!(
        email_risk(&pool, BURNER).await,
        Some(Some("disposable".to_string())),
        "the account opens, and carries the mark for later"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn block_refuses_and_leaves_no_row_behind() {
    let Some((app, pool, name)) = app_with_corpus("epol_block", "block").await else {
        return;
    };
    let token = mint(&pool, BURNER).await;

    let resp = redeem_response(&app, &token).await;
    // Opened from a mail client, so a refusal is a page — and the same page any
    // dead token gets, so it names no addresses.
    assert_eq!(resp.status(), StatusCode::GONE);
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/html"),
        "a browser must not be served a JSON error body, got {content_type}"
    );
    assert_eq!(
        email_risk(&pool, BURNER).await,
        None,
        "a refused signup must not half-create an account"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_account_that_predates_the_listing_still_signs_in() {
    let Some((app, pool, name)) = app_with_corpus("epol_existing", "block").await else {
        return;
    };
    // Joined before anyone listed the domain. `block` must not lock them out —
    // the corpus governs who may open an account, not who may come back.
    sqlx::query(
        "INSERT INTO users (email, terms_version, privacy_version, email_verified_at) \
         VALUES ($1, 'v1', 'v1', now())",
    )
    .bind(BURNER)
    .execute(&pool)
    .await
    .unwrap();
    let token = mint(&pool, BURNER).await;

    assert_eq!(redeem(&app, &token).await, StatusCode::SEE_OTHER);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_clean_address_is_untouched_by_any_of_this() {
    let Some((app, pool, name)) = app_with_corpus("epol_clean", "block").await else {
        return;
    };
    let token = mint(&pool, "real@example.test").await;

    assert_eq!(redeem(&app, &token).await, StatusCode::SEE_OTHER);
    assert_eq!(
        email_risk(&pool, "real@example.test").await,
        Some(None),
        "no mark on an address nothing was wrong with"
    );

    common::drop_test_db(&name).await;
}

async fn seed_owner(pool: &sqlx::PgPool, email: &str) -> (UserId, OrgId) {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version, email_verified_at) \
         VALUES ($1, 'v1', 'v1', now()) RETURNING id",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap();
    let owner = UserId(id);
    let mut tx = pool.begin().await.unwrap();
    let org = loop {
        let slug = generate_signup_slug();
        if let Some(o) = create_signup_org_with_owner_in_tx(&mut tx, owner, &slug, "T")
            .await
            .unwrap()
        {
            break o;
        }
    };
    tx.commit().await.unwrap();
    (owner, org)
}

/// `signup_policy` governs who may open an account, not where we will send
/// mail. `allow` is the strongest form of the claim: signup gate fully off, and
/// the invitation is still refused.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_invitation_to_a_burner_is_refused_even_under_allow() {
    let Some((db, name)) = common::fresh_test_db("epol_invite").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let (owner, org) = seed_owner(&pool, "owner@example.test").await;

    let (app, _provisioned, state): (_, _, AppState) =
        common::build_test_app_with_pg_state(pool.clone(), |cfg: &mut AppConfig| {
            cfg.email_policy.enabled = true;
            cfg.email_policy.require_mx = false;
            cfg.email_policy.signup_policy = "allow".into();
        })
        .await;
    state
        .email_policy
        .install(HashSet::from_iter(["mailinator.test".to_string()]));
    let app = common::with_session(app, owner, Some(org), None);

    let send = |email: &str| {
        let app = app.clone();
        let body = format!(r#"{{"email":"{email}","role":"member"}}"#);
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/orgs/{}/invitations", org.0))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };

    assert_eq!(
        send("newhire@mailinator.test").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(send("newhire@example.test").await, StatusCode::CREATED);

    common::drop_test_db(&name).await;
}

/// Invited while the domain was clean, redeemed after it was listed. The invite
/// path admits regardless of policy, but the mark still has to survive. `block`
/// is the case worth pinning: an operator strict enough to refuse these at
/// signup is the last one who should get the account with no record of why.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_invitation_redeemed_after_the_listing_opens_but_keeps_the_mark() {
    let Some((db, name)) = common::fresh_test_db("epol_invite_late").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let (owner, org) = seed_owner(&pool, "owner@example.test").await;

    let (app, _provisioned, state): (_, _, AppState) =
        common::build_test_app_with_pg_state(pool.clone(), |cfg: &mut AppConfig| {
            cfg.email_policy.enabled = true;
            cfg.email_policy.require_mx = false;
            cfg.email_policy.signup_policy = "block".into();
        })
        .await;
    let invited = common::with_session(app, owner, Some(org), None);

    // Corpus still empty, so the invitation goes out the way it would have
    // before anyone listed the domain.
    let sent = invited
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/orgs/{}/invitations", org.0))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"email":"{BURNER}","role":"member"}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sent.status(), StatusCode::CREATED);

    state
        .email_policy
        .install(HashSet::from_iter(["mailinator.test".to_string()]));

    let invitation_id: Uuid =
        sqlx::query_scalar("SELECT id FROM invitations WHERE email = $1::citext")
            .bind(BURNER)
            .fetch_one(&pool)
            .await
            .unwrap();
    let token = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: BURNER,
            expiry_minutes: 15,
            invitation_id: Some(invitation_id),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .token;

    // A fresh router: `with_session` above pinned the owner's cookie, and the
    // invitee arrives signed in as nobody.
    let (app, _org, state): (_, _, AppState) =
        common::build_test_app_with_pg_state(pool.clone(), |cfg: &mut AppConfig| {
            cfg.email_policy.enabled = true;
            cfg.email_policy.require_mx = false;
            cfg.email_policy.signup_policy = "block".into();
        })
        .await;
    state
        .email_policy
        .install(HashSet::from_iter(["mailinator.test".to_string()]));

    assert_eq!(redeem(&app, &token).await, StatusCode::SEE_OTHER);
    assert_eq!(
        email_risk(&pool, BURNER).await,
        Some(Some("disposable".to_string())),
        "admitted on the invitation, still marked as what it is"
    );

    common::drop_test_db(&name).await;
}

/// The gate ships off, so upgrading a self-hosted install changes nothing.
/// An install behind a VPN or serving an internal mail domain would otherwise
/// refuse its own addresses: the MX half resolves through the public servers
/// in `[dns] servers`, which cannot see them.
#[test]
fn the_feature_ships_off() {
    let cfg = AppConfig::load().expect("config");
    assert!(
        !cfg.email_policy.enabled,
        "email_policy must default to off; turning it on is an operator decision"
    );
}

/// Under `block` a listed domain is not mailed: `/verify` would refuse it, so
/// the link could never be redeemed. The send is skipped, not the row — timing
/// must not tell a listed domain from an unknown one.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn block_does_not_mail_a_link_it_would_refuse() {
    let Some((app, pool, name)) = app_with_corpus("epol_send_block", "block").await else {
        return;
    };

    let request = |email: &str| {
        let app = app.clone();
        let body = format!(r#"{{"email":"{email}"}}"#);
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/magic-link/request")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };
    let stamped = async |email: &str| -> bool {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT sent_at FROM magic_link_tokens WHERE email = $1::citext \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(email)
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten()
        .is_some()
    };

    // A clean address first, to establish how long the detached send task takes
    // and that it stamps at all.
    assert_eq!(request("real@example.test").await, StatusCode::OK);
    let mut sent = false;
    for _ in 0..40 {
        if stamped("real@example.test").await {
            sent = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(sent, "a clean address should have been mailed");

    // Same shape, listed domain. The row still exists — timing must not
    // distinguish it — but nothing is sent.
    assert_eq!(request(BURNER).await, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!stamped(BURNER).await, "a listed domain must not be mailed");
    let row_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM magic_link_tokens WHERE email = $1::citext)",
    )
    .bind(BURNER)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(row_exists, "the row is written either way, or timing leaks");

    common::drop_test_db(&name).await;
}

/// `flag` still mails a listed domain. Suppressing it would make `flag` behave
/// as `block`: no link, no redemption, no account, so no mark — which is the
/// one thing `flag` exists to produce.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn flag_still_mails_a_listed_domain() {
    let Some((app, pool, name)) = app_with_corpus("epol_send_flag", "flag").await else {
        return;
    };

    let status = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/magic-link/request")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(r#"{{"email":"{BURNER}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::OK);

    let mut sent = false;
    for _ in 0..40 {
        let stamped: Option<Option<chrono::DateTime<chrono::Utc>>> = sqlx::query_scalar(
            "SELECT sent_at FROM magic_link_tokens WHERE email = $1::citext \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(BURNER)
        .fetch_optional(&pool)
        .await
        .unwrap();
        if stamped.flatten().is_some() {
            sent = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        sent,
        "under flag the link must go out so the account can open"
    );

    common::drop_test_db(&name).await;
}

/// An address the org already verified keeps working after a list names its
/// domain. The channel is still delivering, and the pinned floor is
/// compile-time, so refusing the edit would strand the owner until a release.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_verified_channel_survives_its_domain_being_listed() {
    let Some((db, name)) = common::fresh_test_db("epol_grandfather").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let (owner, org) = seed_owner(&pool, "owner@example.test").await;

    let (app, _provisioned, state): (_, _, AppState) =
        common::build_test_app_with_pg_state(pool.clone(), |cfg: &mut AppConfig| {
            cfg.email_policy.enabled = true;
            cfg.email_policy.require_mx = false;
            cfg.email_policy.signup_policy = "flag".into();
        })
        .await;

    // Verified while the domain was clean.
    let channel = state
        .notification_channel_store
        .seed_owner_email(org, "alerts@mailinator.test", owner, 10)
        .await
        .unwrap()
        .expect("seeded channel");

    state
        .email_policy
        .install(HashSet::from_iter(["mailinator.test".to_string()]));
    let app = common::with_session(app, owner, Some(org), None);

    let patch = |body: String| {
        let app = app.clone();
        let uri = format!("/api/v1/notification-channels/{}", channel.id);
        async move {
            app.oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };

    let same = patch(
        r#"{"config":{"type":"email","to":"alerts@mailinator.test"},"name":"renamed"}"#.to_string(),
    )
    .await;
    assert_eq!(
        same,
        StatusCode::OK,
        "an address already verified here is grandfathered"
    );

    let moved =
        patch(r#"{"config":{"type":"email","to":"someone@mailinator.test"}}"#.to_string()).await;
    assert_eq!(
        moved,
        StatusCode::BAD_REQUEST,
        "same listed domain, address never verified: a new destination, fully gated"
    );

    common::drop_test_db(&name).await;
}
