//! Heartbeat evaluation: a scheduled check that reads in-memory ping state
//! instead of probing the network.
//!
//! Postgres is the source of truth. The scheduler's refresh tick reconciles
//! this cache from it before dispatching, so restarts, re-arms, org restores
//! and multi-replica ingest converge within one refresh interval.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use std::collections::HashMap;
use std::num::NonZeroU32;
use uuid::Uuid;

use crate::domain::heartbeat::PingState;
use crate::domain::{CheckResult, CheckStatus, HeartbeatCheck, PingSignal};

/// Per-token accepted-ping rate + burst; extra pings 429. Keyed pre-resolve so
/// rejected pings never reach Postgres.
const PING_PER_SEC: u32 = 1;
const PING_BURST: u32 = 10;

type PingLimiter = RateLimiter<u128, DashMapStateStore<u128>, DefaultClock>;

/// Shared main↔worker state: the cache the executor reads, plus the ingest
/// rate limiter.
pub struct HeartbeatRuntime {
    states: DashMap<Uuid, PingState>,
    ping_limiter: PingLimiter,
}

impl Default for HeartbeatRuntime {
    fn default() -> Self {
        let quota = Quota::per_second(NonZeroU32::new(PING_PER_SEC).expect("nonzero"))
            .allow_burst(NonZeroU32::new(PING_BURST).expect("nonzero"));
        Self {
            states: DashMap::new(),
            ping_limiter: RateLimiter::dashmap(quota),
        }
    }
}

impl HeartbeatRuntime {
    pub fn state(&self, id: Uuid) -> Option<PingState> {
        self.states.get(&id).map(|e| *e)
    }

    /// Merged, not overwritten: a concurrent reconcile must not drop a field
    /// this ping did not carry.
    pub fn record(&self, id: Uuid, state: PingState) {
        self.states
            .entry(id)
            .and_modify(|cur| cur.merge_newer(state))
            .or_insert(state);
    }

    /// Prune ids the snapshot no longer has, merge the rest so a ping accepted
    /// mid-read never regresses, sweep idle rate-limiter keys.
    pub fn sync_states(&self, fresh: HashMap<Uuid, PingState>) {
        self.states.retain(|id, _| fresh.contains_key(id));
        for (id, state) in fresh {
            self.record(id, state);
        }
        self.ping_limiter.retain_recent();
    }

    /// Keyed on the presented token's hash. A rejected ping reserves nothing.
    pub fn allow_ping(&self, token_key: u128) -> bool {
        self.ping_limiter.check_key(&token_key).is_ok()
    }
}

pub fn execute_heartbeat_check(
    target_id: Uuid,
    org_id: Uuid,
    check: &HeartbeatCheck,
    runtime: &HeartbeatRuntime,
) -> CheckResult {
    let now = Utc::now();
    let Some(state) = runtime.state(target_id) else {
        // Sub-refresh-interval race at worst (refresh syncs state before
        // dispatch); Error resolves next tick rather than flapping a false Down.
        return CheckResult::error(
            target_id,
            org_id,
            "heartbeat state unavailable on this node",
        );
    };
    passive_result(target_id, org_id, now, verdict(now, &state, check))
}

/// A heartbeat verdict as a result row: nothing was probed, so no timings.
fn passive_result(
    target_id: Uuid,
    org_id: Uuid,
    at: DateTime<Utc>,
    error: Option<String>,
) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: at,
        status: if error.is_none() {
            CheckStatus::Up
        } else {
            CheckStatus::Down
        },
        duration_ms: 0,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        diagnostic: None,
        error,
    }
}

/// The verdict the wiring ping carries on its own, before the monitor reaches
/// the scheduler's registry. Judged on the signal alone, never on the silence
/// window: `armed_at` may be weeks old, and a job that has just started
/// speaking is not down for the time before it did.
pub fn first_ping_result(
    target_id: Uuid,
    org_id: Uuid,
    at: DateTime<Utc>,
    signal: PingSignal,
    exit_code: Option<u8>,
) -> Option<CheckResult> {
    let error = match signal {
        PingSignal::Success => None,
        PingSignal::Fail => Some(failure_message(exit_code)),
        // A run that announced itself has not reported an outcome yet.
        PingSignal::Start => return None,
    };
    Some(passive_result(target_id, org_id, at, error))
}

