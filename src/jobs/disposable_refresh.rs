//! Keeps the disposable-email corpus current.
//!
//! Every failure here is a no-op, not an outage: a failed fetch, a shrunk list
//! and an implausible one all leave the previous corpus live. This is
//! third-party data on the signup path, so an upstream edit is an upstream
//! write to admission control — the guards below bound that by size,
//! `email_policy::NEVER_DISPOSABLE` bounds it by name.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use sqlx::PgPool;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::config::EmailPolicyConfig;
use crate::http_outbound::{OutboundHttpClient, get_text};
use crate::metric_names;
use crate::security::EmailPolicy;
use crate::storage::disposable_domains;
use crate::storage::locks::try_job;

const JOB: &str = "disposable_email_refresh";

/// Largest list in use is ~1.1 MB; this still refuses a source that has become
/// something else entirely.
const MAX_SOURCE_BYTES: usize = 16 << 20;

/// Run before the server accepts traffic, so a restart is never permissive.
pub async fn load_persisted(pool: &PgPool, policy: &EmailPolicy) {
    match disposable_domains::load_all(pool).await {
        Ok(domains) => {
            let n = policy.install(domains);
            tracing::info!(domains = n, "disposable email corpus loaded");
        }
        Err(err) => tracing::warn!(error = %err, "disposable email corpus load failed"),
    }
    publish_corpus_gauges(pool, policy).await;
}

/// Size from the live policy, not the stored `domain_count`: the floor is
/// applied on install, so the stored count is the raw union and overstates what
/// the gate acts on.
async fn publish_corpus_gauges(pool: &PgPool, policy: &EmailPolicy) {
    metrics::gauge!(metric_names::DISPOSABLE_CORPUS_DOMAINS).set(policy.loaded() as f64);
    let Ok(Some(snap)) = disposable_domains::last_snapshot(pool).await else {
        return;
    };
    metrics::gauge!(metric_names::DISPOSABLE_CORPUS_UPDATED)
        .set(snap.fetched_at.timestamp() as f64);
}

/// `None` when the feature is off, so no task and no outbound traffic exist.
pub fn spawn(
    pool: PgPool,
    http: OutboundHttpClient,
    cfg: Arc<EmailPolicyConfig>,
    policy: Arc<EmailPolicy>,
    shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }
    let every = cfg.refresh_interval();
    Some(tokio::spawn(async move {
        // The boot pass can spend a fetch, and SIGTERM must not wait on a slow
        // upstream to answer.
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick(&pool, &http, &cfg, &policy, every) => {}
        }
        let mut ticker = interval(every);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => tick(&pool, &http, &cfg, &policy, every).await,
            }
        }
    }))
}

/// A read failure counts as stale: refreshing a corpus we already have is
/// cheap, running without one is not.
async fn is_stale(pool: &PgPool, every: Duration) -> bool {
    match disposable_domains::last_snapshot(pool).await {
        Ok(Some(snap)) => {
            let age = Utc::now().signed_duration_since(snap.fetched_at);
            age.to_std().map(|a| a >= every).unwrap_or(false)
        }
        Ok(None) => true,
        Err(err) => {
            tracing::warn!(error = %err, "disposable snapshot read failed; refreshing");
            true
        }
    }
}

async fn tick(
    pool: &PgPool,
    http: &OutboundHttpClient,
    cfg: &EmailPolicyConfig,
    policy: &EmailPolicy,
    every: Duration,
) {
    try_job(pool, JOB, || async {
        // Under the lock, not at schedule time: the lock alone only serialises
        // concurrent ticks, so a replica booting an hour later would otherwise
        // repeat the fetch and the whole-table rewrite in the same interval.
        if !is_stale(pool, every).await {
            return;
        }
        match refresh(pool, http, cfg, policy).await {
            Ok(n) => {
                tracing::info!(domains = n, "disposable email corpus refreshed");
                publish_corpus_gauges(pool, policy).await;
            }
            Err(err) => tracing::warn!(error = %err, "disposable email refresh skipped"),
        }
    })
    .await;
}

/// `Err` only when no source answered: one being down must not discard the
/// others, and the union still has to clear the sanity guards.
pub async fn fetch_union(
    http: &OutboundHttpClient,
    sources: &[String],
) -> anyhow::Result<HashSet<String>> {
    let mut union: HashSet<String> = HashSet::new();
    let mut fetched = 0usize;
    for raw in sources {
        let url = match Url::parse(raw) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(source = %raw, error = %e, "disposable source is not a URL");
                continue;
            }
        };
        match get_text(http, &url, MAX_SOURCE_BYTES).await {
            Ok(body) => {
                let before = union.len();
                union.extend(parse(&body));
                fetched += 1;
                tracing::debug!(
                    // SAFE: operator-configured list URL, not data-subject data.
                    source = %url,
                    added = union.len() - before,
                    "disposable source read"
                );
            }
            Err(e) => tracing::warn!(
                // SAFE: operator-configured list URL, not data-subject data.
                source = %url,
                error = %e,
                "disposable source fetch failed"
            ),
        }
    }
    if fetched == 0 {
        anyhow::bail!("no source could be fetched");
    }
    Ok(union)
}

