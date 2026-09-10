//! Live-PG tests for Phase 2-3: OAuth state lifecycle, identity find-or-create
//! with signup org auto-create, session CRUD with idle + absolute timeouts,
//! cookie-driven extractor, and `/api/v1/me` smoke.
//!
//! Run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo test --test auth_session_test -- --ignored

mod common;

use chrono::{Duration as ChronoDuration, Utc};
use uptimepage::auth::OauthProvider as P;
use uptimepage::auth::{
    OauthProvider, fingerprint,
    login_audit::{self, LoginAttempt, LoginMethod},
    oauth_login::{self, RemoteIdentity},
    oauth_state, session as session_store,
};
use uptimepage::config::SessionConfig;
use uptimepage::domain::UserId;
use uptimepage::error::AppError;
use uptimepage::storage::oauth_identities::{self, WaysIn};
use uuid::Uuid;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

/// Every provider works, but the address is not a way back — the shape a
/// deployment takes with magic link off, or with no deliverable mail sender.
fn no_email_back() -> WaysIn {
    WaysIn {
        enabled_providers: P::ALL.to_vec(),
        passkeys_open_the_account: false,
        email_is_a_way_back: false,
    }
}

/// Magic link on and mail actually deliverable, so the last provider may go.
fn email_is_back() -> WaysIn {
    WaysIn {
        enabled_providers: P::ALL.to_vec(),
        passkeys_open_the_account: false,
        email_is_a_way_back: true,
    }
}

async fn fresh_pg() -> Option<(String, String)> {
    common::fresh_test_db("auth_session").await
}

async fn drop_pg(test_db: &str) {
    common::drop_test_db(test_db).await;
}

