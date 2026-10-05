//! Live-PG tests for unattended first-run owner seeding.
//!
//! Run via:
//!     docker compose -f compose.dev.yml up -d postgres
//!     DATABASE_URL=postgres://monitor:monitor@localhost:5432/monitor \
//!         cargo nextest run --test it --run-ignored all -E 'test(/^bootstrap_seed_pg_test::/)'

use crate::common;

use sqlx::PgPool;
use uptimepage::bootstrap::seed_first_owner;
use uptimepage::config::AppConfig;
use uptimepage::domain::OrgId;
use uptimepage::quotas::QuotaService;
use uptimepage::storage::{orgs as orgs_store, users};

use crate::common::MIGRATOR;

async fn fresh_pool() -> Option<(PgPool, String)> {
    let (db, name) = common::fresh_test_db("bootstrap_seed").await?;
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();
    Some((pool, name))
}

fn cfg_with(email: &str) -> AppConfig {
    let mut cfg = AppConfig::load().expect("config");
    cfg.bootstrap.email = email.to_string();
    cfg.bootstrap.org_name = "Home Lab".to_string();
    cfg
}

async fn user_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn link_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM magic_link_tokens")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn org_id(pool: &PgPool) -> OrgId {
    let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM organizations")
        .fetch_one(pool)
        .await
        .unwrap();
    OrgId(id)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn seeds_owner_org_and_a_usable_link() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };

    seed_first_owner(&pool, &cfg_with("owner@example.test"))
        .await
        .expect("seed");

    assert_eq!(user_count(&pool).await, 1);
    assert_eq!(link_count(&pool).await, 1);

    let (user, created) = users::create_invited_user(&pool, "owner@example.test", None)
        .await
        .expect("lookup owner");
    assert!(!created, "seeding must have created the owner already");

    let org = orgs_store::oldest_membership_for_user(&pool, user)
        .await
        .expect("membership")
        .expect("owner must own an org");
    let row = orgs_store::get_org(&pool, org)
        .await
        .expect("get_org")
        .expect("org row");
    assert_eq!(row.name, "Home Lab");

    // The link lands in the log stream and may sit unread for hours, so it gets
    // a longer life than an interactive login.
    let hours: f64 = sqlx::query_scalar(
        "SELECT (extract(epoch FROM expires_at - now()) / 3600)::float8 FROM magic_link_tokens",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((23.0..=24.0).contains(&hours), "expiry was {hours}h");

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn seeding_is_one_shot_across_restarts() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let cfg = cfg_with("owner@example.test");

    seed_first_owner(&pool, &cfg).await.expect("first boot");
    seed_first_owner(&pool, &cfg).await.expect("second boot");
    seed_first_owner(&pool, &cfg).await.expect("third boot");

    assert_eq!(user_count(&pool).await, 1, "owner seeded once");
    assert_eq!(
        link_count(&pool).await,
        1,
        "every restart must not mint a new sign-in link"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn soft_deleted_owner_does_not_reopen_seeding() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let cfg = cfg_with("owner@example.test");
    seed_first_owner(&pool, &cfg).await.expect("first boot");

    sqlx::query("UPDATE users SET deleted_at = now()")
        .execute(&pool)
        .await
        .unwrap();

    seed_first_owner(&pool, &cfg)
        .await
        .expect("boot after wipe");
    assert_eq!(
        link_count(&pool).await,
        1,
        "a deleted owner must not re-arm seeding"
    );

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn existing_unrelated_user_blocks_seeding() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    users::create_invited_user(&pool, "someone@example.test", None)
        .await
        .expect("pre-existing user");

    seed_first_owner(&pool, &cfg_with("owner@example.test"))
        .await
        .expect("seed");

    assert_eq!(user_count(&pool).await, 1);
    assert_eq!(link_count(&pool).await, 0);

    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn empty_email_disables_seeding() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };

    seed_first_owner(&pool, &cfg_with("  "))
        .await
        .expect("seed");

    assert_eq!(user_count(&pool).await, 0);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn malformed_email_fails_boot() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };

    let err = seed_first_owner(&pool, &cfg_with("not-an-email"))
        .await
        .expect_err("must not seed an unreachable account");
    assert!(err.to_string().contains("not an email address"), "{err}");

    assert_eq!(user_count(&pool).await, 0);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn bad_config_is_inert_once_the_instance_is_claimed() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    seed_first_owner(&pool, &cfg_with("owner@example.test"))
        .await
        .expect("first boot");

    // A stale or corrupted value must not take down an already-claimed instance.
    seed_first_owner(&pool, &cfg_with("not-an-email"))
        .await
        .expect("malformed email after claiming");

    let mut no_magic = cfg_with("owner@example.test");
    no_magic.auth.enabled_methods.retain(|m| m != "magic_link");
    seed_first_owner(&pool, &no_magic)
        .await
        .expect("magic link off after claiming");

    assert_eq!(user_count(&pool).await, 1);
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn magic_link_disabled_fails_boot() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let mut cfg = cfg_with("owner@example.test");
    cfg.auth.enabled_methods.retain(|m| m != "magic_link");

    let err = seed_first_owner(&pool, &cfg)
        .await
        .expect_err("no way to hand back a sign-in link");
    assert!(err.to_string().contains("magic_link"), "{err}");

    assert_eq!(user_count(&pool).await, 0, "must fail before writing");
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn seeded_org_lands_on_team_without_configuration() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let cfg = cfg_with("owner@example.test");

    seed_first_owner(&pool, &cfg).await.expect("seed");

    let plan: String = sqlx::query_scalar(
        "SELECT a.plan_id FROM accounts a JOIN organizations o ON o.account_id = a.id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        plan, "team",
        "self-hosted install stayed on the shared-platform plan"
    );
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn opting_back_to_free_leaves_the_schema_default_alone() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let mut cfg = cfg_with("owner@example.test");
    cfg.quotas.default_plan = "free".to_string();

    seed_first_owner(&pool, &cfg).await.expect("seed");

    let plan: String = sqlx::query_scalar(
        "SELECT a.plan_id FROM accounts a JOIN organizations o ON o.account_id = a.id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(plan, "free", "an explicit opt-out must be honoured");
    common::drop_test_db(&name).await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn unknown_default_plan_fails_boot_by_name() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let mut cfg = cfg_with("owner@example.test");
    cfg.quotas.default_plan = "enterprise".to_string();

    let err = seed_first_owner(&pool, &cfg)
        .await
        .expect_err("unknown plan must not boot");
    assert!(err.to_string().contains("enterprise"), "{err}");

    // Half-seeding is worse than not seeding: the next boot sees a user, skips
    // seeding, and the instance can never hand back a sign-in link again.
    assert_eq!(user_count(&pool).await, 0, "must fail before writing");
    assert_eq!(link_count(&pool).await, 0, "must fail before writing");
    common::drop_test_db(&name).await;
}

