//! Per-account / per-user request rate limiting.
//!
//! One `governor` direct limiter per `(scope, category)` key in a `DashMap`.
//! Both the account tier and the user tier are checked; whichever is hit first
//! produces the 429. Per-IP limiting is Caddy's job (it sees the real peer);
//! this tier keys on the authenticated subject, never the TCP peer, so a
//! single reverse-proxy hop cannot collapse every tenant into one bucket.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DashMapStateStore;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

use crate::domain::quota::Plan;
use crate::domain::{AccountId, UserId};

type DirectLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;
/// Keyed by org, user (when there is one) and the refusal's scope.
type RecordKey = (Option<uuid::Uuid>, Option<uuid::Uuid>, String);
type RecordLimiter = RateLimiter<RecordKey, DashMapStateStore<RecordKey>, DefaultClock>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateLimitCategory {
    ApiWrites,
    ApiReads,
    BulkOps,
    TestNow,
    CheckNow,
    Support,
    /// Events from alert sources, counted after their token is looked up.
    AlertEvents,
}

/// Spends the operator's mail budget, not a tenant resource, so paying more
/// buys no larger share of it.
const SUPPORT_PER_MINUTE: i32 = 2;

impl RateLimitCategory {
    /// Per-minute quota for this category from the plan. Floored at 1 so a
    /// misconfigured `0` becomes a clean (if strict) limiter rather than a
    /// `NonZeroU32` panic at `Quota::per_minute` (I6).
    fn per_minute(self, plan: &Plan) -> NonZeroU32 {
        let raw = match self {
            Self::ApiWrites => plan.api_writes_per_minute,
            Self::ApiReads => plan.api_reads_per_minute,
            Self::BulkOps => plan.bulk_ops_per_minute,
            Self::TestNow => plan.test_now_per_minute,
            Self::CheckNow => plan.check_now_per_minute,
            Self::Support => SUPPORT_PER_MINUTE,
            Self::AlertEvents => plan.alert_events_per_minute,
        };
        let clamped = u32::try_from(raw).unwrap_or(u32::MAX).max(1);
        NonZeroU32::new(clamped).expect("clamped to >= 1")
    }

    fn as_scope(self, tier: &'static str) -> String {
        let c = match self {
            Self::ApiWrites => "api_writes",
            Self::ApiReads => "api_reads",
            Self::BulkOps => "bulk_ops",
            Self::TestNow => "test_now",
            Self::CheckNow => "check_now",
            Self::Support => "support",
            Self::AlertEvents => "alert_events",
        };
        format!("{tier}_{c}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RateLimitKey {
    /// The account behind the request's org. Keyed on the account, not the
    /// org, so the plan's budget is one budget however many workspaces the
    /// customer splits their monitors across.
    Account(AccountId, RateLimitCategory),
    User(UserId, RateLimitCategory),
}

struct Entry {
    limiter: DirectLimiter,
    /// The quota it was built with: what one spend may take at most.
    burst: NonZeroU32,
    /// Unix-millis of the last `check`. The janitor evicts entries idle
    /// longer than the threshold so the map stays bounded.
    last_seen: AtomicI64,
}

/// Outcome of a denied check: the scope label and how long until a retry.
pub struct Denied {
    pub scope: String,
    pub retry_after_secs: u32,
}

pub struct RateLimitService {
    limiters: DashMap<RateLimitKey, Arc<Entry>>,
    clock: DefaultClock,
    /// One quota event a minute per org, user and scope refused.
    recorded: RecordLimiter,
}

impl RateLimitService {
    pub fn new() -> Self {
        Self {
            limiters: DashMap::new(),
            clock: DefaultClock::default(),
            recorded: RateLimiter::dashmap(Quota::per_minute(NonZeroU32::MIN)),
        }
    }

    fn now_millis() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// Check one tier. `Ok(())` = allowed; `Err(Denied)` = 429.
    pub fn check(&self, key: RateLimitKey, tier: &'static str, plan: &Plan) -> Result<(), Denied> {
        self.check_n(key, tier, plan, 1)
    }

    /// Spend `n` from one tier at once. More than a whole minute's budget
    /// spends just that, so one large request waits for a full bucket rather
    /// than being refused for ever.
    pub fn check_n(
        &self,
        key: RateLimitKey,
        tier: &'static str,
        plan: &Plan,
        n: u32,
    ) -> Result<(), Denied> {
        let category = match &key {
            RateLimitKey::Account(_, c) | RateLimitKey::User(_, c) => *c,
        };
        let entry = self
            .limiters
            .entry(key)
            .or_insert_with(|| {
                let burst = category.per_minute(plan);
                Arc::new(Entry {
                    limiter: RateLimiter::direct(Quota::per_minute(burst)),
                    burst,
                    last_seen: AtomicI64::new(Self::now_millis()),
                })
            })
            .clone();
        entry.last_seen.store(Self::now_millis(), Ordering::Relaxed);

        let n = NonZeroU32::new(n.clamp(1, entry.burst.get())).expect("clamped to >= 1");
        let checked = entry
            .limiter
            .check_n(n)
            .expect("n is within the burst it was built with");
        match checked {
            Ok(()) => Ok(()),
            Err(not_until) => {
                let wait = not_until.wait_time_from(self.clock.now());
                Err(Denied {
                    scope: category.as_scope(tier),
                    retry_after_secs: (wait.as_secs() as u32).max(1),
                })
            }
        }
    }

    /// Whether a refusal in `scope` for `org` and `user` is worth a quota
    /// event now: at most one a minute each, so a storm past a budget adds no
    /// writes of its own.
    pub fn records_denial(
        &self,
        org: Option<crate::domain::OrgId>,
        user: Option<crate::domain::UserId>,
        scope: &str,
    ) -> bool {
        self.recorded
            .check_key(&(org.map(|o| o.0), user.map(|u| u.0), scope.to_owned()))
            .is_ok()
    }

    /// Evict entries untouched for `idle`. Returns how many were removed.
    /// One indexed pass; called by the janitor only.
    pub fn sweep(&self, idle: Duration) -> usize {
        self.recorded.retain_recent();
        let cutoff = Self::now_millis() - idle.as_millis() as i64;
        let stale: Vec<RateLimitKey> = self
            .limiters
            .iter()
            .filter(|e| e.value().last_seen.load(Ordering::Relaxed) < cutoff)
            .map(|e| e.key().clone())
            .collect();
        for k in &stale {
            self.limiters.remove(k);
        }
        stale.len()
    }

    /// Live entry count. Used by the janitor regression test to assert the
    /// sweep bounds the map.
    pub fn len(&self) -> usize {
        self.limiters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.limiters.is_empty()
    }

    /// Spawn the idle-entry janitor. Co-located with the service so the
    /// "construct" and "sweep" responsibilities are not separable: the task
    /// exits when `shutdown` fires (same lifetime as every other background
    /// worker), so a refactor cannot silently drop the sweep and leak the map.
    pub fn spawn_janitor(
        self: Arc<Self>,
        cleanup_every: Duration,
        idle_threshold: Duration,
        shutdown: CancellationToken,
    ) {
        tokio::spawn(async move {
            let mut ticker = interval(cleanup_every);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = ticker.tick() => {
                        let removed = self.sweep(idle_threshold);
                        if removed > 0 {
                            tracing::debug!(removed, "rate-limit janitor swept idle entries");
                        }
                    }
                }
            }
        });
    }
}