/// One refresh. `Err` means the live corpus was left untouched.
pub async fn refresh(
    pool: &PgPool,
    http: &OutboundHttpClient,
    cfg: &EmailPolicyConfig,
    policy: &EmailPolicy,
) -> anyhow::Result<usize> {
    let union = fetch_union(http, &cfg.sources).await?;

    // Not `.ok().flatten()`: that would skip the shrink guard on a read failure
    // and let a truncated upstream through. Cannot compare, cannot replace.
    let previous = disposable_domains::last_snapshot(pool)
        .await
        .context("reading the stored snapshot to compare against")?
        .map(|s| s.domain_count.max(0) as usize);
    sanity_check(union.len(), previous, cfg)?;

    disposable_domains::replace_all(pool, &union).await?;
    Ok(policy.install(union))
}

/// Stray tokens are dropped rather than failing the parse: these lists carry
/// the occasional one, and it is not worth a stale corpus.
fn parse(body: &str) -> impl Iterator<Item = String> + '_ {
    body.lines().filter_map(|line| {
        let line = line.split('#').next().unwrap_or(line);
        let d = line.trim().trim_end_matches('.').to_ascii_lowercase();
        plausible_domain(&d).then_some(d)
    })
}

/// The two-label floor is load-bearing: a single label would be a TLD, and the
/// parent walk would blanket everything under it.
fn plausible_domain(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 253
        && d.split('.').count() >= 2
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// The shrink guard is the one that catches a truncated or partially-published
/// upstream, which arrives as a perfectly well-formed short file.
pub fn sanity_check(
    incoming: usize,
    previous: Option<usize>,
    cfg: &EmailPolicyConfig,
) -> anyhow::Result<()> {
    if incoming < cfg.min_domains {
        anyhow::bail!("{incoming} domains is below the {} floor", cfg.min_domains);
    }
    if incoming > cfg.max_domains {
        anyhow::bail!(
            "{incoming} domains is above the {} ceiling",
            cfg.max_domains
        );
    }
    if let Some(prev) = previous.filter(|p| *p > 0) {
        let floor =
            prev.saturating_mul(100 - u64::from(cfg.max_shrink_pct).min(100) as usize) / 100;
        if incoming < floor {
            anyhow::bail!(
                "{incoming} domains is more than {}% below the stored {prev}",
                cfg.max_shrink_pct
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EmailPolicyConfig {
        EmailPolicyConfig::default()
    }

    #[test]
    fn parses_the_shapes_both_upstream_lists_use() {
        let body = "\
# a comment
mailinator.com
  TEMPMAIL.NET  
trailing.dot.example.
guerrillamail.com # inline note

";
        let got: Vec<String> = parse(body).collect();
        assert_eq!(
            got,
            vec![
                "mailinator.com",
                "tempmail.net",
                "trailing.dot.example",
                "guerrillamail.com"
            ]
        );
    }

    #[test]
    fn drops_entries_that_would_blanket_a_tld() {
        let got: Vec<String> = parse("com\nnet\nreal.example\n").collect();
        assert_eq!(got, vec!["real.example"]);
    }

    #[test]
    fn drops_tokens_that_are_not_hostnames() {
        let got: Vec<String> =
            parse("not a domain\nhttp://x.test\n-lead.test\na..b\nok.test\n").collect();
        assert_eq!(got, vec!["ok.test"]);
    }

    #[test]
    fn a_list_too_small_to_be_real_is_refused() {
        assert!(sanity_check(10, None, &cfg()).is_err());
    }

    #[test]
    fn a_list_too_large_to_hold_is_refused() {
        assert!(sanity_check(9_000_000, None, &cfg()).is_err());
    }

    #[test]
    fn a_truncated_upstream_is_refused_even_though_it_parses() {
        // 74k domains yesterday, 40k today: well-formed, and not a real update.
        assert!(sanity_check(40_000, Some(74_000), &cfg()).is_err());
        assert!(sanity_check(70_000, Some(74_000), &cfg()).is_ok());
    }

    #[test]
    fn growth_is_never_a_reason_to_refuse() {
        assert!(sanity_check(120_000, Some(74_000), &cfg()).is_ok());
    }

    #[test]
    fn the_first_ever_refresh_has_nothing_to_compare_against() {
        assert!(sanity_check(74_000, None, &cfg()).is_ok());
        assert!(sanity_check(74_000, Some(0), &cfg()).is_ok());
    }
}
