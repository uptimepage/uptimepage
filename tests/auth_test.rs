//! Auth schema + transactional email tests.
//!
//! Live-PG tests are `#[ignore]` and run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo nextest run --test it --run-ignored only -E 'test(/^auth_test::/)'
//!
//! Each ignored test provisions a fresh, randomly-named database and tears it
//! down, so they neither touch the shared dev DB nor race each other.

use crate::common;

use std::time::Duration;

use chrono::Utc;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Executor, PgConnection, Row};
use uptimepage::email::{
    EmailAddress, EmailSender, EmailTemplate, InMemoryEmailSender, LogOnlyEmailSender,
    TransactionalEmail,
};
use url::Url;
use uuid::Uuid;

use crate::common::MIGRATOR;

/// Tables created (and dropped) by migration 007_auth.
const AUTH_TABLES: [&str; 7] = [
    "sessions",
    "oauth_identities",
    "oauth_states",
    "api_tokens",
    "invitations",
    "magic_link_tokens",
    "login_attempts",
];

async fn table_exists(pool: &sqlx::PgPool, name: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = $1)",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

fn admin_url_and_test_db_name() -> Option<(Url, String, String)> {
    let raw = std::env::var("DATABASE_URL").ok()?;
    let mut url = Url::parse(&raw).expect("DATABASE_URL must be a valid URL");
    let original_db = url.path().trim_start_matches('/').to_string();
    let test_db = format!("auth_schema_{}", Uuid::now_v7().simple());
    url.set_path("/postgres");
    Some((url, test_db, original_db))
}

async fn create_test_database() -> Option<(String, String)> {
    let (admin_url, test_db, _orig) = admin_url_and_test_db_name()?;
    let mut conn = PgConnection::connect(admin_url.as_str())
        .await
        .expect("connect to admin DB");
    conn.execute(format!("CREATE DATABASE {test_db}").as_str())
        .await
        .expect("CREATE DATABASE");
    let mut new_url = admin_url.clone();
    new_url.set_path(&format!("/{test_db}"));
    Some((new_url.to_string(), test_db))
}

