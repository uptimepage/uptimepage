//! Smoke tests against the real upstream lists and live DNS: an upstream that
//! changes format or stops being a domain list, and an MX branch that reads
//! real answers wrongly.
//!
//! CI runs ignored tests, so both no-op without `EMAIL_POLICY_LIVE_TEST=1`
//! rather than making every run depend on GitHub raw and public DNS.
//!
//! Run manually:
//!     EMAIL_POLICY_LIVE_TEST=1 \
//!         cargo test --test disposable_sources_live_test -- --ignored

mod common;

use std::collections::HashSet;

use uptimepage::config::{DnsConfig, EmailPolicyConfig};
use uptimepage::http_client::HickoryDnsResolver;
use uptimepage::jobs::disposable_refresh::{fetch_union, sanity_check};
use uptimepage::security::EmailPolicy;
use uptimepage::security::EmailRisk;

#[tokio::test]
#[ignore = "set EMAIL_POLICY_LIVE_TEST=1: fetches the live upstream lists"]
async fn the_configured_sources_still_parse_into_a_usable_corpus() {
    if std::env::var_os("EMAIL_POLICY_LIVE_TEST").is_none() {
        return;
    }
    let (http, _email) = common::build_test_outbound_and_email();
    let cfg = EmailPolicyConfig::default();

    let union = fetch_union(&http, &cfg.sources)
        .await
        .expect("at least one source answered");
    println!("union: {} domains", union.len());

    sanity_check(union.len(), None, &cfg).expect("a live fetch clears its own guards");

    // Carried by these lists for years: all three gone means the fetch
    // succeeded but this is no longer a domain list.
    let known = ["mailinator.com", "guerrillamail.com", "10minutemail.com"];
    let hits = known.iter().filter(|d| union.contains(**d)).count();
    assert!(
        hits >= 2,
        "only {hits} of {known:?} present — check the format"
    );

    // And the floor holds against whatever actually arrived today.
    let policy = EmailPolicy::from_config(&EmailPolicyConfig {
        require_mx: false,
        ..Default::default()
    });
    policy.install(union);
    for keep in [
        "a@gmail.com",
        "a@outlook.com",
        "a@icloud.com",
        "a@privaterelay.appleid.com",
        "a@addy.io",
        "a@duck.com",
        "a@proton.me",
        "a@ukr.net",
        "a@shop.co.uk",
    ] {
        assert!(
            policy.disposable_domain(keep).is_none(),
            "{keep} would have been refused"
        );
    }
    assert!(policy.disposable_domain("a@mailinator.com").is_some());
}

fn resolver() -> HickoryDnsResolver {
    HickoryDnsResolver::new(&DnsConfig {
        cache_size: 128,
        positive_ttl_secs: 300,
        negative_ttl_secs: 60,
        servers: vec!["1.1.1.1:53".into(), "8.8.8.8:53".into()],
    })
    .expect("resolver")
}

/// Domains chosen for stable answers, one per branch.
#[tokio::test]
#[ignore = "set EMAIL_POLICY_LIVE_TEST=1: queries public DNS"]
async fn the_mx_gate_reads_the_answers_it_claims_to() {
    if std::env::var_os("EMAIL_POLICY_LIVE_TEST").is_none() {
        return;
    }
    let policy = EmailPolicy::from_config(&EmailPolicyConfig::default());
    policy.install(HashSet::new());
    let r = resolver();

    // Ordinary MX records.
    assert_eq!(policy.assess("a@uptimepage.dev", r.inner()).await, None);
    // Preference 0 with a real exchange is a normal MX, not RFC 7505.
    assert_eq!(policy.assess("a@github.com", r.inner()).await, None);
    // RFC 7505 null MX: `0 .` is an explicit refusal to accept mail, and it
    // must not fall through to the implicit-A rule.
    assert_eq!(
        policy.assess("a@example.com", r.inner()).await,
        Some(EmailRisk::NoMx)
    );
    // NXDOMAIN under a reserved TLD: nothing to deliver to, ever.
    assert_eq!(
        policy
            .assess("a@nonexistent-domain-xyz-9f3.test", r.inner())
            .await,
        Some(EmailRisk::NoMx)
    );
}