/// The verdict a later signal changed, so a job that recovers or reports a
/// failure is seen when it speaks, not at the scheduler's next look. Judged
/// at the signal's own instant on both states; an unchanged verdict reports
/// nothing, since the scheduler carries a steady state on its own.
pub fn transition_result(
    target_id: Uuid,
    org_id: Uuid,
    at: DateTime<Utc>,
    prev: &PingState,
    state: &PingState,
    check: &HeartbeatCheck,
) -> Option<CheckResult> {
    let before = verdict(at, prev, check);
    let after = verdict(at, state, check);
    if before.is_some() == after.is_some() {
        return None;
    }
    Some(passive_result(target_id, org_id, at, after))
}

fn failure_message(exit_code: Option<u8>) -> String {
    match exit_code {
        Some(code) => format!("job reported failure (exit {code})"),
        None => "job reported failure".to_string(),
    }
}

/// Cheapest evidence first: the job said it failed, the job announced a run it
/// never finished, or nothing has been heard for too long.
fn verdict(now: DateTime<Utc>, state: &PingState, check: &HeartbeatCheck) -> Option<String> {
    if let Some(failure) = state.failing() {
        return Some(failure_message(failure.exit_code));
    }
    if let (Some(started), Some(max)) = (state.run_open_since(), check.max_runtime) {
        let running = now.signed_duration_since(started);
        if running > to_chrono(max) {
            return Some(format!(
                "job started {}s ago and has not finished, past the {}s max runtime",
                running.num_seconds().max(0),
                max.as_secs()
            ));
        }
    }
    // A future anchor (ingest/eval clock skew) is a fresh ping, never Down.
    let age = now.signed_duration_since(state.success_at);
    (age > to_chrono(check.period + check.grace)).then(|| {
        format!(
            "no ping for {}s, expected every {}s (+{}s grace)",
            age.num_seconds().max(0),
            check.period.as_secs(),
            check.grace.as_secs()
        )
    })
}

