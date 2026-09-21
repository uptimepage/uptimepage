//! Browser invitation flows: GET landing pages + post-login auto-accept via
//! the magic-link verify path (the OAuth callback shares the same
//! `try_auto_accept` core).
//!
//! Run via:
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo test --test invitation_browser_flows_test -- --ignored

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;
use uptimepage::app::AppState;
use uptimepage::auth::{invitations, magic_link};
use uptimepage::config::EmailProvider;
use uptimepage::domain::{ChannelKind, OrgId, Role, UserId, generate_signup_slug};
use uptimepage::storage::orgs::create_signup_org_with_owner_in_tx;
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

async fn fresh_pg() -> Option<(String, String)> {
    common::fresh_test_db("inv_flows").await
}

async fn open_pool(db_url: &str) -> sqlx::PgPool {
    common::open_test_pool(db_url).await
}

async fn seed_user(pool: &sqlx::PgPool, email: &str) -> UserId {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version, email_verified_at) \
         VALUES ($1, 'v1', 'v1', now()) RETURNING id",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap();
    UserId(id)
}

async fn seed_org(pool: &sqlx::PgPool, owner: UserId) -> OrgId {
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
    org
}

async fn org_slug(pool: &sqlx::PgPool, org: OrgId) -> String {
    sqlx::query_scalar("SELECT slug::text FROM organizations WHERE id = $1")
        .bind(org.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn invite(
    pool: &sqlx::PgPool,
    org: OrgId,
    inviter: UserId,
    email: &str,
) -> invitations::CreatedInvitation {
    invitations::create(pool, org, inviter, email, Role::Member, 168, 50)
        .await
        .unwrap()
}

async fn invitation_pending(pool: &sqlx::PgPool, id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT accepted_at IS NULL AND declined_at IS NULL FROM invitations WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Seeded alert channels: kind, address, verified or not.
async fn seeded_channels(state: &AppState, org: OrgId) -> Vec<(ChannelKind, String, bool)> {
    state
        .notification_channel_store
        .list(org)
        .await
        .unwrap()
        .into_iter()
        .map(|ch| (ch.kind, ch.name, ch.verified_at.is_some()))
        .collect()
}

/// The role the address holds and the org its signup stamped, if any.
async fn role_and_signup_org(pool: &sqlx::PgPool, email: &str) -> (String, Option<Uuid>) {
    sqlx::query_as(
        "SELECT m.role, u.signup_org_id FROM users u \
         JOIN memberships m ON m.user_id = u.id \
         WHERE u.email = $1::citext",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .expect("the account exists")
}

async fn active_org_of_latest_session(pool: &sqlx::PgPool, user: UserId) -> Option<Uuid> {
    sqlx::query_scalar(
        "SELECT active_org_id FROM sessions WHERE user_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn membership_count(pool: &sqlx::PgPool, org: OrgId, user: UserId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM memberships WHERE org_id = $1 AND user_id = $2")
        .bind(org.0)
        .bind(user.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Option<String>) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (status, location)
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

/// Drive the two-step verify: GET the confirmation page (read-only, must not
/// consume the token) then POST it back with the double-submit nonce the GET
/// set. Returns (GET status, POST status, POST Location).
async fn magic_verify(app: &axum::Router, token: &str) -> (StatusCode, StatusCode, Option<String>) {
    let get_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/auth/magic-link/verify?token={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let get_status = get_resp.status();
    let Some(nonce) = cookie_value(&get_resp, "_sm_ml_confirm") else {
        // No nonce issued (invalid token); the GET status is the outcome.
        return (get_status, get_status, None);
    };
    let post_resp = app
        .clone()
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
        .unwrap();
    let post_status = post_resp.status();
    let location = post_resp
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (get_status, post_status, location)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn get_accept_without_session_bounces_to_login_without_mutating() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own1@example.test").await;
    let org = seed_org(&pool, owner).await;
    let created = invite(&pool, org, owner, "new1@example.test").await;

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let (status, location) = get(
        &app,
        &format!("/invitations/accept?token={}", created.token),
    )
    .await;
    assert!(status.is_redirection(), "expected redirect, got {status}");
    let loc = location.expect("Location header");
    assert!(loc.starts_with("/login?invitation="), "got {loc}");
    assert!(invitation_pending(&pool, created.row.id).await);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn get_accept_with_session_joins_and_redirects() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own2@example.test").await;
    let org = seed_org(&pool, owner).await;
    let slug = org_slug(&pool, org).await;
    let invitee = seed_user(&pool, "new2@example.test").await;
    let their_org = seed_org(&pool, invitee).await;
    let created = invite(&pool, org, owner, "new2@example.test").await;

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let app = common::with_session(app, invitee, Some(their_org), None);
    let (status, location) = get(
        &app,
        &format!("/invitations/accept?token={}", created.token),
    )
    .await;
    assert!(status.is_redirection(), "expected redirect, got {status}");
    assert_eq!(
        location.as_deref(),
        Some(format!("/?joined={slug}").as_str())
    );
    assert!(!invitation_pending(&pool, created.row.id).await);
    assert_eq!(membership_count(&pool, org, invitee).await, 1);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn get_accept_email_mismatch_is_403_and_keeps_pending() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own3@example.test").await;
    let org = seed_org(&pool, owner).await;
    let other = seed_user(&pool, "other3@example.test").await;
    let other_org = seed_org(&pool, other).await;
    let created = invite(&pool, org, owner, "invited3@example.test").await;

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let app = common::with_session(app, other, Some(other_org), None);
    let (status, _) = get(
        &app,
        &format!("/invitations/accept?token={}", created.token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(invitation_pending(&pool, created.row.id).await);
    assert_eq!(membership_count(&pool, org, other).await, 0);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn get_decline_renders_confirm_page_without_mutating() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own4@example.test").await;
    let org = seed_org(&pool, owner).await;
    let created = invite(&pool, org, owner, "new4@example.test").await;

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let (status, _) = get(
        &app,
        &format!("/invitations/decline?token={}", created.token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The scanner-prefetch pin: rendering the page must not settle the row.
    assert!(invitation_pending(&pool, created.row.id).await);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_existing_user_auto_accepts_and_lands_in_org() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own5@example.test").await;
    let org = seed_org(&pool, owner).await;
    let slug = org_slug(&pool, org).await;
    let invitee = seed_user(&pool, "member5@example.test").await;
    let their_org = seed_org(&pool, invitee).await;
    let created = invite(&pool, org, owner, "member5@example.test").await;

    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "member5@example.test",
            expiry_minutes: 15,
            invitation_id: Some(created.row.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (app, _default, state) = common::build_test_app_with_pg_state(pool.clone(), |cfg| {
        cfg.email.provider = EmailProvider::Memory;
    })
    .await;
    let (get_status, status, location) = magic_verify(&app, &minted.token).await;
    assert_eq!(
        get_status,
        StatusCode::OK,
        "confirm page must render for a live token"
    );
    assert!(status.is_redirection(), "expected redirect, got {status}");
    assert_eq!(
        location.as_deref(),
        Some(format!("/?joined={slug}").as_str())
    );
    assert!(
        seeded_channels(&state, org).await.is_empty(),
        "a joined org is the inviter's to route; the invitee's address is not seeded into it"
    );
    assert!(
        seeded_channels(&state, their_org).await.is_empty(),
        "an org held from before is not opened by this sign-in"
    );
    assert_eq!(membership_count(&pool, org, invitee).await, 1);
    // Session row carries the joined org as active.
    let active: Option<Uuid> = sqlx::query_scalar(
        "SELECT active_org_id FROM sessions WHERE user_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(invitee.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, Some(org.0));

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_bootstraps_invited_unknown_email() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own6@example.test").await;
    let org = seed_org(&pool, owner).await;
    let slug = org_slug(&pool, org).await;
    let created = invite(&pool, org, owner, "fresh6@example.test").await;

    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "fresh6@example.test",
            expiry_minutes: 15,
            invitation_id: Some(created.row.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (app, _default, state) = common::build_test_app_with_pg_state(pool.clone(), |cfg| {
        cfg.email.provider = EmailProvider::Memory;
    })
    .await;
    let (get_status, status, location) = magic_verify(&app, &minted.token).await;
    assert_eq!(
        get_status,
        StatusCode::OK,
        "confirm page must render for a live token"
    );
    assert!(status.is_redirection(), "expected redirect, got {status}");
    assert_eq!(
        location.as_deref(),
        Some(format!("/?joined={slug}").as_str())
    );
    assert!(
        seeded_channels(&state, org).await.is_empty(),
        "a signup that joined an org opened none, so nothing is seeded"
    );

    let (user_id, verified, signup_org): (Uuid, bool, Option<Uuid>) = sqlx::query_as(
        "SELECT id, email_verified_at IS NOT NULL, signup_org_id \
         FROM users WHERE email = 'fresh6@example.test'::citext",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(verified);
    assert_eq!(signup_org, None, "invited bootstrap gets no personal org");
    assert_eq!(membership_count(&pool, org, UserId(user_id)).await, 1);
    let org_count: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(org_count, 1, "exactly one org: the inviter's");

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_unknown_email_without_invitation_opens_an_account() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "ghost7@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (app, _default, state) = common::build_test_app_with_pg_state(pool.clone(), |cfg| {
        cfg.email.provider = EmailProvider::Memory;
    })
    .await;
    let (get_status, status, location) = magic_verify(&app, &minted.token).await;
    assert_eq!(
        get_status,
        StatusCode::OK,
        "confirm page must render for a live token"
    );
    assert_eq!(status, StatusCode::SEE_OTHER, "redeemed, got {status}");
    assert_eq!(location.as_deref(), Some("/"), "and lands in the app");

    let (role, signup_org) = role_and_signup_org(&pool, "ghost7@example.test").await;
    assert_eq!(role, "owner", "in an org of their own, not somebody else's");
    let signup_org = OrgId(signup_org.expect("the session has somewhere to open"));
    assert_eq!(
        seeded_channels(&state, signup_org).await,
        vec![(ChannelKind::Email, "ghost7@example.test".to_string(), true)],
        "the claimed link proved the inbox, so the new org alerts it from the first monitor"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_bootstrap_email_mismatch_is_410_no_user() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own8@example.test").await;
    let org = seed_org(&pool, owner).await;
    let created = invite(&pool, org, owner, "invited8@example.test").await;

    // Token minted for a DIFFERENT address than the invitation's.
    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "stranger8@example.test",
            expiry_minutes: 15,
            invitation_id: Some(created.row.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let (get_status, status, _) = magic_verify(&app, &minted.token).await;
    assert_eq!(
        get_status,
        StatusCode::OK,
        "confirm page must render for a live token"
    );
    assert_eq!(status, StatusCode::GONE);
    let users: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE email = 'stranger8@example.test'::citext",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(users, 0);
    assert!(invitation_pending(&pool, created.row.id).await);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_plain_login_resolves_active_org() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = seed_user(&pool, "plain9@example.test").await;
    let org = seed_org(&pool, user).await;

    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "plain9@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (app, _default, state) = common::build_test_app_with_pg_state(pool.clone(), |cfg| {
        cfg.email.provider = EmailProvider::Memory;
    })
    .await;
    let (get_status, status, _) = magic_verify(&app, &minted.token).await;
    assert_eq!(
        get_status,
        StatusCode::OK,
        "confirm page must render for a live token"
    );
    assert!(status.is_redirection());
    // Regression pin: magic sessions used to be minted with NULL active_org,
    // which CurrentOrg rejects.
    assert_eq!(active_org_of_latest_session(&pool, user).await, Some(org.0));
    assert!(
        seeded_channels(&state, org).await.is_empty(),
        "a returning sign-in opens nothing, so it seeds nothing"
    );

    common::drop_test_db(&name).await;
}

/// An account whose last membership went away signs in and gets a personal
/// org, with its alert channel, the same as a signup would.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_orgless_user_opens_a_personal_org_with_its_alert_channel() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = seed_user(&pool, "orphan11@example.test").await;

    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "orphan11@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (app, _default, state) = common::build_test_app_with_pg_state(pool.clone(), |cfg| {
        cfg.email.provider = EmailProvider::Memory;
    })
    .await;
    let (_, status, _) = magic_verify(&app, &minted.token).await;
    assert!(status.is_redirection());

    let (role, _) = role_and_signup_org(&pool, "orphan11@example.test").await;
    assert_eq!(role, "owner", "an org of their own");
    let org = OrgId(
        active_org_of_latest_session(&pool, user)
            .await
            .expect("opens in it"),
    );
    assert_eq!(
        seeded_channels(&state, org).await,
        vec![(
            ChannelKind::Email,
            "orphan11@example.test".to_string(),
            true
        )]
    );

    common::drop_test_db(&name).await;
}

/// The org a sign-in opens in, pinned at the store: an account inside its
/// deletion grace window is never handed a new org.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn session_org_never_opens_one_for_a_pending_deletion() {
    use uptimepage::storage::users::{SessionOrg, session_org};

    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let orphan = seed_user(&pool, "gone12@example.test").await;
    let holder = seed_user(&pool, "held12@example.test").await;
    let held = seed_org(&pool, holder).await;

    assert_eq!(
        session_org(&pool, orphan, true).await.unwrap(),
        SessionOrg::Absent
    );
    let orgs: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships WHERE user_id = $1")
        .bind(orphan.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orgs, 0, "and nothing was opened on the way");
    assert_eq!(
        session_org(&pool, holder, true).await.unwrap(),
        SessionOrg::Held(held)
    );

    match session_org(&pool, orphan, false).await.unwrap() {
        SessionOrg::Opened(_) => {}
        other => panic!("a live account holding nothing opens one: {other:?}"),
    }

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn get_accept_quota_full_shows_page_and_keeps_token() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own10@example.test").await;
    let org = seed_org(&pool, owner).await;
    let invitee = seed_user(&pool, "new10@example.test").await;
    let their_org = seed_org(&pool, invitee).await;
    let created = invite(&pool, org, owner, "new10@example.test").await;
    // Owner occupies the only seat.
    sqlx::query("UPDATE plans SET max_members = 1")
        .execute(&pool)
        .await
        .unwrap();

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let app = common::with_session(app, invitee, Some(their_org), None);
    let (status, _) = get(
        &app,
        &format!("/invitations/accept?token={}", created.token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // The page promises the token survives a full org — pin it.
    assert!(invitation_pending(&pool, created.row.id).await);
    assert_eq!(membership_count(&pool, org, invitee).await, 0);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn b2_full_org_creates_no_user_and_keeps_invitation() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own11@example.test").await;
    let org = seed_org(&pool, owner).await;
    let created = invite(&pool, org, owner, "fresh11@example.test").await;
    sqlx::query("UPDATE plans SET max_members = 1")
        .execute(&pool)
        .await
        .unwrap();

    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "fresh11@example.test",
            expiry_minutes: 15,
            invitation_id: Some(created.row.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let (get_status, status, _) = magic_verify(&app, &minted.token).await;
    assert_eq!(
        get_status,
        StatusCode::OK,
        "confirm page must render for a live token"
    );
    assert_eq!(status, StatusCode::GONE);
    let users: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM users WHERE email = 'fresh11@example.test'::citext",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(users, 0, "cap pre-flight must run before the user INSERT");
    assert!(invitation_pending(&pool, created.row.id).await);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn get_accept_rotates_session_active_org() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let owner = seed_user(&pool, "own12@example.test").await;
    let org = seed_org(&pool, owner).await;
    let invitee = seed_user(&pool, "new12@example.test").await;
    let their_org = seed_org(&pool, invitee).await;
    let created = invite(&pool, org, owner, "new12@example.test").await;

    // Real session row so the landing handler has something to rotate.
    let session_cfg = uptimepage::config::SessionConfig::default();
    let real = uptimepage::auth::session::create(
        &pool,
        &session_cfg,
        invitee,
        Some(their_org),
        None,
        None,
    )
    .await
    .unwrap();
    let id_hash = real.row.id.clone();

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;
    let app = common::with_session(app, invitee, Some(their_org), Some(&id_hash));
    let (status, _) = get(
        &app,
        &format!("/invitations/accept?token={}", created.token),
    )
    .await;
    assert!(status.is_redirection());
    let active: Option<Uuid> =
        sqlx::query_scalar("SELECT active_org_id FROM sessions WHERE id_hash = $1")
            .bind(&id_hash)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active, Some(org.0), "session must open in the joined org");

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_get_prefetch_does_not_consume_then_post_signs_in() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = seed_user(&pool, "scan13@example.test").await;
    let _org = seed_org(&pool, user).await;
    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "scan13@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;

    // A mail link-scanner prefetches the URL: the GET renders the confirm page
    // but must NOT burn the single-use token (issue #92).
    let get_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/auth/magic-link/verify?token={}", minted.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(get_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("scan13@example.test"),
        "confirm page must name the account being signed into"
    );
    let unused: bool =
        sqlx::query_scalar("SELECT used_at IS NULL FROM magic_link_tokens WHERE id = $1")
            .bind(minted.row.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(unused, "GET prefetch consumed the token (#92 regression)");

    // The recipient then completes sign-in via the confirmation POST.
    let (_get, post, _loc) = magic_verify(&app, &minted.token).await;
    assert!(
        post.is_redirection(),
        "human POST should sign in, got {post}"
    );

    // The link is now spent: a fresh confirm-page GET peeks nothing and 410s.
    let (dead_get, _p, _l) = magic_verify(&app, &minted.token).await;
    assert_eq!(
        dead_get,
        StatusCode::GONE,
        "a spent token's confirm page must 410"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_verify_post_with_missing_or_mismatched_nonce_is_forbidden() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = seed_user(&pool, "csrf14@example.test").await;
    let _org = seed_org(&pool, user).await;
    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "csrf14@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |_| {}).await;

    let forged_post = |cookie: Option<&str>, csrf: &str| {
        let mut req = Request::builder()
            .method("POST")
            .uri("/auth/magic-link/verify")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, format!("_sm_ml_confirm={c}"));
        }
        req.body(Body::from(format!("token={}&csrf={csrf}", minted.token)))
            .unwrap()
    };
    let unused = || async {
        sqlx::query_scalar::<_, bool>("SELECT used_at IS NULL FROM magic_link_tokens WHERE id = $1")
            .bind(minted.row.id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };

    // No confirmation nonce at all: the double-submit fails closed.
    let resp = app.clone().oneshot(forged_post(None, "")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(unused().await, "forged POST must not consume the token");

    // Cookie present but its value differs from the posted field.
    let resp = app
        .clone()
        .oneshot(forged_post(Some("aaaaaaaa"), "bbbbbbbb"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        unused().await,
        "mismatched nonce must not consume the token"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_invite_only_deployment_turns_a_stranger_away() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = uptimepage::auth::magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "nope@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("mint a link");

    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |cfg| {
        cfg.auth.open_signup = false;
    })
    .await;
    let (_, post, _) = magic_verify(&app, &created.token).await;
    // The answer an unknown address has always got, so closing signup does
    // not become a way to ask whether an account exists.
    assert_eq!(post, StatusCode::GONE, "refused, got {post}");

    let (users,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM users WHERE email = 'nope@example.test'::citext")
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(users, 0, "and nothing was created");

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_invitation_outranks_the_signup_policy() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let owner = seed_user(&pool, "host@example.test").await;
    let org = seed_org(&pool, owner).await;
    let slug = org_slug(&pool, org).await;
    let invited = invite(&pool, org, owner, "guest@example.test").await;
    let minted = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "guest@example.test",
            expiry_minutes: 15,
            invitation_id: Some(invited.row.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Closing signup stops strangers, not people an owner asked for by name.
    let (app, _default) = common::build_test_app_with_pg(pool.clone(), |cfg| {
        cfg.auth.open_signup = false;
    })
    .await;
    let (_, status, location) = magic_verify(&app, &minted.token).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "redeemed, got {status}");
    assert_eq!(
        location.as_deref(),
        Some(format!("/?joined={slug}").as_str()),
        "and lands in the org that invited them"
    );

    let (role, signup_org) = role_and_signup_org(&pool, "guest@example.test").await;
    assert_eq!(role, "member", "joined, not founded");
    assert!(signup_org.is_none(), "and founded nothing of their own");

    common::drop_test_db(&name).await;
}
