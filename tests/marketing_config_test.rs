//! Boot invariants for the marketing config. Each test mutates a known
//! valid baseline to violate exactly one invariant and asserts the
//! validator returns a clean error.

use uptimepage::config::AppConfig;

fn valid_cfg() -> AppConfig {
    let mut cfg = AppConfig::load().expect("load");
    cfg.tenancy.subdomain_public_routes = true;
    cfg.tenancy.path_based_public_routes = false;
    cfg.public_status.base_domain = "example.com".into();
    cfg.auth.session.cookie_domain = String::new();
    cfg.marketing.enabled = true;
    cfg.marketing.canonical_origin = "https://example.com".into();
    cfg.marketing.app_url = "https://app.example.com".into();
    cfg.marketing.reserved_subdomains = vec!["www".into(), "app".into()];
    cfg
}

#[test]
fn valid_marketing_config_passes() {
    valid_cfg().validate_marketing().expect("valid passes");
}

#[test]
fn disabled_skips_all_checks() {
    // The whole rule set is gated on marketing.enabled — flip it off and
    // every other value can be junk without tripping the validator.
    let mut cfg = valid_cfg();
    cfg.marketing.enabled = false;
    cfg.marketing.canonical_origin = String::new();
    cfg.marketing.app_url = String::new();
    cfg.public_status.base_domain = String::new();
    cfg.validate_marketing().expect("disabled is permissive");
}

#[test]
fn empty_canonical_origin_rejected() {
    let mut cfg = valid_cfg();
    cfg.marketing.canonical_origin = String::new();
    let err = cfg.validate_marketing().expect_err("must reject");
    assert!(err.to_string().contains("marketing.canonical_origin"));
}

#[test]
fn http_canonical_origin_rejected() {
    let mut cfg = valid_cfg();
    cfg.marketing.canonical_origin = "http://example.com".into();
    let err = cfg.validate_marketing().expect_err("must reject");
    assert!(err.to_string().contains("https://"));
}

#[test]
fn trailing_slash_canonical_origin_rejected() {
    let mut cfg = valid_cfg();
    cfg.marketing.canonical_origin = "https://example.com/".into();
    let err = cfg.validate_marketing().expect_err("must reject");
    assert!(err.to_string().contains("trailing slash"));
}

#[test]
fn empty_app_url_rejected() {
    let mut cfg = valid_cfg();
    cfg.marketing.app_url = String::new();
    let err = cfg.validate_marketing().expect_err("must reject");
    assert!(err.to_string().contains("marketing.app_url"));
}

#[test]
fn empty_base_domain_rejected() {
    let mut cfg = valid_cfg();
    cfg.public_status.base_domain = String::new();
    let err = cfg.validate_marketing().expect_err("must reject");
    assert!(err.to_string().contains("public_status.base_domain"));
}

#[test]
fn single_label_base_domain_rejected() {
    let mut cfg = valid_cfg();
    cfg.public_status.base_domain = "local".into();
    let err = cfg.validate_marketing().expect_err("must reject");
    assert!(err.to_string().contains("public_status.base_domain"));
}

#[test]
fn non_reserved_subdomain_rejected() {
    // A made-up label that is not in `domain::reserved_slugs::RESERVED_SLUGS`
    // must be rejected — the marketing reserved set must be a strict
    // subset so the two lists cannot drift.
    let mut cfg = valid_cfg();
    cfg.marketing.reserved_subdomains = vec!["totally-not-reserved-xyz".into()];
    let err = cfg.validate_marketing().expect_err("must reject");
    assert!(err.to_string().contains("reserved_subdomains"));
}