fn to_chrono(d: std::time::Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::heartbeat::Failure;
    use std::time::Duration;

    fn check(period_s: u64, grace_s: u64) -> HeartbeatCheck {
        HeartbeatCheck {
            period: Duration::from_secs(period_s),
            grace: Duration::from_secs(grace_s),
            max_runtime: None,
        }
    }

    fn ago(secs: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::seconds(secs)
    }

    /// The pre-signals shape: one success anchor and nothing else.
    fn armed(rt: &HeartbeatRuntime, id: Uuid, at: DateTime<Utc>) {
        rt.record(id, PingState::from_success(at));
    }

    #[test]
    fn fresh_ping_is_up() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, Utc::now());
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(60, 30), &rt);
        assert_eq!(r.status, CheckStatus::Up);
        assert!(r.error.is_none());
    }

    #[test]
    fn stale_past_period_plus_grace_is_down() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, ago(120));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(60, 30), &rt);
        assert_eq!(r.status, CheckStatus::Down);
        let err = r.error.expect("down carries a reason");
        assert!(err.contains("expected every 60s"), "{err}");
    }

    #[test]
    fn within_grace_is_still_up() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, ago(80));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(60, 30), &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }

    #[test]
    fn reported_failure_is_down_before_the_window_expires() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        rt.record(
            id,
            PingState {
                success_at: ago(30),
                start_at: Some(ago(20)),
                fail: Some(Failure {
                    at: ago(5),
                    exit_code: Some(137),
                }),
            },
        );
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(600, 300), &rt);
        assert_eq!(
            r.status,
            CheckStatus::Down,
            "silence rule says up, job says no"
        );
        assert!(r.error.unwrap().contains("exit 137"));
    }

    #[test]
    fn a_later_success_clears_the_failure() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        rt.record(
            id,
            PingState {
                success_at: ago(5),
                start_at: None,
                fail: Some(Failure {
                    at: ago(60),
                    exit_code: Some(1),
                }),
            },
        );
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(600, 0), &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }

    #[test]
    fn an_open_run_past_max_runtime_is_down() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        rt.record(
            id,
            PingState {
                success_at: ago(400),
                start_at: Some(ago(300)),
                fail: None,
            },
        );
        let mut spec = check(3600, 600);
        spec.max_runtime = Some(Duration::from_secs(120));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &spec, &rt);
        assert_eq!(r.status, CheckStatus::Down);
        assert!(r.error.unwrap().contains("max runtime"));

        // No max runtime configured leaves the run bounded only by the window.
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(3600, 600), &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }

    #[test]
    fn a_finished_run_is_not_an_open_one() {
        // start then success: max runtime must not judge the closed run.
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        rt.record(
            id,
            PingState {
                success_at: ago(10),
                start_at: Some(ago(300)),
                fail: None,
            },
        );
        let mut spec = check(3600, 600);
        spec.max_runtime = Some(Duration::from_secs(120));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &spec, &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }

    #[test]
    fn missing_anchor_is_error_not_down() {
        let rt = HeartbeatRuntime::default();
        let r = execute_heartbeat_check(Uuid::new_v4(), Uuid::new_v4(), &check(60, 0), &rt);
        assert_eq!(r.status, CheckStatus::Error);
    }

    #[test]
    fn future_anchor_is_up() {
        // Clock skew between the ingest write and evaluation must not flap.
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, Utc::now() + chrono::Duration::seconds(5));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(60, 0), &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }

    #[test]
    fn ping_rate_limits_after_burst_and_isolates_tokens() {
        let rt = HeartbeatRuntime::default();
        let mut accepted = 0;
        while rt.allow_ping(7) {
            accepted += 1;
            assert!(accepted < 100, "GCRA never rejected");
        }
        assert!(accepted >= 10, "burst allowance too small: {accepted}");
        assert!(rt.allow_ping(8), "a second token has its own budget");
    }

    #[test]
    fn sync_states_prunes_merges_and_inserts() {
        let rt = HeartbeatRuntime::default();
        let kept = Uuid::new_v4();
        let gone = Uuid::new_v4();
        let added = Uuid::new_v4();
        let newer = Utc::now();
        let older = newer - chrono::Duration::hours(1);

        armed(&rt, kept, newer);
        armed(&rt, gone, newer);
        rt.sync_states(HashMap::from([
            (kept, PingState::from_success(older)),
            (added, PingState::from_success(older)),
        ]));

        let kept_state = rt.state(kept).expect("kept");
        assert_eq!(kept_state.success_at, newer, "merge newer, never regress");
        assert_eq!(
            rt.state(added).unwrap().success_at,
            older,
            "snapshot-only id inserted"
        );
        assert!(rt.state(gone).is_none(), "absent-from-snapshot id pruned");

        // A newer snapshot value (a re-arm on enable) advances the cache.
        let bumped = newer + chrono::Duration::seconds(5);
        rt.sync_states(HashMap::from([(kept, PingState::from_success(bumped))]));
        assert_eq!(rt.state(kept).unwrap().success_at, bumped);
    }

    #[test]
    fn a_snapshot_that_predates_a_ping_keeps_every_newer_field() {
        // The reconcile reads Postgres while a ping lands on this node; neither
        // side may erase a field the other advanced.
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        let live = PingState {
            success_at: ago(300),
            start_at: Some(ago(10)),
            fail: Some(Failure {
                at: ago(1),
                exit_code: Some(2),
            }),
        };
        rt.record(id, live);
        rt.sync_states(HashMap::from([(
            id,
            PingState {
                success_at: ago(600),
                start_at: Some(ago(700)),
                fail: None,
            },
        )]));
        assert_eq!(
            rt.state(id),
            Some(live),
            "the stale snapshot erased nothing"
        );
    }

    #[test]
    fn zero_grace_within_period_is_up() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, ago(30));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(60, 0), &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }

    #[test]
    fn zero_grace_past_period_is_down() {
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, ago(90));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(60, 0), &rt);
        assert_eq!(r.status, CheckStatus::Down);
    }

    #[test]
    fn just_inside_period_plus_grace_is_up() {
        // One second short of the period+grace window still counts as up.
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, ago(89));
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(60, 30), &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }

    #[test]
    fn max_period_does_not_overflow_the_window() {
        // 30-day period + grace must convert without saturating to a false Down.
        let rt = HeartbeatRuntime::default();
        let id = Uuid::new_v4();
        armed(&rt, id, ago(86_400));
        let month = 30 * 24 * 3600;
        let r = execute_heartbeat_check(id, Uuid::new_v4(), &check(month, month), &rt);
        assert_eq!(r.status, CheckStatus::Up);
    }
}

