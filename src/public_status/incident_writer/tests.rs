use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::TimeZone;

use crate::domain::{
    CheckDiagnostic, CheckSpec, DiagnosticConfidence, DiagnosticEvidence, EdgeProvider,
    ExpectedStatus, HttpCheck, HttpMethod, Target, TargetAlerts,
};
use crate::storage::admin::EnabledTargetStream;
use crate::storage::{InMemorySink, InMemoryTargetStore, ResultSink};

use super::*;

fn ts(base: DateTime<Utc>, secs: i64) -> DateTime<Utc> {
    base + ChronoDuration::seconds(secs)
}

fn result(target_id: Uuid, when: DateTime<Utc>, status: CheckStatus) -> CheckResult {
    CheckResult {
        target_id,
        org_id: Uuid::nil(),
        timestamp: when,
        status,
        duration_ms: 1,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        diagnostic: None,
        error: None,
    }
}

fn akamai_blocked(target_id: Uuid, when: DateTime<Utc>) -> CheckResult {
    let mut blocked = result(target_id, when, CheckStatus::Down);
    blocked.error = Some("unexpected status 403".into());
    blocked.diagnostic = Some(CheckDiagnostic::access_interference(
        DiagnosticConfidence::High,
        Some(EdgeProvider::Akamai),
        vec![DiagnosticEvidence::BlockPage],
    ));
    blocked
}

fn tunnel_down(target_id: Uuid, when: DateTime<Utc>) -> CheckResult {
    let mut dead = result(target_id, when, CheckStatus::Down);
    dead.error = Some("unexpected status 530".into());
    dead.response_code = Some(530);
    dead.diagnostic = Some(CheckDiagnostic::origin_unreachable(
        vec![
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::ReferenceId,
            DiagnosticEvidence::OriginErrorCode,
        ],
        true,
    ));
    dead
}

// ── pure decide() ──────────────────────────────────────────────────────

#[test]
fn decide_no_results_is_noop() {
    let action = decide(None, &[], 2);
    assert_eq!(action, Action::None);
}

#[test]
fn decide_single_bad_then_recovery_does_not_open() {
    // [bad, up] — single bad swallowed by the 2-check threshold.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Down),
        result(target, ts(base, 30), CheckStatus::Up),
    ];
    assert_eq!(decide(None, &results, 2), Action::None);
}

