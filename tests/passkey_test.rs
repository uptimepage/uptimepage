//! What a passkey has to get right against a real database.
//!
//! The unit tests cover the reachability arithmetic. These cover the parts
//! only Postgres can answer: that a challenge answers once, that the removal
//! guard and the account page agree, that a removed credential leaves a trail
//! behind it, and that a deleted account takes its credentials with it.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use uptimepage::domain::OauthProvider;
use uptimepage::domain::UserId;
use uptimepage::domain::WaysIn;
use uptimepage::storage::oauth_identities::RequestOrigin;
use uptimepage::storage::passkeys;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

const HOST: &str = "app.test";
const OTHER_HOST: &str = "moved.test";

fn ways_in(passkeys_on: bool) -> WaysIn {
    WaysIn {
        enabled_providers: Vec::new(),
        email_is_a_way_back: false,
        passkeys_open_the_account: passkeys_on,
    }
}

fn anon() -> RequestOrigin<'static> {
    RequestOrigin {
        ip_hash: None,
        user_agent_hash: None,
    }
}

/// `remove` never deserialises the stored credential, so a row is enough to
/// exercise the guard without an authenticator to mint a real one.
async fn add_credential(pool: &sqlx::PgPool, user: UserId, id: &str, rp_id: &str) -> uuid::Uuid {
    let (row,): (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO webauthn_credentials (user_id, credential_id, credential, rp_id) \
         VALUES ($1, $2, '{}'::jsonb, $3) RETURNING id",
    )
    .bind(user.0)
    .bind(id.as_bytes())
    .bind(rp_id)
    .fetch_one(pool)
    .await
    .expect("insert credential");
    row
}