impl Default for RateLimitService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spend_past_the_budget_takes_the_whole_minute_and_no_more() {
        let plan = Plan {
            alert_events_per_minute: 3,
            ..crate::quotas::service::unlimited_plan()
        };
        let limits = RateLimitService::new();
        let key = || {
            RateLimitKey::Account(
                AccountId(uuid::Uuid::now_v7()),
                RateLimitCategory::AlertEvents,
            )
        };
        let account = key();
        assert!(
            limits
                .check_n(account.clone(), "per_account", &plan, 10)
                .is_ok()
        );
        let denied = limits
            .check_n(account, "per_account", &plan, 1)
            .expect_err("the bucket is empty");
        assert_eq!(denied.scope, "per_account_alert_events");
        assert!(limits.check_n(key(), "per_account", &plan, 3).is_ok());
    }

    #[test]
    fn a_refusal_is_recorded_once_a_minute_per_org_and_scope() {
        let limits = RateLimitService::new();
        let (a, b) = (
            Some(crate::domain::OrgId(uuid::Uuid::now_v7())),
            Some(crate::domain::OrgId(uuid::Uuid::now_v7())),
        );
        assert!(limits.records_denial(a, None, "per_account_alert_events"));
        assert!(
            !limits.records_denial(a, None, "per_account_alert_events"),
            "once a minute"
        );
        assert!(
            limits.records_denial(a, None, "per_account_api_writes"),
            "each scope its own"
        );
        assert!(
            limits.records_denial(b, None, "per_account_alert_events"),
            "each org its own"
        );
    }

    /// The bucket refills continuously, so another source's small spends
    /// leave room again within one interval, and a body worth the whole
    /// budget is not kept out by them.
    #[test]
    fn small_spends_do_not_keep_a_whole_budget_body_out() {
        let plan = Plan {
            alert_events_per_minute: 1200,
            ..crate::quotas::service::unlimited_plan()
        };
        let limits = RateLimitService::new();
        let account = RateLimitKey::Account(
            AccountId(uuid::Uuid::now_v7()),
            RateLimitCategory::AlertEvents,
        );
        assert!(
            limits
                .check_n(account.clone(), "per_account", &plan, 1)
                .is_ok()
        );
        std::thread::sleep(Duration::from_millis(60));
        assert!(limits.check_n(account, "per_account", &plan, 5000).is_ok());
    }
}