#[test]
fn decide_two_consecutive_bad_opens_incident() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Up),
        result(target, ts(base, 30), CheckStatus::Down),
        result(target, ts(base, 60), CheckStatus::Down),
    ];
    match decide(None, &results, 2) {
        Action::Open(new) => {
            assert_eq!(new.target_id, target);
            assert_eq!(new.started_at, ts(base, 30));
            assert_eq!(new.status_at_start, CheckStatus::Down);
            assert_eq!(new.check_count, 2);
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_carries_access_diagnosis_into_incident_sample() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let blocked = akamai_blocked(target, base);

    match decide(None, &[blocked], 1) {
        Action::Open(new) => assert_eq!(
            new.error_sample.as_deref(),
            Some(
                "unexpected status 403 · access-policy block detected at the Akamai edge · use an authenticated health endpoint, or allow this monitor through the edge's access rules"
            )
        ),
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_requires_cross_region_cause_consensus_and_reports_it() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let by_region = ["eu", "us", "ap"].map(|region| {
        (
            region.to_string(),
            vec![
                akamai_blocked(target, base),
                akamai_blocked(target, ts(base, 30)),
            ],
        )
    });

    match decide_multi(target, &[], &by_region, 2, 2)
        .into_iter()
        .next()
    {
        Some(Action::Open(new)) => {
            let sample = new.error_sample.expect("diagnostic sample");
            assert!(sample.contains("3/3 reporting regions agree"), "{sample}");
            assert!(sample.contains("authenticated health endpoint"), "{sample}");
            assert!(
                sample.chars().count() <= 200,
                "cause and remediation must survive notifier clipping: {sample}"
            );
            assert!(
                sample.find("authenticated health endpoint") < sample.find("regions agree"),
                "the fix outranks the tally when a long error text forces a clip: {sample}"
            );
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_names_the_failing_side_when_every_region_sees_a_dead_origin() {
    // The shape that prompted this: every region on 530, cause unnamed.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let by_region = ["eu-helsinki", "us-east", "apac-sg"].map(|region| {
        (
            region.to_string(),
            vec![tunnel_down(target, base), tunnel_down(target, ts(base, 30))],
        )
    });

    match decide_multi(target, &[], &by_region, 2, 2)
        .into_iter()
        .next()
    {
        Some(Action::Open(new)) => {
            let sample = new.error_sample.expect("diagnostic sample");
            assert!(
                sample.contains("origin tunnel down behind the Cloudflare edge"),
                "{sample}"
            );
            assert!(sample.contains("restart the tunnel daemon"), "{sample}");
            assert!(sample.contains("3/3 reporting regions agree"), "{sample}");
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_will_not_blame_the_edge_on_one_regions_evidence() {
    // Below quorum the incident must stay unattributed.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let mut ordinary = result(target, base, CheckStatus::Down);
    ordinary.error = Some("connection refused".into());
    let by_region = [
        (
            "eu-helsinki".to_string(),
            vec![tunnel_down(target, base), tunnel_down(target, ts(base, 30))],
        ),
        (
            "us-east".to_string(),
            vec![ordinary.clone(), ordinary.clone()],
        ),
        (
            "apac-sg".to_string(),
            vec![ordinary.clone(), ordinary.clone()],
        ),
    ];

    match decide_multi(target, &[], &by_region, 2, 2)
        .into_iter()
        .next()
    {
        Some(Action::Open(new)) => {
            let sample = new.error_sample.unwrap_or_default();
            assert!(!sample.contains("origin tunnel down"), "{sample}");
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_does_not_promote_one_regions_guess_to_majority_cause() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let mut ordinary = result(target, base, CheckStatus::Down);
    ordinary.error = Some("connection refused".into());
    let by_region = [
        (
            "eu".to_string(),
            vec![
                akamai_blocked(target, base),
                akamai_blocked(target, ts(base, 30)),
            ],
        ),
        ("us".to_string(), vec![ordinary.clone(), ordinary.clone()]),
        (
            "ap".to_string(),
            vec![result(target, base, CheckStatus::Up)],
        ),
    ];

    match decide_multi(target, &[], &by_region, 2, 2)
        .into_iter()
        .next()
    {
        Some(Action::Open(new)) => {
            let sample = new.error_sample.expect("protocol sample");
            assert!(!sample.contains("Akamai"), "{sample}");
            assert!(!sample.contains("reporting regions agree"), "{sample}");
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_three_bad_run_carries_count() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Error),
        result(target, ts(base, 30), CheckStatus::Down),
        result(target, ts(base, 60), CheckStatus::Down),
    ];
    match decide(None, &results, 2) {
        Action::Open(new) => {
            assert_eq!(new.check_count, 3);
            // Worst status in the confirmed run sets the kick-off status.
            assert_eq!(new.status_at_start, CheckStatus::Down);
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_multi_worst_region_sets_status_not_earliest() {
    let b = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    // Region A degrades first; region B hard-fails later. The earliest
    // onset must not mask the hard failure.
    let by_region = [
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Degraded),
                result(t, ts(b, 30), CheckStatus::Degraded),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 10), CheckStatus::Down),
                result(t, ts(b, 40), CheckStatus::Down),
            ],
        ),
    ];
    match decide_multi(t, &[], &by_region, 2, 2).into_iter().next() {
        Some(Action::Open(new)) => {
            assert_eq!(new.status_at_start, CheckStatus::Down);
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

/// A region one check behind the quorum joins the breakdown once it confirms;
/// silence adds nothing and a recovery removes nothing.
#[test]
fn the_breakdown_only_grows_while_open() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: t,
        region: None,
        regions_down: vec!["fra".into(), "us".into()],
        started_at: ts(base, 0),
    };
    let down = |offsets: &[i64]| -> Vec<CheckResult> {
        offsets
            .iter()
            .map(|o| result(t, ts(base, *o), CheckStatus::Down))
            .collect()
    };
    let region = |name: &str, results: Vec<CheckResult>| (name.to_string(), results);

    // Helsinki still one check in: nothing to write.
    let by_region = vec![
        region("fra", down(&[0, 30])),
        region("us", down(&[10, 40])),
        region("hel", down(&[50])),
    ];
    assert!(decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 2).is_empty());

    // Its second failure lands: it joins.
    let by_region = vec![
        region("fra", down(&[0, 30])),
        region("us", down(&[10, 40])),
        region("hel", down(&[50, 80])),
    ];
    match decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 2).as_slice() {
        [
            Action::Widen {
                incident_id,
                regions,
            },
        ] => {
            assert_eq!(*incident_id, open.id);
            assert_eq!(regions, &["hel"]);
        }
        other => panic!("expected Widen, got {other:?}"),
    }

    // Frankfurt recovering while the other two hold the quorum, or Helsinki
    // going quiet, changes nothing stored.
    let full = OpenIncident {
        regions_down: vec!["fra".into(), "us".into(), "hel".into()],
        ..open.clone()
    };
    for by_region in [
        vec![
            region("fra", vec![result(t, ts(base, 110), CheckStatus::Up)]),
            region("us", down(&[10, 40])),
            region("hel", down(&[50, 80])),
        ],
        vec![region("fra", down(&[0, 30])), region("us", down(&[10, 40]))],
        vec![],
    ] {
        assert!(
            decide_multi(t, std::slice::from_ref(&full), &by_region, 2, 2).is_empty(),
            "{by_region:?}"
        );
    }
}

#[test]
fn decide_two_good_closes_open_incident() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: target,
        region: None,
        regions_down: Vec::new(),
        started_at: ts(base, 0),
    };
    let results = vec![
        result(target, ts(base, 30), CheckStatus::Down),
        result(target, ts(base, 60), CheckStatus::Up),
        result(target, ts(base, 90), CheckStatus::Up),
    ];
    match decide(Some(&open), &results, 2) {
        Action::Close {
            incident_id,
            ended_at,
        } => {
            assert_eq!(incident_id, open.id);
            assert_eq!(ended_at, ts(base, 60));
        }
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn decide_single_good_does_not_close_open_incident() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: target,
        region: None,
        regions_down: Vec::new(),
        started_at: ts(base, 0),
    };
    let results = vec![
        result(target, ts(base, 30), CheckStatus::Down),
        result(target, ts(base, 60), CheckStatus::Up),
    ];
    assert_eq!(decide(Some(&open), &results, 2), Action::None);
}

#[test]
fn decide_recovery_run_before_incident_does_not_close() {
    // Stale up-rows pre-date the incident — shouldn't fool us into closing.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: target,
        region: None,
        regions_down: Vec::new(),
        started_at: ts(base, 1_000),
    };
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Up),
        result(target, ts(base, 30), CheckStatus::Up),
    ];
    // Tail-up exists but pre-dates incident.started_at → no action.
    assert_eq!(decide(Some(&open), &results, 2), Action::None);
}