async fn credential_events(pool: &sqlx::PgPool, user: UserId) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT action, provider FROM credential_events WHERE user_id = $1 ORDER BY occurred_at",
    )
    .bind(user.0)
    .fetch_all(pool)
    .await
    .expect("read credential events")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_last_way_in_cannot_be_removed() {
    let Some((db, name)) = common::fresh_test_db("passkey_last").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "solo").await;

    let only = add_credential(&pool, user, "cred-a", HOST).await;
    let err = passkeys::remove(&pool, user, only, Some(HOST), &ways_in(true), anon())
        .await
        .expect_err("the only credential must stay");
    assert!(
        format!("{err:?}").contains("LAST_SIGN_IN_METHOD"),
        "refused for the right reason, got {err:?}"
    );

    // A sibling makes the first one expendable, and only then.
    let second = add_credential(&pool, user, "cred-b", HOST).await;
    passkeys::remove(&pool, user, only, Some(HOST), &ways_in(true), anon())
        .await
        .expect("a sibling still opens the account");
    let err = passkeys::remove(&pool, user, second, Some(HOST), &ways_in(true), anon())
        .await
        .expect_err("now it is the last one again");
    assert!(format!("{err:?}").contains("LAST_SIGN_IN_METHOD"));

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires DATABASE_URL"]
async fn two_removals_at_once_cannot_empty_the_account() {
    let Some((db, name)) = common::fresh_test_db("passkey_race").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "raced").await;

    let a = add_credential(&pool, user, "cred-a", HOST).await;
    let b = add_credential(&pool, user, "cred-b", HOST).await;

    let ways = ways_in(true);
    let (ra, rb) = tokio::join!(
        passkeys::remove(&pool, user, a, Some(HOST), &ways, anon()),
        passkeys::remove(&pool, user, b, Some(HOST), &ways, anon()),
    );
    assert!(
        ra.is_ok() != rb.is_ok(),
        "exactly one may win, got {ra:?} and {rb:?}"
    );

    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webauthn_credentials WHERE user_id = $1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 1, "the account must keep a way in");

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires DATABASE_URL"]
async fn dropping_the_last_provider_and_the_last_passkey_at_once_leaves_one() {
    let Some((db, name)) = common::fresh_test_db("passkey_cross").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "both").await;

    let only = add_credential(&pool, user, "cred-a", HOST).await;
    sqlx::query(
        "INSERT INTO oauth_identities (user_id, provider, provider_user_id)          VALUES ($1, 'github', '900')",
    )
    .bind(user.0)
    .execute(&pool)
    .await
    .expect("link github");

    let ways = WaysIn {
        enabled_providers: vec![OauthProvider::Github],
        email_is_a_way_back: false,
        passkeys_open_the_account: true,
    };
    let (passkey_gone, provider_gone) = tokio::join!(
        passkeys::remove(&pool, user, only, Some(HOST), &ways, anon()),
        uptimepage::storage::oauth_identities::unlink(
            &pool,
            user,
            OauthProvider::Github,
            Some("900"),
            &ways,
            Some(HOST),
            Default::default(),
        ),
    );
    assert!(
        passkey_gone.is_ok() != provider_gone.is_ok(),
        "exactly one may win, got {passkey_gone:?} and {provider_gone:?}"
    );

    let credentials: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webauthn_credentials WHERE user_id = $1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    let identities: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oauth_identities WHERE user_id = $1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        credentials + identities,
        1,
        "the account must keep a way in"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_credential_bound_to_another_host_is_not_a_way_back() {
    let Some((db, name)) = common::fresh_test_db("passkey_rp").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "moved").await;

    let here = add_credential(&pool, user, "cred-here", HOST).await;
    add_credential(&pool, user, "cred-elsewhere", OTHER_HOST).await;

    // The stale row is a row, not a way in: counting it would let the account
    // drop the one credential that still answers on this host.
    let err = passkeys::remove(&pool, user, here, Some(HOST), &ways_in(true), anon())
        .await
        .expect_err("the orphaned credential must not hold the door open");
    assert!(
        format!("{err:?}").contains("LAST_SIGN_IN_METHOD"),
        "{err:?}"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_switched_off_deployment_counts_no_passkey() {
    let Some((db, name)) = common::fresh_test_db("passkey_off").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "off").await;

    add_credential(&pool, user, "cred-a", HOST).await;
    let doomed = add_credential(&pool, user, "cred-b", HOST).await;
    let err = passkeys::remove(&pool, user, doomed, Some(HOST), &ways_in(false), anon())
        .await
        .expect_err("none of them can sign in here");
    assert!(
        format!("{err:?}").contains("LAST_SIGN_IN_METHOD"),
        "{err:?}"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_account_cannot_hold_credentials_without_end() {
    let Some((db, name)) = common::fresh_test_db("passkey_cap").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "hoarder").await;

    for i in 0..passkeys::MAX_PER_USER {
        add_credential(&pool, user, &format!("cred-{i}"), HOST).await;
    }
    // A row written straight to the table skips the guard, so the check itself
    // is driven here, in a transaction of its own, exactly as `insert` does it.
    let mut tx = pool.begin().await.expect("begin");
    let err = passkeys::ensure_room(&mut tx, user, HOST)
        .await
        .expect_err("the cap holds");
    assert!(format!("{err:?}").contains("TOO_MANY_PASSKEYS"), "{err:?}");
    tx.rollback().await.ok();

    // A credential this host cannot use is not what the cap is for. Orphaning
    // one must not block the replacement the owner is being told to add.
    sqlx::query("UPDATE webauthn_credentials SET rp_id = $2 WHERE user_id = $1")
        .bind(user.0)
        .bind(OTHER_HOST)
        .execute(&pool)
        .await
        .expect("orphan them all");
    let mut tx = pool.begin().await.expect("begin");
    passkeys::ensure_room(&mut tx, user, HOST)
        .await
        .expect("dead rows hold no slot");
    tx.rollback().await.ok();
    sqlx::query("UPDATE webauthn_credentials SET rp_id = $2 WHERE user_id = $1")
        .bind(user.0)
        .bind(HOST)
        .execute(&pool)
        .await
        .expect("put them back");

    // One below the cap still has room.
    sqlx::query("DELETE FROM webauthn_credentials WHERE user_id = $1 AND credential_id = $2")
        .bind(user.0)
        .bind("cred-0".as_bytes())
        .execute(&pool)
        .await
        .expect("free a slot");
    let mut tx = pool.begin().await.expect("begin");
    passkeys::ensure_room(&mut tx, user, HOST)
        .await
        .expect("a freed slot is usable");
    tx.rollback().await.ok();

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn removing_a_credential_leaves_the_trail_behind() {
    let Some((db, name)) = common::fresh_test_db("passkey_trail").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "trail").await;

    let first = add_credential(&pool, user, "cred-a", HOST).await;
    add_credential(&pool, user, "cred-b", HOST).await;
    passkeys::remove(&pool, user, first, Some(HOST), &ways_in(true), anon())
        .await
        .expect("removable");

    // The row is gone; the record of it going is not. That is the whole reason
    // `credential_events` keeps the identifier itself.
    let events = credential_events(&pool, user).await;
    assert_eq!(
        events,
        vec![("unlinked".to_string(), "passkey".to_string())],
        "the removal is on the trail"
    );
    assert!(
        passkeys::list_for_user(&pool, user)
            .await
            .expect("list")
            .iter()
            .all(|row| row.id != first),
        "the credential itself is gone"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_credential_already_gone_is_not_an_error_page() {
    let Some((db, name)) = common::fresh_test_db("passkey_gone").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "twice").await;

    let doomed = add_credential(&pool, user, "cred-a", HOST).await;
    add_credential(&pool, user, "cred-b", HOST).await;
    passkeys::remove(&pool, user, doomed, Some(HOST), &ways_in(true), anon())
        .await
        .expect("the first press removes it");

    // A double-click races the first request past its own guard, and the answer
    // for a row that is not there is the same as for one that never was.
    let err = passkeys::remove(&pool, user, doomed, Some(HOST), &ways_in(true), anon())
        .await
        .expect_err("the second press finds nothing");
    assert!(format!("{err:?}").contains("PASSKEY_NOT_FOUND"), "{err:?}");

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_ceremony_answers_once_and_expiry_is_swept() {
    let Some((db, name)) = common::fresh_test_db("passkey_state").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "ceremony").await;

    let handle = passkeys::generate_handle();
    passkeys::put_state(&pool, &handle, Some(user), &"challenge")
        .await
        .expect("store");

    let first: Option<(Option<UserId>, String)> =
        passkeys::take_state(&pool, &handle).await.expect("take");
    let (owner, payload) = first.expect("the first answer is served");
    assert_eq!(owner, Some(user), "the state remembers who started it");
    assert_eq!(payload, "challenge");

    // Replaying it finds nothing, because the read deleted it.
    let second: Option<(Option<UserId>, String)> =
        passkeys::take_state(&pool, &handle).await.expect("take");
    assert!(second.is_none(), "a challenge answers once");

    // A login ceremony carries no user, and an expired one is not served even
    // before the sweep reaches it.
    let live = passkeys::generate_handle();
    passkeys::put_state(&pool, &live, None, &"live")
        .await
        .expect("store live");
    let stale = passkeys::generate_handle();
    passkeys::put_state(&pool, &stale, None, &"stale")
        .await
        .expect("store stale");
    sqlx::query(
        "UPDATE webauthn_states SET expires_at = now() - interval '1 minute' WHERE state_hash = $1",
    )
    .bind(uptimepage::security::sha256_hex(&stale))
    .execute(&pool)
    .await
    .expect("age it");

    let expired: Option<(Option<UserId>, String)> =
        passkeys::take_state(&pool, &stale).await.expect("take");
    assert!(expired.is_none(), "an expired challenge is not answerable");

    let swept = passkeys::purge_expired(&pool).await.expect("purge");
    assert_eq!(swept, 1, "only the expired row goes");
    let remaining: Option<(Option<UserId>, String)> =
        passkeys::take_state(&pool, &live).await.expect("take");
    assert!(remaining.is_some(), "the live challenge survived the sweep");

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deleting_the_account_takes_its_credentials_with_it() {
    let Some((db, name)) = common::fresh_test_db("passkey_cascade").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "gone").await;

    add_credential(&pool, user, "cred-a", HOST).await;
    let handle = passkeys::generate_handle();
    passkeys::put_state(&pool, &handle, Some(user), &"challenge")
        .await
        .expect("store");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.0)
        .execute(&pool)
        .await
        .expect("hard delete");

    let creds: (i64,) = sqlx::query_as("SELECT count(*) FROM webauthn_credentials")
        .fetch_one(&pool)
        .await
        .expect("count credentials");
    let states: (i64,) = sqlx::query_as("SELECT count(*) FROM webauthn_states")
        .fetch_one(&pool)
        .await
        .expect("count states");
    assert_eq!(creds.0, 0, "credentials cascade with the account");
    assert_eq!(states.0, 0, "so does a ceremony left in flight");

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn removing_a_credential_takes_the_sessions_it_opened_with_it() {
    let Some((db, name)) = common::fresh_test_db("passkey_revoke").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    let user = common::make_user(&pool, "revoked").await;

    // Two, so the guard lets one go.
    let doomed = add_credential(&pool, user, "cred-a", "localhost").await;
    add_credential(&pool, user, "cred-b", "localhost").await;
    // The caller's own session, and two the removed credential could have
    // opened on other devices.
    common::seed_session(&pool, "keep-me", user, None).await;
    common::seed_session(&pool, "other-1", user, None).await;
    common::seed_session(&pool, "other-2", user, None).await;

    let (router, _) = common::build_test_app_with_pg(pool.clone(), |_cfg| {}).await;
    let router = common::with_session(router, user, None, Some("keep-me"));
    let res = router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/me/passkeys/{doomed}"))
                .header("X-Requested-With", "uptimepage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("request");
    assert_eq!(
        res.status(),
        StatusCode::NO_CONTENT,
        "the passkey is removed"
    );

    // A session the credential opened would outlive it by its absolute
    // timeout, which is what the confirm dialog promises does not happen.
    let surviving: Vec<(String,)> =
        sqlx::query_as("SELECT id_hash FROM sessions WHERE user_id = $1 ORDER BY id_hash")
            .bind(user.0)
            .fetch_all(&pool)
            .await
            .expect("read sessions");
    assert_eq!(
        surviving,
        vec![("keep-me".to_string(),)],
        "everything but the caller's own session is gone"
    );

    pool.close().await;
    common::drop_test_db(&name).await;
}
