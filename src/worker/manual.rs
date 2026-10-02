//! Manual evaluation: a scheduled check that restates what an operator last
//! set, so history and the incident writer see it like any other result.
//!
//! Postgres is the source of truth; the scheduler's refresh reconciles this
//! cache before dispatching, and a set records here on the node that took it.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::{CheckResult, ManualState};

#[derive(Default)]
pub struct ManualRuntime {
    states: DashMap<Uuid, ManualState>,
}

impl ManualRuntime {
    pub fn state(&self, id: Uuid) -> Option<ManualState> {
        self.states.get(&id).map(|e| e.clone())
    }

    /// The later set wins, so a refresh that read before a set cannot undo it.
    pub fn record(&self, id: Uuid, state: ManualState) {
        self.states
            .entry(id)
            .and_modify(|cur| cur.merge_newer(state.clone()))
            .or_insert(state);
    }

    /// Prune what the snapshot no longer has, merge the rest.
    pub fn sync_states(&self, fresh: HashMap<Uuid, ManualState>) {
        self.states.retain(|id, _| fresh.contains_key(id));
        for (id, state) in fresh {
            self.record(id, state);
        }
    }
}

/// `None` while this node holds no state for the target. Writing nothing is
/// the only safe answer: one result confirms a manual outage, so a guessed
/// `up` would close a real one and an `error` would open a false one.
pub fn execute_manual_check(
    target_id: Uuid,
    org_id: Uuid,
    runtime: &ManualRuntime,
) -> Option<CheckResult> {
    // Before the read, so a set recorded meanwhile is stamped no earlier.
    let at = Utc::now();
    let Some(state) = runtime.state(target_id) else {
        tracing::warn!(%target_id, "manual state unavailable on this node; nothing restated");
        return None;
    };
    Some(manual_result(target_id, org_id, at, &state))
}

/// The state as a result row: nothing was probed, so no timings.
pub fn manual_result(
    target_id: Uuid,
    org_id: Uuid,
    at: DateTime<Utc>,
    state: &ManualState,
) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: at,
        status: state.status.check_status(),
        duration_ms: 0,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        diagnostic: None,
        error: state.error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CheckStatus, ManualStatus};

    fn set(status: ManualStatus, secs_ago: i64) -> ManualState {
        ManualState {
            status,
            note: None,
            set_at: Utc::now() - chrono::Duration::seconds(secs_ago),
            set_by: None,
        }
    }

    #[test]
    fn restates_what_was_set() {
        let rt = ManualRuntime::default();
        let id = Uuid::new_v4();
        rt.record(id, set(ManualStatus::Degraded, 5));
        let r = execute_manual_check(id, Uuid::new_v4(), &rt).expect("state held");
        assert_eq!(r.status, CheckStatus::Degraded);
        assert_eq!(r.error.as_deref(), Some("marked degraded"));
    }

    #[test]
    fn no_state_writes_nothing() {
        let rt = ManualRuntime::default();
        assert!(execute_manual_check(Uuid::new_v4(), Uuid::new_v4(), &rt).is_none());
    }

    #[test]
    fn a_snapshot_older_than_a_set_does_not_undo_it() {
        let rt = ManualRuntime::default();
        let id = Uuid::new_v4();
        rt.record(id, set(ManualStatus::Down, 1));
        rt.sync_states(HashMap::from([(id, set(ManualStatus::Up, 600))]));
        assert_eq!(rt.state(id).unwrap().status, ManualStatus::Down);

        rt.sync_states(HashMap::from([(id, set(ManualStatus::Up, 0))]));
        assert_eq!(
            rt.state(id).unwrap().status,
            ManualStatus::Up,
            "a later set from another node lands"
        );
    }

    #[test]
    fn sync_prunes_targets_the_snapshot_dropped() {
        let rt = ManualRuntime::default();
        let gone = Uuid::new_v4();
        rt.record(gone, set(ManualStatus::Down, 1));
        rt.sync_states(HashMap::new());
        assert!(rt.state(gone).is_none());
    }
}