#[test]
fn decide_isolated_good_blip_does_not_close_then_reopen() {
    // A flapping monitor: while an incident is open, one stray Up between bad
    // checks must not close it — a close would be followed by a reopen on the
    // next bad run, a page storm. Symmetric confirmation (a sustained good run
    // to close, a sustained bad run to reopen) keeps it one incident, so no
    // separate flap cooldown is needed.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: target,
        region: None,
        regions_down: Vec::new(),
        started_at: ts(base, 0),
    };
    let results = vec![
        result(target, ts(base, 30), CheckStatus::Down),
        result(target, ts(base, 60), CheckStatus::Up),
        result(target, ts(base, 90), CheckStatus::Down),
        result(target, ts(base, 120), CheckStatus::Down),
    ];
    assert_eq!(decide(Some(&open), &results, 2), Action::None);
}

#[test]
fn decide_degraded_run_opens_incident() {
    // A degraded service is unhealthy: a sustained run opens an incident.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Degraded),
        result(target, ts(base, 30), CheckStatus::Degraded),
        result(target, ts(base, 60), CheckStatus::Degraded),
    ];
    match decide(None, &results, 2) {
        Action::Open(new) => {
            assert_eq!(new.status_at_start, CheckStatus::Degraded);
            assert_eq!(new.check_count, 3);
            assert_eq!(new.started_at, ts(base, 0));
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_degraded_run_does_not_close_open_incident() {
    // Degraded is not recovery; the incident stays open until a clean Up run.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: target,
        region: None,
        regions_down: Vec::new(),
        started_at: ts(base, 0),
    };
    let results = vec![
        result(target, ts(base, 30), CheckStatus::Error),
        result(target, ts(base, 60), CheckStatus::Degraded),
        result(target, ts(base, 90), CheckStatus::Degraded),
    ];
    assert_eq!(decide(Some(&open), &results, 2), Action::None);
}

#[test]
fn decide_trailing_degraded_extends_a_bad_run() {
    // [Down, Down, Degraded] is still three unhealthy checks in a row.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Down),
        result(target, ts(base, 30), CheckStatus::Down),
        result(target, ts(base, 60), CheckStatus::Degraded),
    ];
    match decide(None, &results, 2) {
        Action::Open(new) => {
            assert_eq!(new.check_count, 3);
            assert_eq!(new.status_at_start, CheckStatus::Down);
            assert_eq!(new.started_at, ts(base, 0));
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_degraded_within_a_bad_run_carries_count() {
    // [Down, Degraded, Down, Down] is one unbroken unhealthy run of 4.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Down),
        result(target, ts(base, 30), CheckStatus::Degraded),
        result(target, ts(base, 60), CheckStatus::Down),
        result(target, ts(base, 90), CheckStatus::Down),
    ];
    match decide(None, &results, 2) {
        Action::Open(new) => {
            assert_eq!(new.check_count, 4);
            assert_eq!(new.started_at, ts(base, 0));
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

#[test]
fn decide_trailing_degraded_does_not_close() {
    // A single Up between bad checks, ending Degraded, is not a recovery run.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: target,
        region: None,
        regions_down: Vec::new(),
        started_at: ts(base, 0),
    };
    let results = vec![
        result(target, ts(base, 30), CheckStatus::Down),
        result(target, ts(base, 60), CheckStatus::Up),
        result(target, ts(base, 90), CheckStatus::Degraded),
    ];
    assert_eq!(decide(Some(&open), &results, 2), Action::None);
}

#[test]
fn decide_running_twice_with_same_data_is_idempotent_for_open() {
    // After an Open, the caller writes it back. Re-running decide() with
    // the same results but now-known open incident produces Action::None.
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let results = vec![
        result(target, ts(base, 0), CheckStatus::Down),
        result(target, ts(base, 30), CheckStatus::Down),
    ];
    match decide(None, &results, 2) {
        Action::Open(_) => {}
        other => panic!("expected Open, got {other:?}"),
    }
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: target,
        region: None,
        regions_down: Vec::new(),
        started_at: ts(base, 0),
    };
    // Same input, but now we know about the open incident; trailing 'up'
    // run length is 0, so nothing happens.
    assert_eq!(decide(Some(&open), &results, 2), Action::None);
}

// ── multi-region decide_multi() ─────────────────────────────────────────

fn mbase() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap()
}

#[test]
fn any_down_opens_from_a_single_bad_region_among_healthy() {
    // The original interleave bug: one region down while another is up. A
    // blended stream could let the healthy region's rows mask it; per-region
    // evaluation opens correctly.
    let b = mbase();
    let t = Uuid::now_v7();
    let by_region = vec![
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Down),
                result(t, ts(b, 30), CheckStatus::Down),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Up),
                result(t, ts(b, 30), CheckStatus::Up),
            ],
        ),
    ];
    match decide_multi(t, &[], &by_region, 2, 1).as_slice() {
        [Action::Open(n)] => {
            assert_eq!(n.region, None, "combined incident is region-agnostic");
            assert_eq!(n.started_at, ts(b, 0));
            assert_eq!(n.status_at_start, CheckStatus::Down);
        }
        other => panic!("expected one Open, got {other:?}"),
    }
}

