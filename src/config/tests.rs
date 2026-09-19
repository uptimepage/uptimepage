use super::runtime::default_user_agent;
use super::*;
use config::FileFormat;

/// Self-hosters get the roomiest plan without configuring anything, so the
/// default is the whole feature; a revert to `free` would be silent.
#[test]
fn boot_seeding_defaults_to_the_largest_plan() {
    // The `Default` impl, not the merged config: a stray
    // UPTIMEPAGE_QUOTAS__DEFAULT_PLAN in the shell would answer for it.
    assert_eq!(QuotasConfig::default().default_plan, "team");
}

/// An override naming one quota key must not blank the others.
#[test]
fn a_partial_quotas_table_keeps_the_other_defaults() {
    let partial: QuotasConfig = Config::builder()
        .add_source(File::from_str("default_plan = 'free'", FileFormat::Toml))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    assert_eq!(partial.default_plan, "free");
    assert_eq!(partial.plan_cache_ttl_secs, 300);
}

/// The bot page spells the version out by hand, so a stale sample teaches
/// site owners to allowlist a string our probes no longer send.
#[test]
fn the_bot_page_quotes_the_user_agent_we_actually_send() {
    assert!(
        include_str!("../../docs/legal/bot.md").contains(&default_user_agent()),
        "docs/legal/bot.md does not quote {:?}",
        default_user_agent()
    );
}

fn scheduler_from(toml: &str) -> SchedulerConfig {
    Config::builder()
        .add_source(File::from_str(toml, FileFormat::Toml))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap()
}

#[test]
fn scheduler_enabled_defaults_true_and_parses_false() {
    assert!(scheduler_from("target_refresh_interval_secs = 30").enabled);
    assert!(!scheduler_from("enabled = false\ntarget_refresh_interval_secs = 30").enabled);
}

#[test]
fn help_form_stays_off_until_an_address_is_configured() {
    let mut cfg = TransactionalEmailConfig::default();
    assert!(!cfg.support_enabled(), "default ships with no address");
    cfg.support_address = "   ".into();
    assert!(!cfg.support_enabled(), "whitespace is not an address");
    cfg.support_address = "hello@example.test".into();
    assert!(cfg.support_enabled());
}

#[test]
fn every_purge_window_the_nightly_job_binds_rejects_zero() {
    for set_zero in [
        (|c: &mut AppConfig| c.retention.login_attempts_days = 0) as fn(&mut AppConfig),
        |c: &mut AppConfig| c.retention.quota_events_days = 0,
        |c: &mut AppConfig| c.retention.audit_log_days = 0,
        |c: &mut AppConfig| c.retention.mcp_audit_days = 0,
        |c: &mut AppConfig| c.auth.session.idle_timeout_days = 0,
        |c: &mut AppConfig| c.tenancy.deletion_grace_period_days = 0,
    ] {
        let mut cfg = AppConfig::load().expect("load");
        assert!(
            cfg.validate_quotas_and_limits().is_ok(),
            "defaults are sane"
        );
        set_zero(&mut cfg);
        assert!(
            cfg.validate_quotas_and_limits().is_err(),
            "a zero purge window must not boot"
        );
    }
}

#[test]
fn reconcile_window_must_outlast_a_tick_or_the_scan_never_matches() {
    let mut cfg = AppConfig::load().expect("load");
    cfg.escalation.tick_interval_secs = 15;
    cfg.escalation.reconcile_window_secs = 15;
    assert!(cfg.validate_quotas_and_limits().is_err());
    cfg.escalation.reconcile_window_secs = 16;
    assert!(cfg.validate_quotas_and_limits().is_ok());
}

fn agent_cfg(url: &str) -> AppConfig {
    let mut cfg = AppConfig::load().expect("load");
    cfg.agent.enabled = true;
    cfg.agent.control_plane_url = url.to_string();
    cfg.agent.region = "eu-helsinki".into();
    cfg.agent.token = SecretString::from("agent-token".to_string());
    cfg
}

#[test]
fn agent_https_control_plane_passes() {
    agent_cfg("https://app.example.com")
        .validate_runtime()
        .expect("https passes");
}

#[test]
fn agent_cleartext_control_plane_rejected() {
    let err = agent_cfg("http://app.example.com")
        .validate_runtime()
        .expect_err("cleartext rejected");
    assert!(err.to_string().contains("https"), "{err}");
}

#[test]
fn agent_cleartext_allowed_with_private_targets_optin() {
    let mut cfg = agent_cfg("http://127.0.0.1:8080");
    cfg.security.allow_private_targets = true;
    cfg.validate_runtime()
        .expect("cleartext ok when private opted in");
}

fn storage_cfg() -> AppConfig {
    let mut cfg = AppConfig::load().expect("load");
    cfg.storage.allow_default_credentials = false;
    cfg.storage.postgres.url = "postgres://monitor:monitor@localhost:5432/monitor".into();
    cfg.storage.clickhouse.password = SecretString::from("monitor".to_string());
    cfg
}

#[test]
fn shipped_postgres_credentials_rejected() {
    let err = storage_cfg()
        .validate_storage()
        .expect_err("shipped pg credentials rejected");
    assert!(err.to_string().contains("storage.postgres.url"), "{err}");
}

