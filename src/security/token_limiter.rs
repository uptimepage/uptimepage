//! A rate per presented capability token, checked before any database work,
//! so a flood on one token never reaches Postgres. Made-up tokens each start
//! with a full budget, so a caller bounds their lookups some other way. Each
//! token presented costs one table entry until its budget refills and a sweep
//! drops it.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};

/// Keys back at full budget are swept this often, so tokens nobody sends
/// again cannot grow the table between the owner's own sweeps.
const SWEEP_EVERY: u64 = 4096;

pub struct TokenLimiter {
    limiter: RateLimiter<u128, DashMapStateStore<u128>, DefaultClock>,
    clock: DefaultClock,
    checks: AtomicU64,
}

impl TokenLimiter {
    pub fn new(per_sec: u32, burst: u32) -> Self {
        Self::with_quota(
            Quota::per_second(NonZeroU32::new(per_sec).expect("nonzero rate"))
                .allow_burst(NonZeroU32::new(burst).expect("nonzero burst")),
        )
    }

    /// `per_minute` for each key, with no burst beyond it.
    pub fn per_minute(per_minute: u32) -> Self {
        Self::with_quota(Quota::per_minute(
            NonZeroU32::new(per_minute).expect("nonzero rate"),
        ))
    }

    fn with_quota(quota: Quota) -> Self {
        let clock = DefaultClock::default();
        Self {
            limiter: RateLimiter::dashmap_with_clock(quota, clock.clone()),
            clock,
            checks: AtomicU64::new(0),
        }
    }

    /// Takes one request from the budget of the token behind `key`
    /// (`capability_token::rate_key`), or says how long until it has one.
    /// A refused request takes nothing.
    pub fn check(&self, key: u128) -> Result<(), Duration> {
        if self.checks.fetch_add(1, Ordering::Relaxed) % SWEEP_EVERY == SWEEP_EVERY - 1 {
            self.sweep();
        }
        self.limiter
            .check_key(&key)
            .map_err(|not_until| not_until.wait_time_from(self.clock.now()))
    }

    pub fn sweep(&self) {
        self.limiter.retain_recent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_past_its_burst_waits_and_another_token_does_not() {
        let limiter = TokenLimiter::new(10, 300);
        for _ in 0..300 {
            assert!(limiter.check(1).is_ok());
        }
        let wait = limiter.check(1).expect_err("past the burst");
        assert!(wait > Duration::ZERO && wait <= Duration::from_millis(100));
        assert!(limiter.check(2).is_ok(), "each token has its own budget");
    }
}