#[test]
fn any_down_stays_open_while_one_region_still_bad() {
    let b = mbase();
    let t = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: t,
        started_at: ts(b, 0),
        region: None,
        regions_down: vec!["eu".into()],
    };
    let by_region = vec![
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 30), CheckStatus::Down),
                result(t, ts(b, 60), CheckStatus::Up),
                result(t, ts(b, 90), CheckStatus::Up),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 60), CheckStatus::Down),
                result(t, ts(b, 90), CheckStatus::Down),
            ],
        ),
    ];
    let actions = decide_multi(t, &[open], &by_region, 2, 1);
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Close { .. })),
        "must not close while a region is still down: {actions:?}"
    );
    assert!(
        matches!(actions.as_slice(), [Action::Widen { regions, .. }] if regions == &["us"]),
        "the region that confirmed after the open joins: {actions:?}"
    );
}

#[test]
fn any_down_closes_when_all_regions_recovered() {
    let b = mbase();
    let t = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: t,
        started_at: ts(b, 0),
        region: None,
        regions_down: Vec::new(),
    };
    let by_region = vec![
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 30), CheckStatus::Down),
                result(t, ts(b, 60), CheckStatus::Up),
                result(t, ts(b, 90), CheckStatus::Up),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 120), CheckStatus::Up),
                result(t, ts(b, 150), CheckStatus::Up),
            ],
        ),
    ];
    match decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 1).as_slice() {
        [
            Action::Close {
                incident_id,
                ended_at,
            },
        ] => {
            assert_eq!(*incident_id, open.id);
            // Latest region recovery onset wins.
            assert_eq!(*ended_at, ts(b, 120));
        }
        other => panic!("expected one Close, got {other:?}"),
    }
}