#[test]
fn shipped_clickhouse_password_rejected() {
    let mut cfg = storage_cfg();
    cfg.storage.postgres.url = "postgres://monitor:s3cret@localhost:5432/monitor".into();
    let err = cfg
        .validate_storage()
        .expect_err("shipped ch password rejected");
    assert!(
        err.to_string().contains("storage.clickhouse.password"),
        "{err}"
    );
}

#[test]
fn empty_clickhouse_password_rejected() {
    let mut cfg = storage_cfg();
    cfg.storage.postgres.url = "postgres://monitor:s3cret@localhost:5432/monitor".into();
    cfg.storage.clickhouse.password = SecretString::from(String::new());
    cfg.validate_storage()
        .expect_err("empty ch password rejected");
}

#[test]
fn overridden_credentials_pass() {
    let mut cfg = storage_cfg();
    cfg.storage.postgres.url = "postgres://monitor:s3cret@localhost:5432/monitor".into();
    cfg.storage.clickhouse.password = SecretString::from("s3cret".to_string());
    cfg.validate_storage().expect("overridden credentials pass");
}

#[test]
fn opt_in_allows_shipped_credentials() {
    let mut cfg = storage_cfg();
    cfg.storage.allow_default_credentials = true;
    cfg.validate_storage().expect("opt-in bypasses the guard");
}

#[test]
fn bootstrap_disabled_by_default_and_org_name_survives_a_partial_table() {
    let loaded = AppConfig::load().expect("load");
    assert!(loaded.bootstrap.email.is_empty());
    assert_eq!(loaded.bootstrap.org_name, "My Org");

    // An override file naming only `email` must not blank the org name.
    let partial: BootstrapConfig = Config::builder()
        .add_source(File::from_str("email = 'a@b.test'", FileFormat::Toml))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    assert_eq!(partial.org_name, "My Org");
}

/// `[auth.microsoft]` flattens the shared client fields onto the section, so
/// TOML and env must both land on them rather than on a nested `client` table.
#[test]
fn microsoft_section_reads_the_flattened_client_fields() {
    let cfg: MicrosoftOauthConfig = Config::builder()
        .add_source(File::from_str(
            r#"
client_id = "cid"
client_secret = "sec"
redirect_url = "https://app.example.test/auth/microsoft/callback"
tenant = "organizations"
"#,
            FileFormat::Toml,
        ))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    assert!(cfg.client.is_configured());
    assert_eq!(cfg.tenant, "organizations");
}

#[test]
fn microsoft_env_keys_reach_the_flattened_client() {
    let env = std::collections::HashMap::from([
        (
            "UPTIMEPAGE_AUTH__MICROSOFT__CLIENT_ID".to_string(),
            "cid".to_string(),
        ),
        (
            "UPTIMEPAGE_AUTH__MICROSOFT__CLIENT_SECRET".to_string(),
            "sec".to_string(),
        ),
        (
            "UPTIMEPAGE_AUTH__MICROSOFT__REDIRECT_URL".to_string(),
            "https://app.example.test/auth/microsoft/callback".to_string(),
        ),
        (
            "UPTIMEPAGE_AUTH__MICROSOFT__TENANT".to_string(),
            "organizations".to_string(),
        ),
    ]);
    let cfg: AppConfig = Config::builder()
        .add_source(File::with_name(DEFAULT_CONFIG_PATH))
        .add_source(
            Environment::with_prefix(ENV_PREFIX)
                .prefix_separator("_")
                .separator(ENV_SEPARATOR)
                .try_parsing(true)
                .source(Some(env)),
        )
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    assert!(cfg.auth.microsoft.client.is_configured());
    assert_eq!(cfg.auth.microsoft.tenant, "organizations");
    assert!(cfg.auth.microsoft_login_enabled());
}

/// `[auth.gitlab]` flattens the shared client fields onto the section, so TOML
/// and env must both land on them rather than on a nested `client` table.
#[test]
fn gitlab_section_reads_the_flattened_client_fields() {
    let cfg: GitlabOauthConfig = Config::builder()
        .add_source(File::from_str(
            r#"
client_id = "cid"
client_secret = "sec"
redirect_url = "https://app.example.test/auth/gitlab/callback"
base_url = "https://git.corp.test"
"#,
            FileFormat::Toml,
        ))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    assert!(cfg.client.is_configured());
    assert_eq!(cfg.base_url, "https://git.corp.test");
}