#[cfg(test)]
mod ping_tests {
    use super::*;
    use crate::domain::heartbeat::Failure;
    use chrono::Utc;

    fn result(signal: PingSignal, exit_code: Option<u8>) -> Option<CheckResult> {
        first_ping_result(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Utc::now(),
            signal,
            exit_code,
        )
    }

    #[test]
    fn a_first_success_is_up_without_waiting_for_the_scheduler() {
        let r = result(PingSignal::Success, None).expect("success reports");
        assert_eq!(r.status, CheckStatus::Up);
        assert!(r.error.is_none());
    }

    #[test]
    fn a_first_failure_reports_the_job_verdict() {
        let r = result(PingSignal::Fail, Some(3)).expect("failure reports");
        assert_eq!(r.status, CheckStatus::Down);
        assert_eq!(r.error.as_deref(), Some("job reported failure (exit 3)"));
    }

    #[test]
    fn a_first_start_reports_nothing() {
        assert!(result(PingSignal::Start, None).is_none());
    }

    /// One sentence, whichever path wrote the row.
    #[test]
    fn the_failure_wording_matches_the_scheduled_verdict() {
        let state = PingState {
            success_at: ago(600),
            start_at: None,
            fail: Some(Failure {
                at: Utc::now(),
                exit_code: Some(3),
            }),
        };
        let scheduled = verdict(Utc::now(), &state, &check(60, 30));
        let on_ping = result(PingSignal::Fail, Some(3)).unwrap().error;
        assert_eq!(scheduled, on_ping);
    }

    fn transition(prev: PingState, state: PingState) -> Option<CheckResult> {
        transition_result(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            Utc::now(),
            &prev,
            &state,
            &check(60, 30),
        )
    }

    /// The scheduler judged silence; the ping that ends it is the recovery.
    #[test]
    fn a_success_after_silence_reports_up_at_once() {
        let r = transition(
            PingState::from_success(ago(600)),
            PingState::from_success(Utc::now()),
        )
        .expect("the verdict changed");
        assert_eq!(r.status, CheckStatus::Up);
        assert!(r.error.is_none());
    }

    #[test]
    fn a_failure_signal_reports_down_at_once() {
        let prev = PingState::from_success(ago(10));
        let state = PingState {
            fail: Some(Failure {
                at: Utc::now(),
                exit_code: Some(2),
            }),
            ..prev
        };
        let r = transition(prev, state).expect("the verdict changed");
        assert_eq!(r.status, CheckStatus::Down);
        assert_eq!(r.error.as_deref(), Some("job reported failure (exit 2)"));
    }

    /// A routine ping on a healthy job, and a ping that leaves it failing,
    /// say nothing new: the scheduler carries a steady state.
    #[test]
    fn an_unchanged_verdict_reports_nothing() {
        assert!(
            transition(
                PingState::from_success(ago(20)),
                PingState::from_success(Utc::now()),
            )
            .is_none()
        );
        let failing = PingState {
            success_at: ago(600),
            start_at: None,
            fail: Some(Failure {
                at: ago(300),
                exit_code: None,
            }),
        };
        let still_failing = PingState {
            start_at: Some(Utc::now()),
            ..failing
        };
        assert!(transition(failing, still_failing).is_none());
    }

    fn ago(secs: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::seconds(secs)
    }

    fn check(period_s: u64, grace_s: u64) -> HeartbeatCheck {
        HeartbeatCheck {
            period: std::time::Duration::from_secs(period_s),
            grace: std::time::Duration::from_secs(grace_s),
            max_runtime: None,
        }
    }
}
