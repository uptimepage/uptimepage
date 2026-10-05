//! Live-PG tests for Phase 8 magic-link primitives.
//!
//! Run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo nextest run --test it --run-ignored only -E 'test(/^auth_magic_link_test::/)'

use crate::common;

use uptimepage::auth::magic_link;
use uptimepage::storage::orgs as orgs_store;
use uuid::Uuid;

use crate::common::MIGRATOR;

async fn fresh_pg() -> Option<(String, String)> {
    common::fresh_test_db("auth_ml").await
}

async fn drop_pg(test_db: &str) {
    common::drop_test_db(test_db).await;
}

async fn open_pool(db_url: &str) -> sqlx::PgPool {
    common::open_test_pool(db_url).await
}

/// Setup shorthand: put a row in the state a delivered mail leaves behind.
async fn mark_sent(pool: &sqlx::PgPool, id: Uuid) {
    sqlx::query("UPDATE magic_link_tokens SET sent_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("mark sent");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn create_then_consume_roundtrip() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "alice@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("create");

    let consumed = magic_link::consume(&pool, &created.token)
        .await
        .expect("consume")
        .expect("row");
    assert_eq!(consumed.email, "alice@example.test");
    assert_eq!(consumed.id, created.row.id);

    // Second consume of the same token is rejected — single-use.
    assert!(
        magic_link::consume(&pool, &created.token)
            .await
            .expect("consume2")
            .is_none(),
        "token must be single-use"
    );

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn peek_is_read_only_and_does_not_consume() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "peek@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("create");

    // Peek returns the row but must leave the token spendable.
    let peeked = magic_link::peek(&pool, &created.token)
        .await
        .expect("peek")
        .expect("row");
    assert_eq!(peeked.id, created.row.id);
    let unused: bool =
        sqlx::query_scalar("SELECT used_at IS NULL FROM magic_link_tokens WHERE id = $1")
            .bind(created.row.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(unused, "peek must not mark the token used");

    // A second peek still succeeds, and consume then still redeems once.
    assert!(
        magic_link::peek(&pool, &created.token)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        magic_link::consume(&pool, &created.token)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        magic_link::consume(&pool, &created.token)
            .await
            .unwrap()
            .is_none(),
        "consume after peek is still single-use"
    );

    // Peek of a now-spent token, and of an unknown token, both return None.
    assert!(
        magic_link::peek(&pool, &created.token)
            .await
            .unwrap()
            .is_none(),
        "peek of a spent token is None"
    );
    assert!(
        magic_link::peek(&pool, "this-token-was-never-issued")
            .await
            .unwrap()
            .is_none()
    );

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn consume_with_unknown_token_returns_none() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    // Seed a real row so the prefix-narrowed SELECT has something to compare
    // against — otherwise an empty table never enters the argon2-verify loop
    // and the test couldn't catch a verify-mismatch regression.
    let _real = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "real@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("seed");

    let outcome = magic_link::consume(&pool, "this-token-was-never-issued")
        .await
        .expect("consume");
    assert!(outcome.is_none());

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn consume_skips_expired_rows() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "bob@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("create");

    // Force-expire the row.
    sqlx::query(
        "UPDATE magic_link_tokens SET expires_at = now() - INTERVAL '1 minute' WHERE id = $1",
    )
    .bind(created.row.id)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        magic_link::consume(&pool, &created.token)
            .await
            .expect("consume")
            .is_none(),
        "expired token must not redeem"
    );

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn purge_old_collects_rows_once_they_expire() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    // Row 1: expired past the retention window.
    let r1 = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "x1@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("create");
    sqlx::query(
        "UPDATE magic_link_tokens SET expires_at = now() - INTERVAL '8 days' WHERE id = $1",
    )
    .bind(r1.row.id)
    .execute(&pool)
    .await
    .unwrap();

    let r2 = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "x2@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("create");
    magic_link::consume(&pool, &r2.token).await.unwrap();
    // Redeemed and expired, but inside the window: the trail of who opened it
    // is what a takeover review reads.
    sqlx::query(
        "UPDATE magic_link_tokens SET expires_at = now() - INTERVAL '1 minute' WHERE id = $1",
    )
    .bind(r2.row.id)
    .execute(&pool)
    .await
    .unwrap();

    // Row 3: fresh, unused — must survive.
    let r3 = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "x3@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("create");

    let removed = magic_link::purge_old(&pool).await.unwrap();
    assert_eq!(removed, 1);

    let survivors: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM magic_link_tokens ORDER BY created_at")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(survivors, vec![r2.row.id, r3.row.id]);

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_throttle_counts_what_was_delivered_not_what_was_inserted() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    // Every request inserts, so inserts alone must not throttle anything.
    let first = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "throttle@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("first create");
    let second = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "throttle@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("second create");
    assert!(
        magic_link::claim_send(&pool, first.row.id, "throttle@example.test", 60)
            .await
            .expect("first claim"),
        "an empty window is the first sender's to take"
    );
    assert!(
        !magic_link::claim_send(&pool, second.row.id, "throttle@example.test", 60)
            .await
            .expect("second claim"),
        "and the one behind it finds the window held"
    );

    let other = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "other@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("other create");
    assert!(
        magic_link::claim_send(&pool, other.row.id, "other@example.test", 60)
            .await
            .expect("other"),
        "the window is per address"
    );

    sqlx::query(
        "UPDATE magic_link_tokens SET sent_at = now() - INTERVAL '2 minutes' WHERE id = $1",
    )
    .bind(first.row.id)
    .execute(&pool)
    .await
    .unwrap();
    let third = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "throttle@example.test",
            expiry_minutes: 15,
            ..Default::default()
        },
    )
    .await
    .expect("third create");
    assert!(
        magic_link::claim_send(&pool, third.row.id, "throttle@example.test", 60)
            .await
            .expect("past window"),
        "once the window lapses the next request sends"
    );

    // Throttle off still stamps, or the code in that mail is unredeemable.
    assert!(
        magic_link::claim_send(&pool, second.row.id, "throttle@example.test", 0)
            .await
            .expect("disabled"),
        "a zero window suppresses nothing"
    );
    let stamped: bool =
        sqlx::query_scalar("SELECT sent_at IS NOT NULL FROM magic_link_tokens WHERE id = $1")
            .bind(second.row.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(stamped);

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn asking_again_does_not_starve_the_next_mail() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let sent = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "again@example.test",
            expiry_minutes: 15,
            nonce: Some("n"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    magic_link::claim_send(&pool, sent.row.id, "again@example.test", 60)
        .await
        .expect("send");
    sqlx::query(
        "UPDATE magic_link_tokens SET sent_at = now() - INTERVAL '90 seconds' WHERE id = $1",
    )
    .bind(sent.row.id)
    .execute(&pool)
    .await
    .unwrap();

    let mut latest = sent.row.id;
    for _ in 0..3 {
        latest = magic_link::create(
            &pool,
            magic_link::NewMagicLink {
                email: "again@example.test",
                expiry_minutes: 15,
                nonce: Some("n"),
                ..Default::default()
            },
        )
        .await
        .expect("resend")
        .row
        .id;
    }
    assert!(
        magic_link::claim_send(&pool, latest, "again@example.test", 60)
            .await
            .expect("claim"),
        "undelivered rows must never hold the window open"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_code_nobody_was_sent_is_not_redeemable() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "unsent@example.test",
            expiry_minutes: 15,
            nonce: Some("n"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    assert!(matches!(
        magic_link::consume_code(&pool, "n", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));

    mark_sent(&pool, created.row.id).await;
    assert!(matches!(
        magic_link::consume_code(&pool, "n", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_link_with_no_browser_behind_it_survives_a_stranger_asking() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let console = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "owner@example.test",
            expiry_minutes: 1440,
            ..Default::default()
        },
    )
    .await
    .expect("console link");
    let asked = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "owner@example.test",
            expiry_minutes: 15,
            nonce: Some("n"),
            ..Default::default()
        },
    )
    .await
    .expect("requested link");
    mark_sent(&pool, asked.row.id).await;
    assert_eq!(
        magic_link::supersede_others(&pool, "owner@example.test", asked.row.id)
            .await
            .expect("supersede"),
        0
    );
    assert!(
        magic_link::peek(&pool, &console.token)
            .await
            .unwrap()
            .is_some()
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn tombstone_lookup_and_undelete_restore_for_magic_link_verify() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ('Frank@Example.test', 'v1', 'v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed user");
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("soft-delete");

    // Invitation semantics stay active-only: tombstoned user is invisible.
    assert!(
        orgs_store::find_user_by_email(&pool, "frank@example.test")
            .await
            .expect("active lookup")
            .is_none()
    );

    // The verify path's lookup sees the tombstone; restoring is a later,
    // separate confirmation.
    let (found, deleted_at) =
        orgs_store::find_user_by_email_including_deleted(&pool, "frank@example.test")
            .await
            .expect("tombstone lookup")
            .expect("row");
    assert_eq!(found.0, user_id);
    assert!(deleted_at.is_some());

    uptimepage::auth::account::restore_account(&pool, found)
        .await
        .expect("restore")
        .expect("account was scheduled for deletion");

    assert_eq!(
        orgs_store::find_user_by_email(&pool, "frank@example.test")
            .await
            .expect("post-restore lookup")
            .map(|u| u.0),
        Some(user_id)
    );

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn tombstone_lookup_prefers_active_row_over_tombstone() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    // Same email twice: a tombstoned row (older) and an active row — the
    // partial unique index permits this pair. The lookup must never pick
    // the tombstone when an active account exists.
    sqlx::query(
        "INSERT INTO users (email, terms_version, privacy_version, deleted_at, created_at) \
         VALUES ('Gus@Example.test', 'v1', 'v1', now(), now() - INTERVAL '1 day')",
    )
    .execute(&pool)
    .await
    .expect("seed tombstone");
    let (active_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ('gus@example.test', 'v1', 'v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed active");

    let (found, deleted_at) =
        orgs_store::find_user_by_email_including_deleted(&pool, "gus@example.test")
            .await
            .expect("lookup")
            .expect("row");
    assert_eq!(found.0, active_id, "active row must win over tombstone");
    assert!(deleted_at.is_none());

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn redirect_and_invitation_round_trip_through_create_and_consume() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "hop@example.test",
            expiry_minutes: 15,
            redirect_after: Some("/targets/abc"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    let row = magic_link::consume(&pool, &created.token)
        .await
        .expect("consume")
        .expect("row");
    assert_eq!(row.redirect_after.as_deref(), Some("/targets/abc"));
    assert_eq!(row.invitation_id, None);

    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_stranger_opening_an_account_gets_an_org_of_their_own() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let (user, created) =
        uptimepage::storage::users::create_signup_user(&pool, "cold@example.test", None)
            .await
            .expect("signup");
    assert!(created, "a brand-new account");

    let (org_id, role): (Uuid, String) =
        sqlx::query_as("SELECT m.org_id, m.role FROM memberships m WHERE m.user_id = $1")
            .bind(user.0)
            .fetch_one(&pool)
            .await
            .expect("membership");
    assert_eq!(role, "owner", "their own org, not somebody else's");

    let (signup_org, verified): (Option<Uuid>, bool) = sqlx::query_as(
        "SELECT signup_org_id, email_verified_at IS NOT NULL FROM users WHERE id = $1",
    )
    .bind(user.0)
    .fetch_one(&pool)
    .await
    .expect("user row");
    assert_eq!(signup_org, Some(org_id), "the session opens in it");
    assert!(verified, "the link that got here is the proof");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_second_account_on_one_address_is_refused() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let (first, _) =
        uptimepage::storage::users::create_signup_user(&pool, "dup@example.test", None)
            .await
            .expect("first");
    // Two links racing resolve to one account, not an error and not a second org.
    let (second, created) =
        uptimepage::storage::users::create_signup_user(&pool, "dup@example.test", None)
            .await
            .expect("the loser resolves the winner");
    assert!(!created, "it created nothing");
    assert_eq!(second, first, "and hands back the same account");

    let (orgs,): (i64,) = sqlx::query_as("SELECT count(*) FROM organizations")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(orgs, 1, "so no second org exists");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_wrong_code_burns_itself_and_spares_the_link() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let nonce = "browser-that-asked";
    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "one@example.test",
            expiry_minutes: 15,
            nonce: Some(nonce),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, created.row.id).await;

    assert!(matches!(
        magic_link::consume_code(&pool, nonce, &wrong_code(&created.code))
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));
    // The one attempt is gone, even for the right code.
    assert!(matches!(
        magic_link::consume_code(&pool, nonce, &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));
    assert!(
        magic_link::consume(&pool, &created.token)
            .await
            .unwrap()
            .is_some(),
        "the link still redeems"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_paste_accident_does_not_spend_the_attempt() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let nonce = "asked-here";
    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "two@example.test",
            expiry_minutes: 15,
            nonce: Some(nonce),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, created.row.id).await;

    for malformed in ["", "  ", "ABC", "4KP9RT77"] {
        // Malformed, not Refused: the caller keeps the browser's binding.
        assert!(matches!(
            magic_link::consume_code(&pool, nonce, malformed)
                .await
                .unwrap(),
            magic_link::CodeOutcome::Malformed
        ));
    }
    let padded = format!("  {}  ", created.code);
    assert!(matches!(
        magic_link::consume_code(&pool, nonce, &padded)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_code_is_useless_in_another_browser() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "three@example.test",
            expiry_minutes: 15,
            nonce: Some("mine"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, created.row.id).await;
    assert!(matches!(
        magic_link::consume_code(&pool, "somebody-elses", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));
    assert!(matches!(
        magic_link::consume_code(&pool, "mine", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn only_the_newest_credential_is_live() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let first = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "four@example.test",
            expiry_minutes: 15,
            nonce: Some("n1"),
            ..Default::default()
        },
    )
    .await
    .expect("first");
    mark_sent(&pool, first.row.id).await;
    let second = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "four@example.test",
            expiry_minutes: 15,
            nonce: Some("n2"),
            ..Default::default()
        },
    )
    .await
    .expect("second");
    mark_sent(&pool, second.row.id).await;
    let retired = magic_link::supersede_others(&pool, "four@example.test", second.row.id)
        .await
        .expect("supersede");
    assert_eq!(retired, 1, "the earlier row went");

    assert!(
        magic_link::consume(&pool, &first.token)
            .await
            .unwrap()
            .is_none(),
        "the older link is dead"
    );
    assert!(matches!(
        magic_link::consume_code(&pool, "n1", &first.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));
    assert!(matches!(
        magic_link::consume_code(&pool, "n2", &second.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_trail_says_which_credential_opened_the_session() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let by_link = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "l@example.test",
            expiry_minutes: 15,
            nonce: Some("a"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, by_link.row.id).await;
    magic_link::consume(&pool, &by_link.token).await.unwrap();
    let by_code = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "c@example.test",
            expiry_minutes: 15,
            nonce: Some("b"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, by_code.row.id).await;
    magic_link::consume_code(&pool, "b", &by_code.code)
        .await
        .unwrap();

    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT email::text, redeemed_via FROM magic_link_tokens ORDER BY email")
            .fetch_all(&pool)
            .await
            .expect("read");
    assert_eq!(
        rows,
        vec![
            ("c@example.test".into(), Some("code".into())),
            ("l@example.test".into(), Some("link".into())),
        ]
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_superseded_row_does_not_look_redeemed() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let first = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "s@example.test",
            expiry_minutes: 15,
            nonce: Some("n1"),
            ..Default::default()
        },
    )
    .await
    .expect("first");
    mark_sent(&pool, first.row.id).await;
    let second = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "s@example.test",
            expiry_minutes: 15,
            nonce: Some("n2"),
            ..Default::default()
        },
    )
    .await
    .expect("second");
    mark_sent(&pool, second.row.id).await;
    magic_link::supersede_others(&pool, "s@example.test", second.row.id)
        .await
        .expect("supersede");

    let (used, superseded, via): (bool, bool, Option<String>) = sqlx::query_as(
        "SELECT used_at IS NOT NULL, superseded_at IS NOT NULL, redeemed_via \
         FROM magic_link_tokens WHERE id = $1",
    )
    .bind(first.row.id)
    .fetch_one(&pool)
    .await
    .expect("read");
    assert!(!used, "nobody redeemed it");
    assert!(superseded, "it was replaced");
    assert_eq!(via, None);

    pool.close().await;
    drop_pg(&name).await;
}

/// Well formed, so entering it spends the attempt.
fn wrong_code(right: &str) -> String {
    let mut c = right.to_string();
    c.replace_range(0..1, if right.starts_with('2') { "3" } else { "2" });
    c
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_expired_code_is_refused() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "old@example.test",
            expiry_minutes: 15,
            nonce: Some("n"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, created.row.id).await;
    sqlx::query(
        "UPDATE magic_link_tokens SET expires_at = now() - INTERVAL '1 minute' WHERE id = $1",
    )
    .bind(created.row.id)
    .execute(&pool)
    .await
    .unwrap();

    assert!(matches!(
        magic_link::consume_code(&pool, "n", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_code_opens_the_session_once() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "once@example.test",
            expiry_minutes: 15,
            nonce: Some("n"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, created.row.id).await;
    assert!(matches!(
        magic_link::consume_code(&pool, "n", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));
    assert!(matches!(
        magic_link::consume_code(&pool, "n", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_link_dies_with_the_code_that_beat_it() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "both@example.test",
            expiry_minutes: 15,
            nonce: Some("n"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, created.row.id).await;
    assert!(matches!(
        magic_link::consume_code(&pool, "n", &created.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));
    assert!(
        magic_link::consume(&pool, &created.token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        magic_link::peek(&pool, &created.token)
            .await
            .unwrap()
            .is_none()
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_wrong_guess_spends_only_the_browser_that_made_it() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let mine = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "a@example.test",
            expiry_minutes: 15,
            nonce: Some("mine"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, mine.row.id).await;
    let theirs = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "b@example.test",
            expiry_minutes: 15,
            nonce: Some("theirs"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, theirs.row.id).await;

    assert!(matches!(
        magic_link::consume_code(&pool, "mine", &wrong_code(&mine.code))
            .await
            .unwrap(),
        magic_link::CodeOutcome::Refused
    ));
    assert!(matches!(
        magic_link::consume_code(&pool, "theirs", &theirs.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn two_sends_that_raced_do_not_retire_each_other() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let older = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "race@example.test",
            expiry_minutes: 15,
            nonce: Some("a"),
            ..Default::default()
        },
    )
    .await
    .expect("older");
    let newer = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "race@example.test",
            expiry_minutes: 15,
            nonce: Some("b"),
            ..Default::default()
        },
    )
    .await
    .expect("newer");

    // Same created_at is real: it is transaction-start time, not statement time.
    sqlx::query(
        "UPDATE magic_link_tokens SET created_at = now() WHERE email = 'race@example.test'",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Both winners, each retiring what it found.
    magic_link::supersede_others(&pool, "race@example.test", newer.row.id)
        .await
        .expect("newer sweeps");
    magic_link::supersede_others(&pool, "race@example.test", older.row.id)
        .await
        .expect("older sweeps");

    let live: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM magic_link_tokens WHERE superseded_at IS NULL")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(live.len(), 1, "exactly one link must survive the tie");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_attempt_is_claimed_before_the_guess_is_judged() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let sent = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "a@example.test",
            expiry_minutes: 15,
            nonce: Some("mine"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, sent.row.id).await;

    assert!(matches!(
        magic_link::consume_code(&pool, "mine", &sent.code)
            .await
            .unwrap(),
        magic_link::CodeOutcome::Ok(_)
    ));

    // Stamped before anything knows it won, so it marks the guess that wins too.
    let spent: bool =
        sqlx::query_scalar("SELECT code_spent_at IS NOT NULL FROM magic_link_tokens WHERE id = $1")
            .bind(sent.row.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(spent);

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn supersede_others_spares_other_addresses_and_spent_rows() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let elsewhere = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "other@example.test",
            expiry_minutes: 15,
            nonce: Some("e"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, elsewhere.row.id).await;
    let spent = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "same@example.test",
            expiry_minutes: 15,
            nonce: Some("s"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, spent.row.id).await;
    magic_link::consume(&pool, &spent.token).await.unwrap();
    let earlier = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "same@example.test",
            expiry_minutes: 15,
            nonce: Some("e2"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, earlier.row.id).await;
    let newest = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "same@example.test",
            expiry_minutes: 15,
            nonce: Some("n"),
            ..Default::default()
        },
    )
    .await
    .expect("create");
    mark_sent(&pool, newest.row.id).await;

    let retired = magic_link::supersede_others(&pool, "same@example.test", newest.row.id)
        .await
        .expect("supersede");
    assert_eq!(retired, 1);

    let untouched: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM magic_link_tokens WHERE superseded_at IS NOT NULL")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(untouched, vec![earlier.row.id]);
    assert!(
        magic_link::peek(&pool, &elsewhere.token)
            .await
            .unwrap()
            .is_some()
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_browser_binding_and_the_code_are_hashed_at_rest() {
    let Some((db, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let nonce = "the-cookie-value";
    let created = magic_link::create(
        &pool,
        magic_link::NewMagicLink {
            email: "rest@example.test",
            expiry_minutes: 15,
            nonce: Some(nonce),
            ..Default::default()
        },
    )
    .await
    .expect("create");

    let (code_hash, nonce_hash): (String, String) =
        sqlx::query_as("SELECT code_hash, nonce_hash FROM magic_link_tokens WHERE id = $1")
            .bind(created.row.id)
            .fetch_one(&pool)
            .await
            .expect("read");
    assert!(!code_hash.contains(&created.code));
    assert!(!nonce_hash.contains(nonce));
    assert!(code_hash.starts_with("$argon2"), "{code_hash}");

    pool.close().await;
    drop_pg(&name).await;
}