/// The scenario that upgrading to the roomier default actually creates: an
/// install claimed under the old default restarts under the new one.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn an_already_claimed_org_is_never_moved_by_a_later_default() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let mut old = cfg_with("owner@example.test");
    old.quotas.default_plan = "free".to_string();
    seed_first_owner(&pool, &old).await.expect("first boot");

    seed_first_owner(&pool, &cfg_with("owner@example.test"))
        .await
        .expect("restart on the new default");

    let plan: String = sqlx::query_scalar(
        "SELECT a.plan_id FROM accounts a JOIN organizations o ON o.account_id = a.id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(plan, "free", "a claimed instance was re-planned on restart");
    common::drop_test_db(&name).await;
}

/// Guards the join the app actually reads, not just the column the seed wrote.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn seeded_org_gets_team_limits_through_the_quota_service() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let cfg = cfg_with("owner@example.test");
    seed_first_owner(&pool, &cfg).await.expect("seed");
    let org = org_id(&pool).await;

    let plan = QuotaService::new(&cfg, Some(pool.clone()))
        .limit_for_org(org)
        .await
        .expect("effective plan");

    assert_eq!(plan.id, "team");
    assert_eq!(plan.max_targets, 150);
    assert_eq!(plan.min_check_interval_secs, 30);
    common::drop_test_db(&name).await;
}

/// The config default names a plan id no migration is obliged to keep. If the
/// seed ever drops or renames it, every self-host boot fails; catch it here.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_default_plan_is_seeded_by_the_migrations() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let wanted = AppConfig::load().expect("config").quotas.default_plan;

    let known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM plans WHERE id = $1)")
        .bind(&wanted)
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(known, "quotas.default_plan {wanted:?} is not seeded");
    common::drop_test_db(&name).await;
}

/// Hosted signups must be untouched by the self-host default.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn signup_orgs_ignore_the_boot_seeding_default() {
    let Some((pool, name)) = fresh_pool().await else {
        return;
    };
    let (user, _) = users::create_invited_user(&pool, "signup@example.test", None)
        .await
        .expect("user");

    let mut tx = pool.begin().await.unwrap();
    let org = orgs_store::create_signup_org_with_owner_in_tx(&mut tx, user, "signup-co", "Signup")
        .await
        .expect("signup org")
        .expect("slug free");
    tx.commit().await.unwrap();

    let plan: String = sqlx::query_scalar(
        "SELECT a.plan_id FROM accounts a \
         JOIN organizations o ON o.account_id = a.id WHERE o.id = $1",
    )
    .bind(org.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        plan == "founding" || plan == "free",
        "signup landed on {plan}, so the self-host default leaked into hosted"
    );
    common::drop_test_db(&name).await;
}

/// The operator CLI shares the org-creation helper with boot seeding, but it is
/// also how a hosted operator makes staff accounts — it must not hand out the
/// self-host plan.
#[tokio::test]
#[ignore = "requires DATABASE_URL"]
async fn the_operator_cli_does_not_apply_the_seeding_default() {
    let Some((db, name)) = common::fresh_test_db("bootstrap_cli").await else {
        return;
    };
    let pool = common::open_test_pool(&db).await;
    MIGRATOR.run(&pool).await.unwrap();

    let mut cfg = cfg_with("");
    cfg.storage.postgres.url = db.clone();
    assert_eq!(cfg.quotas.default_plan, "team", "guard the premise");

    uptimepage::bootstrap::run_owner(
        &cfg,
        &uptimepage::bootstrap::BootstrapArgs {
            email: "staff@example.test".to_string(),
            org_name: "Staff".to_string(),
        },
    )
    .await
    .expect("cli bootstrap");

    let plan: String = sqlx::query_scalar(
        "SELECT a.plan_id FROM accounts a JOIN organizations o ON o.account_id = a.id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(plan, "free", "the CLI inherited the boot-seeding default");
    common::drop_test_db(&name).await;
}