async fn drop_test_database(test_db: &str) {
    let Some((admin_url, _, _)) = admin_url_and_test_db_name() else {
        return;
    };
    let mut conn = match PgConnection::connect(admin_url.as_str()).await {
        Ok(c) => c,
        Err(_) => return,
    };
    // Force-disconnect any leftover sessions before dropping.
    let _ = conn
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{test_db}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    let _ = conn
        .execute(format!("DROP DATABASE IF EXISTS {test_db}").as_str())
        .await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn migrations_apply_forward_and_rollback_cleanly() {
    let Some((db_url, test_db)) = create_test_database().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&db_url)
        .await
        .expect("connect to test DB");

    MIGRATOR.run(&pool).await.expect("forward migrations");

    for tbl in AUTH_TABLES {
        assert!(
            table_exists(&pool, tbl).await,
            "{tbl} should exist after forward migration"
        );
    }

    // Roll back the auth migration (007) only; older down migrations are
    // out-of-scope for this acceptance.
    MIGRATOR
        .undo(&pool, 6)
        .await
        .expect("rollback 007_auth cleanly");

    for tbl in AUTH_TABLES {
        assert!(
            !table_exists(&pool, tbl).await,
            "{tbl} should be gone after rollback"
        );
    }
    assert!(
        table_exists(&pool, "users").await,
        "users must survive 007 rollback"
    );

    // Re-apply forward: the auth tables come back.
    MIGRATOR
        .run(&pool)
        .await
        .expect("forward migrations after rollback");
    for tbl in AUTH_TABLES {
        assert!(
            table_exists(&pool, tbl).await,
            "{tbl} should exist after re-applying forward"
        );
    }

    pool.close().await;
    drop_test_database(&test_db).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL — run via DATABASE_URL=... cargo test -- --ignored"]
async fn auth_tables_accept_representative_inserts() {
    let Some((db_url, test_db)) = create_test_database().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&db_url)
        .await
        .expect("connect");
    MIGRATOR.run(&pool).await.expect("migrate");

    let org_id: Uuid = sqlx::query(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, $2, a.id FROM a RETURNING id",
    )
    .bind(
        format!("test-org-{}", Uuid::now_v7().simple())
            .chars()
            .take(30)
            .collect::<String>(),
    )
    .bind("Test Org")
    .fetch_one(&pool)
    .await
    .expect("insert org")
    .get(0);

    let user_id: Uuid = sqlx::query(
        "INSERT INTO users (email, email_verified_at, terms_version, privacy_version) \
         VALUES ($1, now(), 'v1', 'v1') RETURNING id",
    )
    .bind(format!("user-{}@example.test", Uuid::now_v7().simple()))
    .fetch_one(&pool)
    .await
    .expect("insert user")
    .get(0);

    common::seed_session(
        &pool,
        "sess-fake-id-hash",
        uptimepage::domain::UserId(user_id),
        Some(uptimepage::domain::OrgId(org_id)),
    )
    .await;

    sqlx::query(
        "INSERT INTO oauth_identities (user_id, provider, provider_user_id, provider_username) \
         VALUES ($1, 'github', '12345', 'octocat')",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("insert oauth identity");

    sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, token_prefix) \
         VALUES ($1, 'ci', 'argon2-stub', 'sm_live_abcdefgh')",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("insert api token");

    sqlx::query(
        "INSERT INTO invitations \
            (org_id, inviter_id, email, role, token_hash, token_prefix, expires_at) \
         VALUES ($1, $2, $3, 'member', 'hash', '0123456789abcdef', $4)",
    )
    .bind(org_id)
    .bind(user_id)
    .bind("Mixed.Case@Example.com")
    .bind(Utc::now() + chrono::Duration::days(7))
    .execute(&pool)
    .await
    .expect("insert invitation");

    // CITEXT case-insensitive lookup must find the row regardless of input
    // case. sqlx binds &str as TEXT, so force the cast to make PG use the
    // CITEXT operator rather than a text equality.
    let found: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM invitations WHERE email = $1::citext")
            .bind("mixed.case@example.com")
            .fetch_one(&pool)
            .await
            .expect("CITEXT lookup");
    assert_eq!(found, 1, "CITEXT email lookup must be case-insensitive");

    sqlx::query(
        "INSERT INTO login_attempts (user_id, method, success) VALUES ($1, 'github_oauth', true)",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("insert login attempt");

    pool.close().await;
    drop_test_database(&test_db).await;
}

#[tokio::test]
async fn log_only_sender_renders_invitation_via_tracing() {
    let sender = LogOnlyEmailSender::new("Uptimepage [TEST]");
    let invite = TransactionalEmail {
        to: EmailAddress::new("alice@example.test", "Alice"),
        from: EmailAddress::new("no-reply@example.invalid", "Uptimepage"),
        template: EmailTemplate::Invitation {
            org_name: "Acme".into(),
            inviter_display: "Bob".into(),
            accept_url: "https://example.test/invitations/accept?token=tok".into(),
            decline_url: "https://example.test/invitations/decline?token=tok".into(),
            expires_at: Utc::now() + chrono::Duration::days(7),
        },
    };
    let id = sender.send(invite).await.expect("log-only send");
    assert!(id.0.starts_with("log-only-"));
}

#[tokio::test]
async fn in_memory_sender_captures_for_assertion() {
    let sender = InMemoryEmailSender::new();
    let email = TransactionalEmail {
        to: EmailAddress::new("alice@example.test", "Alice"),
        from: EmailAddress::new("no-reply@example.invalid", "Uptimepage"),
        template: EmailTemplate::Invitation {
            org_name: "Acme".into(),
            inviter_display: "Bob".into(),
            accept_url: "https://example.test/invitations/accept?token=tok".into(),
            decline_url: "https://example.test/invitations/decline?token=tok".into(),
            expires_at: Utc::now() + chrono::Duration::days(7),
        },
    };
    sender.send(email).await.expect("memory send");
    let captured = sender.sent();
    assert_eq!(captured.len(), 1);
    match &captured[0].template {
        EmailTemplate::Invitation {
            org_name,
            accept_url,
            ..
        } => {
            assert_eq!(org_name, "Acme");
            assert!(accept_url.contains("token=tok"));
        }
        _ => panic!("expected Invitation template captured"),
    }
}

// Factory smoke tests live in the lib (`src/email/mod.rs::tests`); the
// integration binary intentionally avoids `build_outbound_client()` because
// macOS sandboxed test runs can't always read native root CAs and that path
// panics on root-CA load failure. The lib `#[cfg(test)]` block exercises
// every factory branch under the same process.
