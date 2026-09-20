//! Seeds the first owner on a fresh instance. Sign-in providers (OAuth,
//! magic-link) all assume an account already exists or an inviter to carry a
//! token; this creates that initial account + org out of band. Two entry
//! points: the operator CLI, which mints a full-access API token and needs no
//! provider at all, and boot-time seeding for installs with no terminal, which
//! hands back a magic-link sign-in URL and so requires that method enabled.

use anyhow::Context;
use sqlx::PgPool;

use crate::auth::scope::ScopeSet;
use crate::auth::url::token_link;
use crate::auth::{api_tokens, magic_link};
use crate::config::AppConfig;
use crate::domain::{OrgId, UserId, generate_signup_slug};
use crate::error::{AppError, Result};
use crate::quotas::QuotaService;
use crate::storage::notification_channels::PgNotificationChannelStore;
use crate::storage::{PostgresTargetStore, orgs, users};

const USAGE: &str = "usage: uptimepage bootstrap-owner --email <addr> [--org-name <name>]";

/// Parsed `bootstrap-owner` arguments.
pub struct BootstrapArgs {
    pub email: String,
    pub org_name: String,
}

/// Parses the flags following the `bootstrap-owner` subcommand. Accepts both
/// `--flag value` and `--flag=value`.
pub fn parse_args(raw: impl IntoIterator<Item = String>) -> Result<BootstrapArgs> {
    let args: Vec<String> = raw.into_iter().collect();
    let mut email = None;
    let mut org_name = None;
    let mut i = 0;
    while i < args.len() {
        let (key, inline) = match args[i].strip_prefix("--") {
            Some(rest) => match rest.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (rest.to_string(), None),
            },
            None => return Err(bad_usage(format!("unexpected argument {:?}", args[i]))),
        };
        let value = match inline {
            Some(v) => v,
            None => {
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| bad_usage(format!("--{key} needs a value")))?
            }
        };
        match key.as_str() {
            "email" => email = Some(value),
            "org-name" => org_name = Some(value),
            other => return Err(bad_usage(format!("unknown flag --{other}"))),
        }
        i += 1;
    }

    let email = email.ok_or_else(|| bad_usage("--email is required"))?;
    if !email.contains('@') {
        return Err(bad_usage(format!("{email:?} is not an email address")));
    }
    Ok(BootstrapArgs {
        email,
        org_name: org_name.unwrap_or_else(|| "My Org".to_string()),
    })
}

fn bad_usage(msg: impl std::fmt::Display) -> AppError {
    AppError::Other(anyhow::anyhow!("{msg}\n{USAGE}"))
}

/// Creates the owner account and its org, reusing either if already present.
async fn ensure_owner_org(
    pool: &PgPool,
    cfg: &AppConfig,
    email: &str,
    org_name: &str,
) -> Result<(UserId, OrgId)> {
    // The operator's own address from config, not a stranger's from a form.
    let (user, _) = users::create_invited_user(pool, email, None).await?;

    let org = match orgs::oldest_membership_for_user(pool, user).await? {
        Some(existing) => existing,
        None => {
            let mut created = None;
            for _ in 0..8 {
                let slug = generate_signup_slug();
                if let Some(org) = orgs::create_org_with_owner(pool, user, &slug, org_name).await? {
                    created = Some(org.id);
                    break;
                }
            }
            created.context("bootstrap: could not allocate a unique org slug")?
        }
    };
    // A cipher that fails to load is not fatal here: the owner row is already
    // committed, and a channel is fixable later.
    match crate::security::Cipher::from_config(&cfg.security) {
        Ok(cipher) => {
            let store = PgNotificationChannelStore::new(pool.clone(), cipher);
            let quotas = QuotaService::new(cfg, Some(pool.clone()));
            crate::channels::seed_owner_email(&store, &quotas, &cfg.email, org, user, email).await;
        }
        Err(e) => tracing::warn!(error = %e, "seeding the owner's email alert channel skipped"),
    }
    Ok((user, org))
}

/// Resolves `quotas.default_plan` against the `plans` table.
///
/// Called before the seed writes anything: a run that dies partway leaves a
/// user row behind, and the next boot then treats the instance as claimed and
/// never issues a sign-in link again.
async fn checked_default_plan<'a>(pool: &PgPool, cfg: &'a AppConfig) -> Result<Option<&'a str>> {
    let plan = cfg.quotas.default_plan.trim();
    if plan.is_empty() {
        return Ok(None);
    }
    let (known,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM plans WHERE id = $1)")
        .bind(plan)
        .fetch_one(pool)
        .await
        .context("bootstrap: look up quotas.default_plan")?;
    if !known {
        return Err(AppError::Other(anyhow::anyhow!(
            "quotas.default_plan {plan:?} is not a plan this instance has"
        )));
    }
    Ok(Some(plan))
}

