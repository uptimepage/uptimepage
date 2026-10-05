//! Live-Postgres contract for the corpus: whole-set swap and snapshot stamp.
//! Needs `DATABASE_URL`; migrations auto-apply on first connect, so point it at
//! a throwaway DB to also validate `052_email_policy`.
//!
//! One test, not several: the corpus is a global singleton, so two running
//! concurrently would each replace the set the other is reading.

use crate::common;

use std::collections::HashSet;

use uptimepage::config::EmailPolicyConfig;
use uptimepage::security::EmailPolicy;
use uptimepage::storage::disposable_domains;

use common::pg_pool_from_env;

fn set(domains: &[&str]) -> HashSet<String> {
    domains.iter().map(|d| (*d).to_string()).collect()
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn the_corpus_round_trips_and_swaps_whole() {
    let Some(pool) = pg_pool_from_env().await else {
        return;
    };

    // Empty to start: nothing stored, nothing claimed.
    disposable_domains::replace_all(&pool, &HashSet::new())
        .await
        .expect("clear");
    assert!(
        disposable_domains::load_all(&pool)
            .await
            .unwrap()
            .is_empty()
    );
    let stamp = disposable_domains::last_snapshot(&pool)
        .await
        .unwrap()
        .expect("snapshot written even for an empty set");
    assert_eq!(stamp.domain_count, 0);

    let first = set(&["mailinator.com", "tempmail.example", "guerrilla.example"]);
    disposable_domains::replace_all(&pool, &first)
        .await
        .expect("first write");
    assert_eq!(disposable_domains::load_all(&pool).await.unwrap(), first);
    assert_eq!(
        disposable_domains::last_snapshot(&pool)
            .await
            .unwrap()
            .unwrap()
            .domain_count,
        3
    );

    // Replace, not merge: a delisted domain has to actually leave, or every
    // false positive is forever.
    let second = set(&["mailinator.com", "newburner.example"]);
    disposable_domains::replace_all(&pool, &second)
        .await
        .expect("second write");
    let loaded = disposable_domains::load_all(&pool).await.unwrap();
    assert_eq!(loaded, second);
    assert!(!loaded.contains("tempmail.example"));

    // What the boot path does: read the stored set into the live policy.
    let policy = EmailPolicy::from_config(&EmailPolicyConfig {
        enabled: true,
        require_mx: false,
        ..Default::default()
    });
    policy.install(loaded);
    assert!(policy.disposable_domain("a@newburner.example").is_some());
    assert!(policy.disposable_domain("a@tempmail.example").is_none());

    let stamp = disposable_domains::last_snapshot(&pool).await.unwrap();
    assert_eq!(stamp.unwrap().domain_count, 2);

    // A bind-parameter ceiling would only show up at production scale.
    let big: HashSet<String> = (0..80_000).map(|i| format!("burner{i}.example")).collect();
    disposable_domains::replace_all(&pool, &big)
        .await
        .expect("bulk write");
    assert_eq!(
        disposable_domains::load_all(&pool).await.unwrap().len(),
        big.len()
    );

    disposable_domains::replace_all(&pool, &HashSet::new())
        .await
        .expect("cleanup");
}