#[test]
fn quorum_needs_two_regions_before_opening() {
    let b = mbase();
    let t = Uuid::now_v7();
    let one_bad = vec![
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Down),
                result(t, ts(b, 30), CheckStatus::Down),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Up),
                result(t, ts(b, 30), CheckStatus::Up),
            ],
        ),
    ];
    assert!(
        decide_multi(t, &[], &one_bad, 2, 2).is_empty(),
        "one region down is below quorum"
    );

    let two_bad = vec![
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Down),
                result(t, ts(b, 30), CheckStatus::Down),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 60), CheckStatus::Down),
                result(t, ts(b, 90), CheckStatus::Down),
            ],
        ),
    ];
    match decide_multi(t, &[], &two_bad, 2, 2).as_slice() {
        [Action::Open(n)] => {
            assert_eq!(n.region, None);
            // Opens when the quorum-th (second) region went bad.
            assert_eq!(n.started_at, ts(b, 60));
        }
        other => panic!("expected one Open, got {other:?}"),
    }
}

#[test]
fn quorum_closes_when_back_below_threshold() {
    // One region still down, but below a quorum of 2 → the combined incident
    // clears (the per-region policy is the one that keeps it open).
    let b = mbase();
    let t = Uuid::now_v7();
    let open = OpenIncident {
        id: Uuid::now_v7(),
        target_id: t,
        started_at: ts(b, 0),
        region: None,
        regions_down: Vec::new(),
    };
    let by_region = vec![
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 30), CheckStatus::Down),
                result(t, ts(b, 60), CheckStatus::Up),
                result(t, ts(b, 90), CheckStatus::Up),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 60), CheckStatus::Down),
                result(t, ts(b, 90), CheckStatus::Down),
            ],
        ),
    ];
    match decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 2).as_slice() {
        [Action::Close { incident_id, .. }] => assert_eq!(*incident_id, open.id),
        other => panic!("expected one Close, got {other:?}"),
    }
}