#[test]
fn gitlab_env_keys_reach_the_flattened_client() {
    let env = std::collections::HashMap::from([
        (
            "UPTIMEPAGE_AUTH__GITLAB__CLIENT_ID".to_string(),
            "cid".to_string(),
        ),
        (
            "UPTIMEPAGE_AUTH__GITLAB__CLIENT_SECRET".to_string(),
            "sec".to_string(),
        ),
        (
            "UPTIMEPAGE_AUTH__GITLAB__REDIRECT_URL".to_string(),
            "https://app.example.test/auth/gitlab/callback".to_string(),
        ),
        (
            "UPTIMEPAGE_AUTH__GITLAB__BASE_URL".to_string(),
            "https://git.corp.test".to_string(),
        ),
    ]);
    let cfg: AppConfig = Config::builder()
        .add_source(File::with_name(DEFAULT_CONFIG_PATH))
        .add_source(
            Environment::with_prefix(ENV_PREFIX)
                .prefix_separator("_")
                .separator(ENV_SEPARATOR)
                .try_parsing(true)
                .source(Some(env)),
        )
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    assert!(cfg.auth.gitlab.client.is_configured());
    assert_eq!(cfg.auth.gitlab.base_url, "https://git.corp.test");
    assert!(cfg.auth.gitlab_login_enabled());
}

/// An unaddressable instance stops the boot rather than signing users in
/// against the wrong GitLab.
#[test]
fn an_unaddressable_gitlab_instance_fails_the_boot() {
    let mut cfg: AppConfig = Config::builder()
        .add_source(File::with_name(DEFAULT_CONFIG_PATH))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    cfg.auth.gitlab.client.client_id = "cid".into();
    cfg.auth.gitlab.client.client_secret = "sec".to_string().into();
    cfg.auth.gitlab.client.redirect_url = "https://app.example.test/cb".into();

    cfg.auth.gitlab.base_url = "https://git.corp.test".into();
    assert!(cfg.validate_gitlab_oauth().is_ok());

    cfg.auth.gitlab.base_url = "http://git.corp.test".into();
    assert!(cfg.validate_gitlab_oauth().is_err());

    cfg.auth.gitlab.client.client_id = String::new();
    assert!(cfg.validate_gitlab_oauth().is_ok());
}

/// A mistyped single-tenant lock stops the boot rather than widening.
#[test]
fn an_unaddressable_microsoft_tenant_fails_the_boot() {
    let mut cfg: AppConfig = Config::builder()
        .add_source(File::with_name(DEFAULT_CONFIG_PATH))
        .build()
        .unwrap()
        .try_deserialize()
        .unwrap();
    cfg.auth.microsoft.client.client_id = "cid".into();
    cfg.auth.microsoft.client.client_secret = "sec".to_string().into();
    cfg.auth.microsoft.client.redirect_url = "https://app.example.test/cb".into();

    cfg.auth.microsoft.tenant = "contoso.com".into();
    assert!(cfg.validate_microsoft_oauth().is_ok());

    cfg.auth.microsoft.tenant = "contoso.com/".into();
    assert!(cfg.validate_microsoft_oauth().is_err());

    cfg.auth.microsoft.client.client_id = String::new();
    assert!(cfg.validate_microsoft_oauth().is_ok());
}

/// The field defaults to `true`, so a misplaced or misspelled key loads as if
/// it were there. Only reading the parsed tree by path catches that.
#[test]
fn the_shipped_config_puts_open_signup_where_auth_reads_it() {
    let shipped = include_str!("../../config/default.toml");
    let built = Config::builder()
        .add_source(File::from_str(shipped, FileFormat::Toml))
        .build()
        .expect("shipped config builds");
    assert_eq!(
        built.get_bool("auth.open_signup").ok(),
        Some(true),
        "auth.open_signup is missing from the shipped config, or sits under another table"
    );

    let cfg: crate::config::AppConfig = built
        .try_deserialize()
        .expect("shipped config deserialises");
    assert!(cfg.auth.open_signup, "and it reaches the field");
}

fn ms(tenant: &str) -> MicrosoftOauthConfig {
    MicrosoftOauthConfig {
        tenant: tenant.into(),
        ..MicrosoftOauthConfig::default()
    }
}

fn gl(base_url: &str) -> GitlabOauthConfig {
    GitlabOauthConfig {
        base_url: base_url.into(),
        ..GitlabOauthConfig::default()
    }
}

#[test]
fn only_addressable_tenants_pass_validation() {
    for ok in [
        "common",
        "organizations",
        "consumers",
        "72f988bf-86f1-41af-91ab-2d7cd011db47",
        "contoso.com",
    ] {
        assert!(ms(ok).tenant_is_valid(), "{ok} should be accepted");
    }
    // Each of these would otherwise widen a single-tenant lock to `common`.
    for bad in [
        "",
        "..",
        ".",
        "../../evil",
        "contoso.com/",
        "{72f988bf}",
        "a b",
    ] {
        assert!(!ms(bad).tenant_is_valid(), "{bad:?} should be rejected");
    }
}

#[test]
fn only_an_https_origin_passes_validation() {
    for ok in [
        "https://gitlab.com",
        "https://gitlab.com/",
        "https://git.corp.test:8443",
        "https://git.corp.test/gitlab",
    ] {
        assert!(gl(ok).base_url_is_valid(), "{ok} should be accepted");
    }
    for bad in [
        "",
        "gitlab.com",
        "http://gitlab.com",
        "https://",
        "https://user:pw@gitlab.com",
        "https://gitlab.com?a=1",
        "https://gitlab.com#f",
    ] {
        assert!(!gl(bad).base_url_is_valid(), "{bad:?} should be rejected");
    }
}