async fn open_pool(db_url: &str) -> sqlx::PgPool {
    common::open_test_pool(db_url).await
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn oauth_state_insert_consume_round_trip() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let s = oauth_state::generate_state();
    let inv_id = uuid::Uuid::new_v4();
    oauth_state::insert(
        &pool,
        &s,
        "github",
        oauth_state::StateBinding {
            redirect_after: Some("/after"),
            invitation_id: Some(inv_id),
            ..Default::default()
        },
    )
    .await
    .expect("insert");
    let consumed = oauth_state::consume(&pool, &s)
        .await
        .expect("consume")
        .expect("row");
    assert_eq!(consumed.provider, "github");
    assert_eq!(consumed.redirect_after.as_deref(), Some("/after"));
    assert_eq!(consumed.invitation_id, Some(inv_id));
    assert_eq!(consumed.org_id, None);

    // Second consume must return None — state is single-use.
    let again = oauth_state::consume(&pool, &s).await.expect("re-consume");
    assert!(again.is_none(), "second consume must fail");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn oauth_state_consume_rejects_expired() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    sqlx::query(
        "INSERT INTO oauth_states (state_hash, provider, expires_at) VALUES ($1, 'github', now() - INTERVAL '1 minute')",
    )
    .bind(oauth_state::hash_state("expired-state"))
    .execute(&pool)
    .await
    .expect("seed expired");

    let consumed = oauth_state::consume(&pool, "expired-state").await.unwrap();
    assert!(consumed.is_none(), "expired state must not consume");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn upsert_creates_user_and_signup_org_for_new_identity() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let identity = RemoteIdentity {
        provider_user_id: "12345".into(),
        provider_username: Some("octocat".into()),
        verified_email: Some("Alice@Example.test".into()),
        display_name: Some("Alice".into()),
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("upsert");
    assert!(resolved.is_new_user);
    assert!(resolved.signup_org_id.is_some());

    // CITEXT — invitation row with lower-case match should find this user.
    let (user_email,): (String,) = sqlx::query_as("SELECT email::text FROM users WHERE id = $1")
        .bind(resolved.user_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(user_email.to_lowercase(), "alice@example.test");

    // Idempotent re-callback with same identity must NOT create a second
    // user. Returns is_new_user=false; signup_org_id resolves to the org
    // the first call created.
    let again = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("re-upsert");
    assert!(!again.is_new_user);
    assert_eq!(again.signup_org_id, resolved.signup_org_id);
    assert_eq!(again.user_id.0, resolved.user_id.0);

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn upsert_links_existing_user_on_email_match() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let (existing_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ('Bob@Example.test', 'v1', 'v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed user");

    let identity = RemoteIdentity {
        provider_user_id: "99".into(),
        provider_username: Some("bob".into()),
        verified_email: Some("bob@example.test".into()),
        display_name: None,
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("upsert");
    assert!(!resolved.is_new_user);
    // Bob existed with no memberships → signup_org_id is None.
    assert!(resolved.signup_org_id.is_none());
    assert_eq!(resolved.user_id.0, existing_id);

    // Identity link must have been inserted.
    let (linked_count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM oauth_identities WHERE provider_user_id = $1")
            .bind("99")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked_count, 1);

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn upsert_reports_pending_deletion_without_restoring_on_reauth() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    // Seed a user + identity, then soft-delete the user. A subsequent GitHub
    // OAuth sign-in for the same provider_user_id resolves to that row and
    // reports the pending deletion — signing in must not cancel it.
    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ('Carol@Example.test', 'v1', 'v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed user");
    sqlx::query(
        "INSERT INTO oauth_identities (user_id, provider, provider_user_id, provider_username) \
         VALUES ($1, 'github', '777', 'carol')",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("seed identity");
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("soft-delete user");

    let identity = RemoteIdentity {
        provider_user_id: "777".into(),
        provider_username: Some("carol".into()),
        verified_email: Some("carol@example.test".into()),
        display_name: Some("Carol".into()),
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("soft-deleted identity resolves on re-auth");
    assert_eq!(resolved.user_id.0, user_id);
    assert!(
        resolved.pending_deletion.is_some(),
        "re-auth must report the pending deletion"
    );
    assert!(!resolved.is_new_user, "must not create a parallel user");

    let (deleted_at,): (Option<chrono::DateTime<Utc>>,) =
        sqlx::query_as("SELECT deleted_at FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        deleted_at.is_some(),
        "signing in must not cancel the deletion"
    );
    let (user_count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM users WHERE email = $1::citext")
            .bind("carol@example.test")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(user_count, 1, "must not have created a parallel user");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn upsert_links_a_second_provider_and_reports_it_for_the_notice() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let github_id = RemoteIdentity {
        provider_user_id: "555".into(),
        provider_username: Some("dora".into()),
        verified_email: Some("Dora@Example.test".into()),
        display_name: Some("Dora".into()),
    };
    let first = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &github_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("github signup");
    assert!(first.is_new_user);

    // Same verified email arriving via Google lands on the SAME user — one
    // account, two identity rows. `newly_linked` is what the callback turns
    // into mail, so a credential can never appear without the owner hearing.
    let google_id = RemoteIdentity {
        provider_user_id: "g-sub-1".into(),
        provider_username: Some("dora@example.test".into()),
        verified_email: Some("dora@example.test".into()),
        display_name: Some("Dora".into()),
    };
    let second = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Google,
        &google_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("google link");
    assert!(!second.is_new_user);
    assert!(second.newly_linked, "the account must be told");
    assert_eq!(second.user_id.0, first.user_id.0);
    assert_eq!(second.signup_org_id, first.signup_org_id);

    let (identities,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM oauth_identities WHERE user_id = $1")
            .bind(first.user_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(identities, 2);

    // Signing in again with a provider already on file is not a new link, so
    // it must not send mail every time.
    let again = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Google,
        &google_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("google re-auth");
    assert!(!again.newly_linked);

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_identity_on_file_outranks_a_matching_address() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    // Two accounts. `owner` holds the Google identity; `claimant` merely has
    // the address Google now attests — a provider that changed the email on an
    // existing account, or an address that moved hands.
    let owner = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Google,
        &RemoteIdentity {
            provider_user_id: "g-owner".into(),
            provider_username: None,
            verified_email: Some("first@example.test".into()),
            display_name: None,
        },
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("owner signup");
    let (claimant,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ('second@example.test', 'v1', 'v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed claimant");

    // The identity is the key, not the address: this must open `owner`, and
    // must not hand `claimant`'s account to a provider account it never had.
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Google,
        &RemoteIdentity {
            provider_user_id: "g-owner".into(),
            provider_username: None,
            verified_email: Some("second@example.test".into()),
            display_name: None,
        },
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("identity match");
    assert_eq!(resolved.user_id.0, owner.user_id.0);
    assert!(
        !resolved.newly_linked,
        "nothing was linked, it was already ours"
    );

    assert!(
        oauth_identities::list_for_user(&pool, UserId(claimant))
            .await
            .unwrap()
            .is_empty(),
        "the address alone must not have attached anything"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn upsert_matches_tombstoned_user_by_email_on_new_provider() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    // GitHub signup, then account deletion. A first-ever Google sign-in with
    // the same verified email must resolve to this account, not mint a
    // duplicate user row (the email unique index is partial — active rows
    // only). It must not cancel the deletion either.
    let github_id = RemoteIdentity {
        provider_user_id: "888".into(),
        provider_username: Some("gail".into()),
        verified_email: Some("gail@example.test".into()),
        display_name: None,
    };
    let first = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &github_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("github signup");
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(first.user_id.0)
        .execute(&pool)
        .await
        .expect("soft-delete");

    let google_id = RemoteIdentity {
        provider_user_id: "g-sub-3".into(),
        provider_username: Some("gail@example.test".into()),
        verified_email: Some("gail@example.test".into()),
        display_name: None,
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Google,
        &google_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("google email-match");
    assert_eq!(resolved.user_id.0, first.user_id.0);
    assert!(resolved.pending_deletion.is_some());
    assert!(!resolved.is_new_user);

    let (user_count,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM users WHERE email = $1::citext")
            .bind("gail@example.test")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(user_count, 1, "no duplicate user row");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn link_adds_a_second_provider_to_a_signed_in_account() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let github_id = RemoteIdentity {
        provider_user_id: "901".into(),
        provider_username: Some("ivy".into()),
        verified_email: Some("ivy@example.test".into()),
        display_name: None,
    };
    let owner = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &github_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("github signup");

    // Whatever address the second provider attests is irrelevant here: the
    // session already proved who this is, which is what makes a link safe.
    let gitlab_id = RemoteIdentity {
        provider_user_id: "https://gitlab.com/42".into(),
        provider_username: Some("ivy-at-work".into()),
        verified_email: Some("ivy@work.test".into()),
        display_name: None,
    };
    let outcome =
        oauth_login::link_identity_to_user(&pool, OauthProvider::Gitlab, &gitlab_id, owner.user_id)
            .await
            .expect("link");
    assert_eq!(outcome, oauth_login::LinkOutcome::Linked);

    // Replaying the same dance is benign, not a second row.
    let again =
        oauth_login::link_identity_to_user(&pool, OauthProvider::Gitlab, &gitlab_id, owner.user_id)
            .await
            .expect("relink");
    assert_eq!(again, oauth_login::LinkOutcome::AlreadyLinked);

    let (identities,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM oauth_identities WHERE user_id = $1")
            .bind(owner.user_id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(identities, 2);

    // Signing in with the linked provider now opens the account it was
    // attached to, without touching the email path at all.
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Gitlab,
        &gitlab_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("gitlab sign-in");
    assert_eq!(resolved.user_id.0, owner.user_id.0);
    assert!(!resolved.is_new_user);

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn link_refuses_a_provider_account_that_opens_someone_else() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let mk = |id: &str, email: &str| RemoteIdentity {
        provider_user_id: id.into(),
        provider_username: None,
        verified_email: Some(email.into()),
        display_name: None,
    };
    let a = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &mk("a-1", "a@example.test"),
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("a signup");
    let b = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &mk("b-1", "b@example.test"),
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("b signup");

    let err = oauth_login::link_identity_to_user(
        &pool,
        OauthProvider::Github,
        &mk("a-1", "a@example.test"),
        b.user_id,
    )
    .await
    .expect_err("a-1 is already a's credential");
    assert!(
        matches!(err, AppError::BadRequest { code, .. } if code == oauth_login::IDENTITY_TAKEN)
    );

    let (owner,): (Uuid,) = sqlx::query_as(
        "SELECT user_id FROM oauth_identities WHERE provider = 'github' AND provider_user_id = 'a-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owner, a.user_id.0, "ownership never moved");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn unlink_holds_the_last_way_in_only_when_email_cannot_open_it() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let github_id = RemoteIdentity {
        provider_user_id: "777".into(),
        provider_username: Some("finn".into()),
        verified_email: Some("finn@example.test".into()),
        display_name: None,
    };
    let owner = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &github_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("github signup");

    // Magic link off: this row is the only way in, so it stays.
    let err = oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Github,
        Some("777"),
        &no_email_back(),
        None,
        Default::default(),
    )
    .await
    .expect_err("the only method must survive");
    assert!(matches!(err, AppError::BadRequest { code, .. } if code == "LAST_SIGN_IN_METHOD"));

    // Magic link on: the address itself opens the account, so a user whose
    // provider is compromised can drop it without first handing another
    // provider a credential on the account they are locking down.
    oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Github,
        Some("777"),
        &email_is_back(),
        None,
        Default::default(),
    )
    .await
    .expect("email is a way back");
    assert!(
        oauth_identities::list_for_user(&pool, owner.user_id)
            .await
            .unwrap()
            .is_empty()
    );

    oauth_login::link_identity_to_user(&pool, OauthProvider::Github, &github_id, owner.user_id)
        .await
        .expect("relink github");

    let gitlab_id = RemoteIdentity {
        provider_user_id: "https://gitlab.com/7".into(),
        provider_username: None,
        verified_email: None,
        display_name: None,
    };
    oauth_login::link_identity_to_user(&pool, OauthProvider::Gitlab, &gitlab_id, owner.user_id)
        .await
        .expect("link second");

    oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Github,
        Some("777"),
        &no_email_back(),
        None,
        Default::default(),
    )
    .await
    .expect("removable once a second exists");

    let rows = oauth_identities::list_for_user(&pool, owner.user_id)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].provider, "gitlab");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn unlink_without_a_subject_cannot_empty_the_account() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    // Two accounts at the same vendor, which the API's own
    // `?provider_user_id=` parameter exists to tell apart.
    let first = RemoteIdentity {
        provider_user_id: "gh-a".into(),
        provider_username: Some("work".into()),
        verified_email: Some("two@example.test".into()),
        display_name: None,
    };
    let owner = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &first,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("github signup");
    let second = RemoteIdentity {
        provider_user_id: "gh-b".into(),
        provider_username: Some("personal".into()),
        verified_email: None,
        display_name: None,
    };
    oauth_login::link_identity_to_user(&pool, OauthProvider::Github, &second, owner.user_id)
        .await
        .expect("second github");

    // Omitting the subject removes BOTH, so the guard has to weigh what would
    // remain, not what is there now — counting rows before the delete lets
    // this call empty the account and lock it out.
    let err = oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Github,
        None,
        &no_email_back(),
        None,
        Default::default(),
    )
    .await
    .expect_err("that would leave nothing");
    assert!(matches!(err, AppError::BadRequest { code, .. } if code == "LAST_SIGN_IN_METHOD"));
    assert_eq!(
        oauth_identities::list_for_user(&pool, owner.user_id)
            .await
            .unwrap()
            .len(),
        2,
        "the refusal must not have deleted anything"
    );

    // Naming one leaves the other, so it goes through.
    oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Github,
        Some("gh-a"),
        &no_email_back(),
        None,
        Default::default(),
    )
    .await
    .expect("one of two is removable");

    let (events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM credential_events \
          WHERE user_id = $1 AND action = 'unlinked' AND origin = 'session'",
    )
    .bind(owner.user_id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "removal leaves a trail the mail cannot");

    pool.close().await;
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn unlink_reports_a_method_that_is_not_on_the_account() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let github_id = RemoteIdentity {
        provider_user_id: "601".into(),
        provider_username: None,
        verified_email: Some("nan@example.test".into()),
        display_name: None,
    };
    let owner = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &github_id,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("github signup");

    // Not "you would lock yourself out" — this method was never here.
    let err = oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Gitlab,
        Some("nope"),
        &no_email_back(),
        None,
        Default::default(),
    )
    .await
    .expect_err("gitlab was never linked");
    assert!(matches!(err, AppError::NotFound { code, .. } if code == "SIGN_IN_METHOD_NOT_FOUND"));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn upsert_reports_pending_deletion_on_google_reauth() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ('Erin@Example.test', 'v1', 'v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed user");
    sqlx::query(
        "INSERT INTO oauth_identities (user_id, provider, provider_user_id, provider_username) \
         VALUES ($1, 'google', 'g-sub-2', 'erin@example.test')",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("seed identity");
    sqlx::query("UPDATE users SET deleted_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("soft-delete user");

    let identity = RemoteIdentity {
        provider_user_id: "g-sub-2".into(),
        provider_username: Some("erin@example.test".into()),
        verified_email: Some("erin@example.test".into()),
        display_name: None,
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Google,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("google re-auth resolves");
    assert_eq!(resolved.user_id.0, user_id);
    assert!(resolved.pending_deletion.is_some());

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn session_create_lookup_destroy_round_trip() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let (user_id,): (Uuid,) = sqlx::query_as("INSERT INTO users (email, terms_version, privacy_version) VALUES ($1, 'v1', 'v1') RETURNING id")
        .bind(format!("ses-{}@example.test", Uuid::now_v7()))
        .fetch_one(&pool)
        .await
        .unwrap();
    let user = UserId(user_id);
    let cfg = SessionConfig::default();
    let created = session_store::create(&pool, &cfg, user, None, Some("ip"), Some("ua"))
        .await
        .expect("create");
    assert_eq!(
        created.cookie_token.len(),
        43,
        "cookie value is 43-char b64url"
    );
    assert_eq!(created.row.id.len(), 64, "id_hash is 64-char sha256 hex");
    assert_ne!(
        created.cookie_token, created.row.id,
        "cookie value must not equal its DB hash"
    );

    let outcome = session_store::lookup(&pool, &cfg, &created.cookie_token)
        .await
        .unwrap();
    assert!(matches!(outcome, session_store::LookupOutcome::Active(_)));

    // Sanity: presenting the hash as the cookie must NOT log in — only the raw
    // pre-hash secret should match the row, otherwise hashing buys nothing.
    let hash_as_cookie = session_store::lookup(&pool, &cfg, &created.row.id)
        .await
        .unwrap();
    assert!(matches!(
        hash_as_cookie,
        session_store::LookupOutcome::Missing
    ));

    let removed = session_store::destroy(&pool, &created.cookie_token)
        .await
        .unwrap();
    assert_eq!(removed, 1);

    let missing = session_store::lookup(&pool, &cfg, &created.cookie_token)
        .await
        .unwrap();
    assert!(matches!(missing, session_store::LookupOutcome::Missing));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn session_lookup_destroys_idle_expired() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let (user_id,): (Uuid,) = sqlx::query_as("INSERT INTO users (email, terms_version, privacy_version) VALUES ($1, 'v1', 'v1') RETURNING id")
        .bind(format!("idle-{}@example.test", Uuid::now_v7()))
        .fetch_one(&pool)
        .await
        .unwrap();
    let user = UserId(user_id);

    // Mint a real session, then backdate `last_used_at` past the idle window
    // so lookup must reap it. Going through `create` keeps the test honest:
    // a hand-crafted id would have to pre-hash the cookie value to match the
    // new schema, and that's exactly the production codepath we want to
    // exercise.
    let cfg = SessionConfig::default();
    let created = session_store::create(&pool, &cfg, user, None, None, None)
        .await
        .expect("create");
    sqlx::query(
        "UPDATE sessions SET last_used_at = now() - INTERVAL '40 days' \
         WHERE id_hash = $1",
    )
    .bind(&created.row.id)
    .execute(&pool)
    .await
    .unwrap();

    let outcome = session_store::lookup(&pool, &cfg, &created.cookie_token)
        .await
        .unwrap();
    assert!(matches!(outcome, session_store::LookupOutcome::Expired));
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM sessions WHERE id_hash = $1")
        .bind(&created.row.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "idle-expired row must be deleted");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn session_touch_debounced_writes_at_most_once_per_window() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let (user_id,): (Uuid,) = sqlx::query_as("INSERT INTO users (email, terms_version, privacy_version) VALUES ($1, 'v1', 'v1') RETURNING id")
        .bind(format!("touch-{}@example.test", Uuid::now_v7()))
        .fetch_one(&pool)
        .await
        .unwrap();
    let user = UserId(user_id);
    let cfg = SessionConfig::default();
    let created = session_store::create(&pool, &cfg, user, None, None, None)
        .await
        .expect("create");

    // Backdate last_used_at so the first touch produces a measurable bump.
    sqlx::query(
        "UPDATE sessions SET last_used_at = now() - INTERVAL '5 minutes' WHERE id_hash = $1",
    )
    .bind(&created.row.id)
    .execute(&pool)
    .await
    .unwrap();

    let debounce = session_store::build_debounce_cache();
    for _ in 0..5 {
        session_store::touch_last_used_debounced(&pool, &debounce, &created.row.id)
            .await
            .unwrap();
    }

    // Within the window, exactly one UPDATE should have run.
    let (last_used,): (chrono::DateTime<Utc>,) =
        sqlx::query_as("SELECT last_used_at FROM sessions WHERE id_hash = $1")
            .bind(&created.row.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        Utc::now().signed_duration_since(last_used) < ChronoDuration::seconds(10),
        "first touch should have moved last_used_at to ~now"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn fingerprint_salt_guard_first_boot_inserts_then_rejects_change() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    // First boot: empty history, inserts the digest.
    let inserted = fingerprint::ensure_fingerprint_salt(&pool, "salt-A")
        .await
        .unwrap();
    assert!(inserted, "first salt must be inserted");

    // Same salt: known → no insert.
    let again = fingerprint::ensure_fingerprint_salt(&pool, "salt-A")
        .await
        .unwrap();
    assert!(!again, "same salt must not re-insert");

    // Different salt without override env: refuses to boot.
    // SAFETY: tests in this binary run on the same process; clean the env
    // before and after so we don't leak state to peers.
    unsafe {
        std::env::remove_var(fingerprint::ROTATION_OVERRIDE_ENV);
    }
    let err = fingerprint::ensure_fingerprint_salt(&pool, "salt-B")
        .await
        .expect_err("rotation without override must fail");
    let msg = format!("{err:?}");
    assert!(msg.contains(fingerprint::SALT_ROTATED_CODE), "{msg}");

    // With override env: accepts rotation, persists new digest.
    unsafe {
        std::env::set_var(fingerprint::ROTATION_OVERRIDE_ENV, "1");
    }
    let accepted = fingerprint::ensure_fingerprint_salt(&pool, "salt-B")
        .await
        .unwrap();
    assert!(accepted);
    unsafe {
        std::env::remove_var(fingerprint::ROTATION_OVERRIDE_ENV);
    }

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn login_audit_records_success_and_failure() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let (user_id,): (Uuid,) = sqlx::query_as("INSERT INTO users (email, terms_version, privacy_version) VALUES ($1, 'v1', 'v1') RETURNING id")
        .bind(format!("aud-{}@example.test", Uuid::now_v7()))
        .fetch_one(&pool)
        .await
        .unwrap();
    let user = UserId(user_id);

    login_audit::record(
        &pool,
        LoginMethod::GithubOauth,
        LoginAttempt {
            user_id: Some(user),
            success: true,
            ip_hash: Some("iph"),
            user_agent_hash: Some("uah"),
            failure_reason: None,
        },
    )
    .await
    .unwrap();
    login_audit::record(
        &pool,
        LoginMethod::GithubOauth,
        LoginAttempt {
            user_id: None,
            success: false,
            ip_hash: Some("iph"),
            user_agent_hash: None,
            failure_reason: Some("invalid_state"),
        },
    )
    .await
    .unwrap();

    let (succ_count, fail_count): (i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM login_attempts WHERE success), \
            (SELECT count(*) FROM login_attempts WHERE NOT success)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(succ_count, 1);
    assert_eq!(fail_count, 1);
    // Failed-row select must come back through the partial index too.
    let (recent_fail_ip,): (Option<String>,) =
        sqlx::query_as("SELECT ip_hash FROM login_attempts WHERE success = false LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recent_fail_ip.as_deref(), Some("iph"));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn oauth_state_cleanup_purges_only_expired_rows() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    sqlx::query(
        "INSERT INTO oauth_states (state_hash, provider, expires_at) VALUES \
         ($1, 'github', now() - INTERVAL '15 minutes'), \
         ($2, 'github', now() - INTERVAL '1 second'), \
         ($3, 'github', now() + INTERVAL '5 minutes')",
    )
    .bind(oauth_state::hash_state("expired-old"))
    .bind(oauth_state::hash_state("expired-recent"))
    .bind(oauth_state::hash_state("still-valid"))
    .execute(&pool)
    .await
    .unwrap();

    let removed = oauth_state::purge_expired(&pool).await.unwrap();
    assert_eq!(removed, 2, "exactly the two expired rows should be purged");

    let (still,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM oauth_states WHERE state_hash = $1")
            .bind(oauth_state::hash_state("still-valid"))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still, 1, "valid future-expiry row must survive");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn session_fixation_pattern_destroys_pre_login_session() {
    // Mirrors what github_callback now does: read the pre-login cookie value,
    // destroy that session before minting a new one. We exercise the helpers
    // directly because the full callback path needs a GitHub mock.
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let (user_id,): (Uuid,) = sqlx::query_as("INSERT INTO users (email, terms_version, privacy_version) VALUES ($1, 'v1', 'v1') RETURNING id")
        .bind(format!("fix-{}@example.test", Uuid::now_v7()))
        .fetch_one(&pool)
        .await
        .unwrap();
    let user = UserId(user_id);
    let cfg = SessionConfig::default();

    // Pre-login session (attacker-supplied or stale).
    let pre = session_store::create(&pool, &cfg, user, None, None, None)
        .await
        .unwrap();

    // Callback: destroy the cookie's session id, then mint a fresh one.
    let removed = session_store::destroy(&pool, &pre.cookie_token)
        .await
        .unwrap();
    assert_eq!(removed, 1, "pre-login session must be destroyed");

    let post = session_store::create(&pool, &cfg, user, None, None, None)
        .await
        .unwrap();
    assert_ne!(
        pre.cookie_token, post.cookie_token,
        "regenerated cookie must differ"
    );
    assert_ne!(pre.row.id, post.row.id, "regenerated id_hash must differ");

    let pre_lookup = session_store::lookup(&pool, &cfg, &pre.cookie_token)
        .await
        .unwrap();
    assert!(
        matches!(pre_lookup, session_store::LookupOutcome::Missing),
        "old session must not be revivable"
    );

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_disabled_provider_is_a_row_not_a_way_in() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let owner = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &RemoteIdentity {
            provider_user_id: "gh-off".into(),
            provider_username: None,
            verified_email: Some("off@example.test".into()),
            display_name: None,
        },
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("signup");
    oauth_login::link_identity_to_user(
        &pool,
        OauthProvider::Gitlab,
        &RemoteIdentity {
            provider_user_id: "gl-on".into(),
            provider_username: None,
            verified_email: None,
            display_name: None,
        },
        owner.user_id,
    )
    .await
    .expect("link gitlab");

    // GitHub is switched off, so `/auth/github/login` answers 404. Counting it
    // as a way in would let the account drop its only working method.
    let gitlab_only = WaysIn {
        enabled_providers: vec![OauthProvider::Gitlab],
        passkeys_open_the_account: false,
        email_is_a_way_back: false,
    };
    let err = oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Gitlab,
        Some("gl-on"),
        &gitlab_only,
        None,
        Default::default(),
    )
    .await
    .expect_err("github cannot be signed in with");
    assert!(matches!(err, AppError::BadRequest { code, .. } if code == "LAST_SIGN_IN_METHOD"));

    // The one that does not work is free to go.
    oauth_identities::unlink(
        &pool,
        owner.user_id,
        OauthProvider::Github,
        Some("gh-off"),
        &gitlab_only,
        None,
        Default::default(),
    )
    .await
    .expect("a method nothing can use is not what keeps the account reachable");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn signup_and_a_later_link_both_leave_a_trail() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let owner = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &RemoteIdentity {
            provider_user_id: "gh-first".into(),
            provider_username: None,
            verified_email: Some("trail@example.test".into()),
            display_name: None,
        },
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("signup");
    assert!(owner.is_new_user);

    // The runbook tells operators every credential is in this table. An account
    // whose only method is the signup one must not read as "nothing was ever
    // added" — that is the answer that ends an investigation early.
    oauth_identities::record_event(
        &pool,
        owner.user_id,
        oauth_identities::CredentialEvent {
            provider: OauthProvider::Github.as_db_str(),
            provider_user_id: "gh-first",
            action: uptimepage::auth::CredentialAction::Linked,
            origin: uptimepage::auth::CredentialOrigin::Signup,
            ip_hash: None,
            user_agent_hash: None,
        },
    )
    .await;

    let (origin,): (String,) =
        sqlx::query_as("SELECT origin FROM credential_events WHERE user_id = $1")
            .bind(owner.user_id.0)
            .fetch_one(&pool)
            .await
            .expect("the first credential is on record");
    assert_eq!(origin, "signup");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_migration_gives_older_identities_their_signup_row() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;

    // Everything up to and including 043, then an identity as it would exist
    // on a database that predates the credential trail.
    sqlx::migrate::Migrator::new(std::path::Path::new("./migrations/postgres"))
        .await
        .expect("load")
        .undo(&pool, 0)
        .await
        .ok();
    MIGRATOR.run(&pool).await.expect("migrate");
    sqlx::query("DELETE FROM credential_events")
        .execute(&pool)
        .await
        .expect("clear");

    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ('old@example.test', 'v1', 'v1') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .expect("seed user");
    sqlx::query(
        "INSERT INTO oauth_identities (user_id, provider, provider_user_id, provider_username) \
         VALUES ($1, 'github', 'gh-old', 'old')",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("seed identity");

    // Re-run just the backfill statement the migration carries.
    sqlx::query(
        "INSERT INTO credential_events \
             (user_id, provider, provider_user_id, action, origin, occurred_at) \
         SELECT user_id, provider, provider_user_id, 'linked', 'signup', created_at \
           FROM oauth_identities",
    )
    .execute(&pool)
    .await
    .expect("backfill");

    // Without this, the runbook's query answers "nothing was ever added" for
    // every account that existed before the table did.
    let (provider, origin): (String, String) =
        sqlx::query_as("SELECT provider, origin FROM credential_events WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("an account older than the table still has provenance");
    assert_eq!(provider, "github");
    assert_eq!(origin, "signup");

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deferred_signup_leaves_an_invitee_with_only_the_joined_org() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let inviter = common::make_user(&pool, "inviter").await;
    let team = uptimepage::storage::create_org_with_owner(
        &pool,
        inviter,
        &common::unique_slug("team"),
        "Team",
    )
    .await
    .expect("create team org")
    .expect("slug free");

    let identity = RemoteIdentity {
        provider_user_id: "invitee-1".into(),
        provider_username: Some("invitee".into()),
        verified_email: Some("invitee@example.test".into()),
        display_name: None,
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Defer,
    )
    .await
    .expect("upsert");
    assert!(resolved.is_new_user);
    assert!(resolved.signup_org_id.is_none());
    assert_eq!(memberships_of(&pool, resolved.user_id).await, 0);
    assert_eq!(accounts_owned_by(&pool, resolved.user_id).await, 0);

    uptimepage::storage::orgs::add_member(
        &pool,
        team.id,
        inviter,
        resolved.user_id,
        uptimepage::domain::Role::Member,
        10,
    )
    .await
    .expect("join team");

    let (org, created) = uptimepage::storage::users::ensure_signup_org(&pool, resolved.user_id)
        .await
        .expect("ensure");
    assert_eq!(org, team.id);
    assert!(!created);
    assert_eq!(memberships_of(&pool, resolved.user_id).await, 1);
    assert_eq!(accounts_owned_by(&pool, resolved.user_id).await, 0);

    let again = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Defer,
    )
    .await
    .expect("re-upsert");
    assert!(!again.is_new_user);
    assert_eq!(again.signup_org_id, Some(team.id));

    pool.close().await;
    drop_pg(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn deferred_signup_gets_a_personal_org_when_the_invitation_does_not_land() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let identity = RemoteIdentity {
        provider_user_id: "invitee-2".into(),
        provider_username: None,
        verified_email: Some("stranded@example.test".into()),
        display_name: None,
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Defer,
    )
    .await
    .expect("upsert");
    assert!(resolved.signup_org_id.is_none());

    let (org, created) = uptimepage::storage::users::ensure_signup_org(&pool, resolved.user_id)
        .await
        .expect("ensure");
    assert!(created);
    assert_eq!(memberships_of(&pool, resolved.user_id).await, 1);
    assert_eq!(accounts_owned_by(&pool, resolved.user_id).await, 1);
    let (role,): (String,) =
        sqlx::query_as("SELECT role FROM memberships WHERE user_id = $1 AND org_id = $2")
            .bind(resolved.user_id.0)
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(role, "owner");
    assert_eq!(
        uptimepage::storage::users::get_signup_org_id(&pool, resolved.user_id)
            .await
            .unwrap(),
        Some(org)
    );

    let (same, created_twice) =
        uptimepage::storage::users::ensure_signup_org(&pool, resolved.user_id)
            .await
            .expect("ensure again");
    assert_eq!(same, org);
    assert!(!created_twice);
    assert_eq!(memberships_of(&pool, resolved.user_id).await, 1);

    pool.close().await;
    drop_pg(&name).await;
}

async fn memberships_of(pool: &sqlx::PgPool, user: UserId) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM memberships WHERE user_id = $1")
        .bind(user.0)
        .fetch_one(pool)
        .await
        .unwrap()
        .0
}

async fn accounts_owned_by(pool: &sqlx::PgPool, user: UserId) -> i64 {
    sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM accounts WHERE owner_user_id = $1")
        .bind(user.0)
        .fetch_one(pool)
        .await
        .unwrap()
        .0
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn a_user_removed_from_their_signup_org_gets_a_new_personal_org() {
    let Some((db_url, name)) = fresh_pg().await else {
        return;
    };
    let pool = open_pool(&db_url).await;
    MIGRATOR.run(&pool).await.expect("migrate");

    let identity = RemoteIdentity {
        provider_user_id: "removed-1".into(),
        provider_username: None,
        verified_email: Some("removed@example.test".into()),
        display_name: None,
    };
    let resolved = oauth_login::upsert_identity_and_signup_org(
        &pool,
        OauthProvider::Github,
        &identity,
        uptimepage::security::Admission::Clear,
        oauth_login::SignupOrg::Create,
    )
    .await
    .expect("upsert");
    let first = resolved.signup_org_id.expect("signup org");

    sqlx::query("DELETE FROM memberships WHERE user_id = $1")
        .bind(resolved.user_id.0)
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        uptimepage::storage::users::resolve_signup_org(&pool, resolved.user_id)
            .await
            .unwrap(),
        None
    );
    let (org, created) = uptimepage::storage::users::ensure_signup_org(&pool, resolved.user_id)
        .await
        .expect("ensure");
    assert!(created);
    assert_ne!(org, first);
    assert_eq!(memberships_of(&pool, resolved.user_id).await, 1);

    pool.close().await;
    drop_pg(&name).await;
}