#[test]
fn quorum_clamps_to_live_region_count() {
    // quorum=3 but only 2 regions report → clamps to 2, so a both-down
    // outage still opens instead of waiting for a third region that
    // doesn't exist.
    let b = mbase();
    let t = Uuid::now_v7();
    let by_region = vec![
        (
            "eu".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Down),
                result(t, ts(b, 30), CheckStatus::Down),
            ],
        ),
        (
            "us".to_string(),
            vec![
                result(t, ts(b, 0), CheckStatus::Down),
                result(t, ts(b, 30), CheckStatus::Down),
            ],
        ),
    ];
    match decide_multi(t, &[], &by_region, 2, 3).as_slice() {
        [Action::Open(_)] => {}
        other => panic!("expected Open (quorum clamped to live count), got {other:?}"),
    }
}

// ── full writer tick with InMemoryIncidentStore ─────────────────────────

fn make_public_target(name: &str) -> Target {
    Target {
        id: Uuid::now_v7(),
        name: name.into(),
        check: CheckSpec::Http(HttpCheck {
            url: url::Url::parse("https://example.com/").unwrap(),
            method: HttpMethod::Get,
            timeout: StdDuration::from_secs(5),
            follow_redirects: false,
            max_redirects: 0,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: std::collections::HashMap::new(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        }),
        interval: StdDuration::from_secs(30),
        enabled: true,
        tags: vec![],
        alerts: TargetAlerts::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: crate::domain::WriteSource::Ui,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        plan_hold_at: None,
    }
}

async fn seed_results(sink: &InMemorySink, results: Vec<CheckResult>) {
    sink.write_batch(&results).await.expect("seed results");
}

fn writer(
    targets: Arc<InMemoryTargetStore>,
    sink: Arc<InMemorySink>,
    incidents: Arc<InMemoryIncidentStore>,
) -> IncidentWriter {
    let cfg = IncidentWriterConfig {
        tick_interval: StdDuration::from_secs(1),
        lookback: ChronoDuration::days(1),
        flap_threshold: 2,
        max_results_per_tick: 10_000,
        page_size: 256,
        max_concurrency: 4,
    };
    IncidentWriter::new(
        targets as Arc<dyn EnabledTargetStream>,
        sink as Arc<dyn crate::storage::ResultsStore>,
        incidents as Arc<dyn IncidentStore>,
        cfg,
    )
}

fn writer_with_lookback(
    targets: Arc<InMemoryTargetStore>,
    sink: Arc<InMemorySink>,
    incidents: Arc<InMemoryIncidentStore>,
    lookback: ChronoDuration,
) -> IncidentWriter {
    let cfg = IncidentWriterConfig {
        tick_interval: StdDuration::from_secs(1),
        lookback,
        flap_threshold: 2,
        max_results_per_tick: 10_000,
        page_size: 256,
        max_concurrency: 4,
    };
    IncidentWriter::new(
        targets as Arc<dyn EnabledTargetStream>,
        sink as Arc<dyn crate::storage::ResultsStore>,
        incidents as Arc<dyn IncidentStore>,
        cfg,
    )
}

#[test]
fn lookback_grows_with_target_interval() {
    let w = writer_with_lookback(
        Arc::new(InMemoryTargetStore::new()),
        Arc::new(InMemorySink::new()),
        Arc::new(InMemoryIncidentStore::new()),
        ChronoDuration::minutes(10),
    );
    let mut fast = make_public_target("fast");
    fast.interval = StdDuration::from_secs(30);
    fast.alert_confirmations = 2;
    assert_eq!(
        w.lookback_for(&fast),
        ChronoDuration::minutes(10),
        "fast monitor is bounded by the floor"
    );
    let mut hourly = make_public_target("cert");
    hourly.interval = StdDuration::from_secs(3600);
    hourly.alert_confirmations = 2;
    assert_eq!(
        w.lookback_for(&hourly),
        ChronoDuration::hours(4),
        "2 * confirmations * interval beats the floor for an hourly monitor"
    );
}

#[tokio::test]
async fn hourly_monitor_opens_despite_small_floor() {
    // tls_cert / domain_expiry are forced to 3600s. Two hourly failures sit
    // far outside a 10-min floor; the per-target 4h window catches both.
    let mut target = make_public_target("cert");
    target.interval = StdDuration::from_secs(3600);
    target.alert_confirmations = 2;
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    seed_results(
        &sink,
        vec![
            result(target_id, now - ChronoDuration::hours(2), CheckStatus::Down),
            result(target_id, now - ChronoDuration::hours(1), CheckStatus::Down),
        ],
    )
    .await;
    let w = writer_with_lookback(
        targets,
        sink,
        incidents.clone(),
        ChronoDuration::minutes(10),
    );
    w.tick_once().await.expect("tick");
    assert_eq!(incidents.insert_count(), 1);
}

#[tokio::test]
async fn slow_user_set_interval_opens_despite_small_floor() {
    // A user can set an interval well above the per-kind floor; the window
    // must follow the configured interval, not a fixed constant.
    let mut target = make_public_target("http-slow");
    target.interval = StdDuration::from_secs(600);
    target.alert_confirmations = 2;
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    // 600s interval → 40-min window; samples at 25 and 15 min are inside it
    // but outside the 10-min floor.
    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::minutes(25),
                CheckStatus::Down,
            ),
            result(
                target_id,
                now - ChronoDuration::minutes(15),
                CheckStatus::Down,
            ),
        ],
    )
    .await;
    let w = writer_with_lookback(
        targets,
        sink,
        incidents.clone(),
        ChronoDuration::minutes(10),
    );
    w.tick_once().await.expect("tick");
    assert_eq!(incidents.insert_count(), 1);
}

