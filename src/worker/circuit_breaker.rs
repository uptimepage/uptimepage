use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU32, Ordering};

use chrono::Utc;
use metrics::counter;

use crate::config::CircuitBreakerConfig;
use crate::domain::CheckStatus;
use crate::metric_names;

pub const CIRCUIT_OPEN_REASON: &str = "circuit_open";

fn record_transition(from: BreakerState, to: BreakerState) {
    counter!(metric_names::BREAKER_STATE_CHANGES, "from" => from.as_label(), "to" => to.as_label())
        .increment(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BreakerState {
    Closed = 0,
    Open = 1,
    HalfOpen = 2,
}

impl BreakerState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Open,
            2 => Self::HalfOpen,
            _ => Self::Closed,
        }
    }

    fn as_label(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

pub struct CircuitBreaker {
    state: AtomicU8,
    failure_count: AtomicU32,
    success_count: AtomicU32,
    half_open_in_flight: AtomicU32,
    opened_at: AtomicI64,
    cfg: CircuitBreakerConfig,
}

impl CircuitBreaker {
    pub fn new(cfg: CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU8::new(BreakerState::Closed as u8),
            failure_count: AtomicU32::new(0),
            success_count: AtomicU32::new(0),
            half_open_in_flight: AtomicU32::new(0),
            opened_at: AtomicI64::new(0),
            cfg,
        }
    }

    pub fn state(&self) -> BreakerState {
        BreakerState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn allow(&self) -> bool {
        match self.state() {
            BreakerState::Closed => true,
            BreakerState::Open => {
                if !self.cooldown_elapsed() {
                    return false;
                }
                self.try_transition_to_half_open();
                self.try_admit_half_open()
            }
            BreakerState::HalfOpen => self.try_admit_half_open(),
        }
    }

    fn cooldown_elapsed(&self) -> bool {
        let now = Utc::now().timestamp();
        let opened = self.opened_at.load(Ordering::Acquire);
        now - opened >= self.cfg.open_duration_secs as i64
    }

    fn try_transition_to_half_open(&self) {
        if self
            .state
            .compare_exchange(
                BreakerState::Open as u8,
                BreakerState::HalfOpen as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.success_count.store(0, Ordering::Release);
            self.half_open_in_flight.store(0, Ordering::Release);
            record_transition(BreakerState::Open, BreakerState::HalfOpen);
        }
    }

    fn try_admit_half_open(&self) -> bool {
        let max = self.cfg.half_open_max_calls;
        loop {
            let cur = self.half_open_in_flight.load(Ordering::Acquire);
            if cur >= max {
                return false;
            }
            if self
                .half_open_in_flight
                .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub fn record(&self, status: CheckStatus) {
        let is_failure = matches!(status, CheckStatus::Down | CheckStatus::Error);
        match self.state() {
            BreakerState::Closed => {
                if is_failure {
                    let fails = self.failure_count.fetch_add(1, Ordering::AcqRel) + 1;
                    if fails >= self.cfg.failure_threshold {
                        self.trip_open();
                    }
                } else {
                    self.failure_count.store(0, Ordering::Release);
                }
            }
            BreakerState::HalfOpen => {
                self.half_open_in_flight.fetch_sub(1, Ordering::AcqRel);
                if is_failure {
                    self.trip_open();
                } else {
                    let succ = self.success_count.fetch_add(1, Ordering::AcqRel) + 1;
                    if succ >= self.cfg.success_threshold {
                        self.close();
                    }
                }
            }
            BreakerState::Open => {}
        }
    }

    fn trip_open(&self) {
        let prev = BreakerState::from_u8(self.state.load(Ordering::Acquire));
        if prev == BreakerState::Open {
            return;
        }
        self.state
            .store(BreakerState::Open as u8, Ordering::Release);
        self.opened_at
            .store(Utc::now().timestamp(), Ordering::Release);
        self.failure_count.store(0, Ordering::Release);
        self.success_count.store(0, Ordering::Release);
        self.half_open_in_flight.store(0, Ordering::Release);
        record_transition(prev, BreakerState::Open);
    }

    fn close(&self) {
        let prev = BreakerState::from_u8(self.state.load(Ordering::Acquire));
        if prev == BreakerState::Closed {
            return;
        }
        self.state
            .store(BreakerState::Closed as u8, Ordering::Release);
        self.failure_count.store(0, Ordering::Release);
        self.success_count.store(0, Ordering::Release);
        self.half_open_in_flight.store(0, Ordering::Release);
        record_transition(prev, BreakerState::Closed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            success_threshold: 2,
            open_duration_secs: 0,
            half_open_max_calls: 5,
        }
    }

    #[test]
    fn closed_stays_closed_on_success() {
        let cb = CircuitBreaker::new(cfg());
        assert!(cb.allow());
        cb.record(CheckStatus::Up);
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn trips_open_after_threshold_failures() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            assert!(cb.allow());
            cb.record(CheckStatus::Down);
        }
        assert_eq!(cb.state(), BreakerState::Open);
    }

    #[test]
    fn open_transitions_to_half_open_after_duration() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            cb.record(CheckStatus::Down);
        }
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(cb.allow());
        assert_eq!(cb.state(), BreakerState::HalfOpen);
    }

    #[test]
    fn half_open_closes_on_success_threshold() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            cb.record(CheckStatus::Down);
        }
        assert!(cb.allow());
        cb.record(CheckStatus::Up);
        assert!(cb.allow());
        cb.record(CheckStatus::Up);
        assert_eq!(cb.state(), BreakerState::Closed);
    }

    #[test]
    fn half_open_reopens_on_failure() {
        let cb = CircuitBreaker::new(cfg());
        for _ in 0..3 {
            cb.record(CheckStatus::Down);
        }
        assert!(cb.allow());
        cb.record(CheckStatus::Down);
        assert_eq!(cb.state(), BreakerState::Open);
    }

    #[test]
    fn half_open_admit_limit_enforced() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 10,
            open_duration_secs: 0,
            half_open_max_calls: 2,
        });
        cb.record(CheckStatus::Down);
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(cb.allow());
        assert!(cb.allow());
        assert!(!cb.allow());
    }
}