/// Seeds the owner named by `bootstrap.email` when the instance has no users.
/// Hands back only a sign-in link, never a token: this lands in the log stream,
/// so it has to expire and be single-use.
pub async fn seed_first_owner(pool: &PgPool, cfg: &AppConfig) -> Result<()> {
    let email = cfg.bootstrap.email.trim();
    if email.is_empty() {
        return Ok(());
    }
    // Claimed first, so a stale value left in config can never brick a later boot.
    if users::any_exist(pool).await? {
        return Ok(());
    }
    if !email.contains('@') {
        return Err(AppError::Other(anyhow::anyhow!(
            "bootstrap.email {email:?} is not an email address"
        )));
    }
    if !cfg.auth.magic_link_enabled() {
        return Err(AppError::Other(anyhow::anyhow!(
            "bootstrap.email needs magic_link in auth.enabled_methods to hand back a sign-in link"
        )));
    }

    let plan = checked_default_plan(pool, cfg).await?;

    // Whoever installed the app may not read the logs for hours.
    const FIRST_RUN_LINK_MINUTES: u32 = 24 * 60;
    // Link before account: failing here leaves no user row, so the next boot retries.
    let link = magic_link::create(
        pool,
        magic_link::NewMagicLink {
            email,
            expiry_minutes: FIRST_RUN_LINK_MINUTES,
            ..Default::default()
        },
    )
    .await?;

    let (_, org) = ensure_owner_org(pool, cfg, email, &cfg.bootstrap.org_name).await?;
    // Reached only when the instance had no users, so this org is always the one
    // just created; an org the operator re-planned later is never touched.
    if let Some(plan) = plan {
        sqlx::query(
            "UPDATE accounts /* SAFE: bound to the account behind the org seeding just created */ \
             SET plan_id = $1, updated_at = now() \
             WHERE id = (SELECT account_id FROM organizations WHERE id = $2)",
        )
        .bind(plan)
        .bind(org.0)
        .execute(pool)
        .await
        .context("bootstrap: apply quotas.default_plan")?;
    }
    let slug = orgs::get_org(pool, org)
        .await?
        .map(|o| o.slug)
        .unwrap_or_default();

    let url = token_link(
        &cfg.auth.public_base_url,
        "/auth/magic-link/verify",
        &link.token,
    );
    tracing::warn!(
        // SAFE: the operator's own config named this owner, and the link is the
        // only channel a terminal-less first boot has. Single-use and expiring,
        // never a token; the email itself stays out, it adds nothing here.
        org = %slug,
        sign_in_url = %url,
        expires_in_hours = FIRST_RUN_LINK_MINUTES / 60,
        "seeded the first owner; open sign_in_url to claim this instance"
    );
    Ok(())
}

/// Idempotent on the account and org; each run mints a new full-access token.
pub async fn run_owner(cfg: &AppConfig, args: &BootstrapArgs) -> Result<()> {
    let pool = PostgresTargetStore::connect_pool(&cfg.storage.postgres).await?;

    let (user, org) = ensure_owner_org(&pool, cfg, &args.email, &args.org_name).await?;

    let token = api_tokens::create(
        &pool,
        user,
        "bootstrap",
        &ScopeSet::full_access(),
        Some(org),
        None,
        cfg.auth.api_tokens.prefix_visible_chars as usize,
        i64::MAX,
    )
    .await?;

    let slug = orgs::get_org(&pool, org)
        .await?
        .map(|o| o.slug)
        .unwrap_or_default();

    // A fresh instance has no configured OAuth provider and the dev mailer
    // redacts links, so mint a one-time sign-in URL the operator can open
    // directly. The verify route logs the existing owner straight in.
    let web_login = if cfg.auth.magic_link_enabled() {
        let expiry = cfg.auth.magic_link.expiry_minutes;
        let link = magic_link::create(
            &pool,
            magic_link::NewMagicLink {
                email: &args.email,
                expiry_minutes: expiry,
                ..Default::default()
            },
        )
        .await?;
        Some((
            token_link(
                &cfg.auth.public_base_url,
                "/auth/magic-link/verify",
                &link.token,
            ),
            expiry,
        ))
    } else {
        None
    };

    // stdout, shown once: the credentials must stay copyable and out of the logs.
    println!();
    println!("Owner account ready.");
    println!("  email: {}", args.email);
    println!("  org:   {slug}");
    println!();
    match &web_login {
        Some((url, expiry)) => {
            println!("Web dashboard — open this link to sign in (expires in {expiry} min):");
            println!("  {url}");
        }
        None => {
            println!("Web dashboard — enable a sign-in provider (GitHub/Google OAuth or");
            println!("  magic_link in auth.enabled_methods) to sign in.");
        }
    }
    println!();
    println!("API access — full-access token (shown once):");
    println!("  {}", token.token);
    println!();
    println!("  curl -H 'authorization: Bearer {}' \\", token.token);
    println!("       -H 'x-uptimepage-org: {slug}' \\");
    println!("       http://127.0.0.1:8080/api/v1/targets");
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_space_and_equals_forms() {
        let a = parse_args(args(&["--email", "a@b.com", "--org-name", "Acme"])).unwrap();
        assert_eq!(a.email, "a@b.com");
        assert_eq!(a.org_name, "Acme");

        let b = parse_args(args(&["--email=a@b.com", "--org-name=Acme Inc"])).unwrap();
        assert_eq!(b.email, "a@b.com");
        assert_eq!(b.org_name, "Acme Inc");
    }

    #[test]
    fn org_name_defaults() {
        let a = parse_args(args(&["--email", "a@b.com"])).unwrap();
        assert_eq!(a.org_name, "My Org");
    }

    #[test]
    fn rejects_missing_or_malformed_email() {
        assert!(parse_args(args(&["--org-name", "Acme"])).is_err());
        assert!(parse_args(args(&["--email", "not-an-email"])).is_err());
        assert!(parse_args(args(&["--email"])).is_err());
    }

    #[test]
    fn rejects_unknown_flag_and_stray_arg() {
        assert!(parse_args(args(&["--email", "a@b.com", "--bogus", "x"])).is_err());
        assert!(parse_args(args(&["positional"])).is_err());
    }
}