#[tokio::test]
async fn fast_monitor_ignores_results_older_than_its_window() {
    // Negative control: a 30s monitor's window is the 10-min floor, so two
    // failures spaced an hour apart fall outside it and must not open —
    // proving the window is interval-scoped, not unbounded.
    let mut target = make_public_target("fast");
    target.interval = StdDuration::from_secs(30);
    target.alert_confirmations = 2;
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    seed_results(
        &sink,
        vec![
            result(target_id, now - ChronoDuration::hours(2), CheckStatus::Down),
            result(target_id, now - ChronoDuration::hours(1), CheckStatus::Down),
        ],
    )
    .await;
    let w = writer_with_lookback(
        targets,
        sink,
        incidents.clone(),
        ChronoDuration::minutes(10),
    );
    w.tick_once().await.expect("tick");
    assert_eq!(incidents.insert_count(), 0);
}

#[tokio::test]
async fn tick_does_not_open_on_single_bad_then_recovery() {
    let target = make_public_target("api");
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::seconds(60),
                CheckStatus::Down,
            ),
            result(
                target_id,
                now - ChronoDuration::seconds(30),
                CheckStatus::Up,
            ),
        ],
    )
    .await;

    let w = writer(targets, sink, incidents.clone());
    w.tick_once().await.expect("tick");
    assert_eq!(incidents.insert_count(), 0);
    assert!(incidents.all_for(target_id).is_empty());
}

#[tokio::test]
async fn tick_opens_on_two_consecutive_bad() {
    let target = make_public_target("api");
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::seconds(60),
                CheckStatus::Down,
            ),
            result(
                target_id,
                now - ChronoDuration::seconds(30),
                CheckStatus::Down,
            ),
        ],
    )
    .await;

    let w = writer(targets, sink, incidents.clone());
    w.tick_once().await.expect("tick");
    let all = incidents.all_for(target_id);
    assert_eq!(all.len(), 1);
    assert!(all[0].ended_at.is_none());
    assert_eq!(all[0].status_at_start, CheckStatus::Down);
    assert_eq!(all[0].check_count, 2);
}

#[tokio::test]
async fn tick_closes_open_incident_on_two_consecutive_good() {
    // Simulates the realistic sequence: tick sees the bad run and opens
    // an incident; later results arrive showing recovery; next tick
    // observes the trailing up-run and closes.
    let target = make_public_target("api");
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());

    // Step 1: bad run only — tick opens the incident.
    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::seconds(120),
                CheckStatus::Down,
            ),
            result(
                target_id,
                now - ChronoDuration::seconds(90),
                CheckStatus::Down,
            ),
        ],
    )
    .await;
    let w = writer(targets, sink.clone(), incidents.clone());
    w.tick_once().await.expect("tick 1 opens");
    assert_eq!(incidents.insert_count(), 1, "first tick must open");
    let opened = incidents.all_for(target_id);
    assert_eq!(opened.len(), 1);
    assert!(opened[0].ended_at.is_none());

    // Step 2: recovery results show up — next tick closes.
    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::seconds(60),
                CheckStatus::Up,
            ),
            result(
                target_id,
                now - ChronoDuration::seconds(30),
                CheckStatus::Up,
            ),
        ],
    )
    .await;
    w.tick_once().await.expect("tick 2 closes");
    let all = incidents.all_for(target_id);
    assert_eq!(all.len(), 1);
    let inc = &all[0];
    assert!(inc.ended_at.is_some(), "incident must be closed");
    assert_eq!(inc.ended_at.unwrap(), now - ChronoDuration::seconds(60));
}

#[tokio::test]
async fn re_running_writer_with_no_new_data_is_noop() {
    let target = make_public_target("api");
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::seconds(60),
                CheckStatus::Down,
            ),
            result(
                target_id,
                now - ChronoDuration::seconds(30),
                CheckStatus::Down,
            ),
        ],
    )
    .await;

    let w = writer(targets, sink, incidents.clone());
    for _ in 0..5 {
        w.tick_once().await.expect("tick");
    }
    assert_eq!(incidents.insert_count(), 1, "must not double-insert");
    assert_eq!(incidents.close_count(), 0, "no close without recovery");
    assert_eq!(
        incidents.widen_count(),
        0,
        "nothing new to confirm, nothing written"
    );
}

#[tokio::test]
async fn re_running_after_close_is_noop() {
    let target = make_public_target("api");
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::seconds(120),
                CheckStatus::Down,
            ),
            result(
                target_id,
                now - ChronoDuration::seconds(90),
                CheckStatus::Down,
            ),
        ],
    )
    .await;
    let w = writer(targets, sink.clone(), incidents.clone());
    w.tick_once().await.expect("open");

    seed_results(
        &sink,
        vec![
            result(
                target_id,
                now - ChronoDuration::seconds(60),
                CheckStatus::Up,
            ),
            result(
                target_id,
                now - ChronoDuration::seconds(30),
                CheckStatus::Up,
            ),
        ],
    )
    .await;
    w.tick_once().await.expect("close");

    let baseline_inserts = incidents.insert_count();
    let baseline_closes = incidents.close_count();
    // Re-running shouldn't churn anything.
    for _ in 0..5 {
        w.tick_once().await.expect("tick");
    }
    assert_eq!(incidents.insert_count(), baseline_inserts);
    assert_eq!(incidents.close_count(), baseline_closes);
}

#[tokio::test]
async fn shutdown_cancels_run_loop() {
    let targets = Arc::new(InMemoryTargetStore::new());
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    let w = writer(targets, sink, incidents);
    let token = CancellationToken::new();
    let handle = {
        let token = token.clone();
        tokio::spawn(async move { w.run(token).await })
    };
    token.cancel();
    tokio::time::timeout(StdDuration::from_secs(2), handle)
        .await
        .expect("run did not exit within deadline")
        .expect("join");
}
