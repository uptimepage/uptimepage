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

    match decide_multi(target, &[], &by_region, 2, 2, ChronoDuration::zero())
        .into_iter()
        .next()
    {
        Some(Action::Open(new)) => {
            let sample = new.error_sample.expect("diagnostic sample");
            assert!(sample.contains("3/3 failing regions agree"), "{sample}");
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
fn cause_tally_counts_only_failing_regions() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let blocked = || {
        vec![
            akamai_blocked(target, base),
            akamai_blocked(target, ts(base, 30)),
        ]
    };
    let healthy = || vec![result(target, base, CheckStatus::Up)];
    let sample_for = |by_region: &[(String, Vec<CheckResult>)], quorum| match decide_multi(
        target,
        &[],
        by_region,
        2,
        quorum,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Open(new)] => new.error_sample.clone().expect("diagnostic sample"),
        other => panic!("expected Open, got {other:?}"),
    };

    let one_of_three = [
        ("eu".to_string(), blocked()),
        ("us".to_string(), healthy()),
        ("ap".to_string(), healthy()),
    ];
    let sample = sample_for(&one_of_three, 1);
    assert!(sample.contains("Akamai"), "{sample}");
    assert!(!sample.contains("regions agree"), "{sample}");

    let two_of_three = [
        ("eu".to_string(), blocked()),
        ("us".to_string(), blocked()),
        ("ap".to_string(), healthy()),
    ];
    let sample = sample_for(&two_of_three, 2);
    assert!(sample.contains("2/2 failing regions agree"), "{sample}");
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

    match decide_multi(target, &[], &by_region, 2, 2, ChronoDuration::zero())
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
            assert!(sample.contains("3/3 failing regions agree"), "{sample}");
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

    match decide_multi(target, &[], &by_region, 2, 2, ChronoDuration::zero())
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

    match decide_multi(target, &[], &by_region, 2, 2, ChronoDuration::zero())
        .into_iter()
        .next()
    {
        Some(Action::Open(new)) => {
            let sample = new.error_sample.expect("protocol sample");
            assert!(!sample.contains("Akamai"), "{sample}");
            assert!(!sample.contains("failing regions agree"), "{sample}");
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
    match decide_multi(t, &[], &by_region, 2, 2, ChronoDuration::zero())
        .into_iter()
        .next()
    {
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
        worst_status: CheckStatus::Down,
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
    assert!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &by_region,
            2,
            2,
            ChronoDuration::zero()
        )
        .is_empty()
    );

    // Its second failure lands: it joins.
    let by_region = vec![
        region("fra", down(&[0, 30])),
        region("us", down(&[10, 40])),
        region("hel", down(&[50, 80])),
    ];
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &by_region,
        2,
        2,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
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
            decide_multi(
                t,
                std::slice::from_ref(&full),
                &by_region,
                2,
                2,
                ChronoDuration::zero()
            )
            .is_empty(),
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
        worst_status: CheckStatus::Down,
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
        worst_status: CheckStatus::Down,
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
        worst_status: CheckStatus::Down,
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
        worst_status: CheckStatus::Down,
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
        worst_status: CheckStatus::Down,
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
        worst_status: CheckStatus::Down,
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
        worst_status: CheckStatus::Down,
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
    match decide_multi(t, &[], &by_region, 2, 1, ChronoDuration::zero()).as_slice() {
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
        worst_status: CheckStatus::Down,
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
    let actions = decide_multi(t, &[open], &by_region, 2, 1, ChronoDuration::zero());
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
        worst_status: CheckStatus::Down,
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
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &by_region,
        2,
        1,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
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
        decide_multi(t, &[], &one_bad, 2, 2, ChronoDuration::zero()).is_empty(),
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
    match decide_multi(t, &[], &two_bad, 2, 2, ChronoDuration::zero()).as_slice() {
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
        worst_status: CheckStatus::Down,
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
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &by_region,
        2,
        2,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
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
    match decide_multi(t, &[], &by_region, 2, 3, ChronoDuration::zero()).as_slice() {
        [Action::Open(_)] => {}
        other => panic!("expected Open (quorum clamped to live count), got {other:?}"),
    }
}

// ── recovery period ─────────────────────────────────────────────────────

fn open_since(target_id: Uuid, started_at: DateTime<Utc>, regions_down: &[&str]) -> OpenIncident {
    OpenIncident {
        id: Uuid::now_v7(),
        target_id,
        started_at,
        region: None,
        regions_down: regions_down.iter().map(|r| r.to_string()).collect(),
        worst_status: CheckStatus::Down,
    }
}

fn run(t: Uuid, base: DateTime<Utc>, from: i64, to: i64, status: CheckStatus) -> Vec<CheckResult> {
    (from..=to)
        .step_by(30)
        .map(|o| result(t, ts(base, o), status))
        .collect()
}

#[test]
fn a_recovery_closes_only_once_it_has_held() {
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &[]);
    let hold = ChronoDuration::minutes(5);
    let up_for = |secs: i64| {
        let mut results = run(t, base, 0, 30, CheckStatus::Down);
        results.extend(run(t, base, 60, 60 + secs, CheckStatus::Up));
        vec![(String::new(), results)]
    };

    assert!(
        decide_multi(t, std::slice::from_ref(&open), &up_for(240), 2, 1, hold).is_empty(),
        "four minutes up is not yet five"
    );
    match decide_multi(t, std::slice::from_ref(&open), &up_for(300), 2, 1, hold).as_slice() {
        [Action::Close { ended_at, .. }] => {
            assert_eq!(*ended_at, ts(base, 60), "the hold is not downtime");
        }
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn a_failure_inside_the_hold_keeps_one_incident() {
    // A service that comes back for three minutes and falls over again is one
    // outage: the second failure restarts the hold instead of opening anew.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &[]);
    let hold = ChronoDuration::minutes(5);
    let mut results = run(t, base, 0, 30, CheckStatus::Down);
    results.extend(run(t, base, 60, 240, CheckStatus::Up));
    results.extend(run(t, base, 270, 300, CheckStatus::Down));
    results.extend(run(t, base, 330, 600, CheckStatus::Up));
    let by_region = vec![(String::new(), results.clone())];
    assert!(
        decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 1, hold).is_empty(),
        "the hold restarted at the second recovery"
    );

    results.extend(run(t, base, 630, 630, CheckStatus::Up));
    let by_region = vec![(String::new(), results)];
    match decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 1, hold).as_slice() {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 330)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn a_flapping_quorum_folds_into_one_incident_while_one_region_stays_up() {
    // Two of three regions fail, recover, and fail again; the third never
    // fails. Its long good run is not a recovery, so it neither closes the
    // incident early nor dates the end.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra", "us"]);
    let hold = ChronoDuration::minutes(10);
    let flapping = |tail: i64| {
        let mut results = run(t, base, 0, 30, CheckStatus::Down);
        results.extend(run(t, base, 60, 300, CheckStatus::Up));
        results.extend(run(t, base, 330, 390, CheckStatus::Down));
        results.extend(run(t, base, 420, tail, CheckStatus::Up));
        results
    };
    let regions = |tail: i64| {
        vec![
            ("fra".to_string(), flapping(tail)),
            ("hel".to_string(), run(t, base, 0, tail, CheckStatus::Up)),
            ("us".to_string(), flapping(tail)),
        ]
    };

    assert!(
        decide_multi(t, std::slice::from_ref(&open), &regions(990), 2, 2, hold).is_empty(),
        "nine and a half minutes since the second recovery"
    );
    match decide_multi(t, std::slice::from_ref(&open), &regions(1_020), 2, 2, hold).as_slice() {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 420)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn a_region_up_throughout_does_not_close_an_outage_the_others_are_still_in() {
    // The window opens long after the incident did, so the region that never
    // failed has a good run reaching back past every other region's recovery.
    // One good check from the failing regions is not a confirmed recovery.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra", "us"]);
    let failing_then = |ups: i64| {
        let mut results = run(t, base, 1_200, 1_470, CheckStatus::Down);
        results.extend(run(t, base, 1_500, 1_500 + 30 * (ups - 1), CheckStatus::Up));
        results
    };
    let regions = |ups: i64| {
        vec![
            ("fra".to_string(), failing_then(ups)),
            (
                "hel".to_string(),
                run(t, base, 1_200, 1_530, CheckStatus::Up),
            ),
            ("us".to_string(), failing_then(ups)),
        ]
    };

    assert!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &regions(1),
            2,
            2,
            ChronoDuration::zero()
        )
        .is_empty(),
        "a single good check each"
    );
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &regions(2),
        2,
        2,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 1_500)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn a_lone_region_failing_during_the_hold_is_not_part_of_the_outage() {
    // fra and us were the outage and have recovered; hel confirms a failure of
    // its own while the hold runs. Below quorum, it neither widens the
    // breakdown into a total outage nor ends the hold.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra", "us"]);
    let hold = ChronoDuration::minutes(5);
    let recovered = || {
        let mut results = run(t, base, 0, 30, CheckStatus::Down);
        results.extend(run(t, base, 60, 240, CheckStatus::Up));
        results
    };
    let mut hel = run(t, base, 0, 150, CheckStatus::Up);
    hel.extend(run(t, base, 180, 240, CheckStatus::Down));
    let by_region = vec![
        ("fra".to_string(), recovered()),
        ("hel".to_string(), hel),
        ("us".to_string(), recovered()),
    ];
    assert!(
        decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 2, hold).is_empty(),
        "no Widen, and the hold is still running"
    );
}

#[test]
fn a_region_still_failing_alone_does_not_keep_the_outage_open() {
    // fra is blocked for good and us recovered before the window: one region
    // down is below the majority, so the outage is over even though nothing
    // in view saw it end.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra", "us"]);
    let by_region = vec![
        (
            "fra".to_string(),
            run(t, base, 1_200, 1_500, CheckStatus::Down),
        ),
        (
            "hel".to_string(),
            run(t, base, 1_200, 1_500, CheckStatus::Up),
        ),
        (
            "us".to_string(),
            run(t, base, 1_200, 1_500, CheckStatus::Up),
        ),
    ];
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &by_region,
        2,
        2,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 1_200)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn a_stray_failed_check_neither_restarts_the_hold_nor_moves_the_end() {
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &[]);
    let hold = ChronoDuration::minutes(10);
    let mut results = run(t, base, 0, 30, CheckStatus::Down);
    results.extend(run(t, base, 60, 270, CheckStatus::Up));
    results.extend(run(t, base, 300, 300, CheckStatus::Down));
    results.extend(run(t, base, 330, 660, CheckStatus::Up));
    let by_region = vec![(String::new(), results)];
    match decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 1, hold).as_slice() {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 60)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn another_regions_blip_is_not_the_recovery_of_the_one_that_failed() {
    // Any-down: fra has been failing for two hours and passes one check; hel
    // had a single timeout minutes ago. fra has not confirmed a recovery.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra"]);
    let mut fra = run(t, base, 7_000, 7_170, CheckStatus::Down);
    fra.extend(run(t, base, 7_200, 7_200, CheckStatus::Up));
    let mut hel = run(t, base, 7_000, 7_050, CheckStatus::Up);
    hel.extend(run(t, base, 7_080, 7_080, CheckStatus::Down));
    hel.extend(run(t, base, 7_110, 7_200, CheckStatus::Up));
    let by_region = vec![("fra".to_string(), fra), ("hel".to_string(), hel)];
    assert!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &by_region,
            2,
            1,
            ChronoDuration::zero()
        )
        .is_empty()
    );
}

#[test]
fn a_lone_region_recovering_during_the_hold_does_not_redate_the_end() {
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra", "us"]);
    let hold = ChronoDuration::minutes(5);
    let recovered = || {
        let mut results = run(t, base, 0, 30, CheckStatus::Down);
        results.extend(run(t, base, 60, 360, CheckStatus::Up));
        results
    };
    let mut hel = run(t, base, 0, 150, CheckStatus::Up);
    hel.extend(run(t, base, 180, 240, CheckStatus::Down));
    hel.extend(run(t, base, 270, 360, CheckStatus::Up));
    let by_region = vec![
        ("fra".to_string(), recovered()),
        ("hel".to_string(), hel),
        ("us".to_string(), recovered()),
    ];
    match decide_multi(t, std::slice::from_ref(&open), &by_region, 2, 2, hold).as_slice() {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 60)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn a_region_that_stops_reporting_is_no_evidence_of_a_recovery() {
    // fra and us opened it; us has gone quiet while fra still fails. With us
    // out of the vote one failing region is below the majority, but hel never
    // failed, so nothing has seen the outage end.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra", "us"]);
    let fra = run(t, base, 1_200, 1_500, CheckStatus::Down);
    let hel = run(t, base, 1_200, 1_500, CheckStatus::Up);
    let quiet = vec![
        ("fra".to_string(), fra.clone()),
        ("hel".to_string(), hel.clone()),
    ];
    assert!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &quiet,
            2,
            2,
            ChronoDuration::zero()
        )
        .is_empty()
    );

    let back = vec![
        ("fra".to_string(), fra),
        ("hel".to_string(), hel),
        (
            "us".to_string(),
            run(t, base, 1_440, 1_500, CheckStatus::Up),
        ),
    ];
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &back,
        2,
        2,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 1_440)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn a_quiet_region_does_not_backdate_the_recovery_of_the_one_still_reporting() {
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra", "us"]);
    let mut fra = run(t, base, 1_200, 1_290, CheckStatus::Down);
    fra.extend(run(t, base, 1_320, 1_500, CheckStatus::Up));
    let by_region = vec![
        ("fra".to_string(), fra),
        (
            "hel".to_string(),
            run(t, base, 1_200, 1_500, CheckStatus::Up),
        ),
    ];
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &by_region,
        2,
        2,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 1_320)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn with_every_failing_region_quiet_the_regions_still_reporting_decide() {
    // fra was the only region failing and has stopped reporting, as when it is
    // taken off the monitor; hel and us both pass.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra"]);
    let by_region = vec![
        (
            "hel".to_string(),
            run(t, base, 1_200, 1_500, CheckStatus::Up),
        ),
        (
            "us".to_string(),
            run(t, base, 1_230, 1_500, CheckStatus::Up),
        ),
    ];
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &by_region,
        2,
        1,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 1_230)),
        other => panic!("expected Close, got {other:?}"),
    }
}

#[test]
fn with_every_failing_region_quiet_a_reporter_not_yet_confirmed_up_keeps_it_open() {
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra"]);
    let by_region = vec![
        (
            "hel".to_string(),
            run(t, base, 1_200, 1_500, CheckStatus::Up),
        ),
        (
            "us".to_string(),
            run(t, base, 1_500, 1_500, CheckStatus::Down),
        ),
    ];
    assert!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &by_region,
            2,
            1,
            ChronoDuration::zero()
        )
        .is_empty()
    );
}

#[test]
fn with_every_failing_region_quiet_the_hold_runs_from_the_last_reporter_up() {
    // us failed on its own, below the majority, and passes again from 1_320:
    // the regions still reporting have only all been up since then.
    let base = mbase();
    let t = Uuid::now_v7();
    let open = open_since(t, ts(base, 0), &["fra"]);
    let hold = ChronoDuration::minutes(5);
    let window = |to| {
        let mut us = run(t, base, 1_200, 1_290, CheckStatus::Down);
        us.extend(run(t, base, 1_320, to, CheckStatus::Up));
        vec![
            ("hel".to_string(), run(t, base, 1_200, to, CheckStatus::Up)),
            ("us".to_string(), us),
        ]
    };
    assert!(decide_multi(t, std::slice::from_ref(&open), &window(1_500), 2, 2, hold).is_empty());
    match decide_multi(t, std::slice::from_ref(&open), &window(1_620), 2, 2, hold).as_slice() {
        [Action::Close { ended_at, .. }] => assert_eq!(*ended_at, ts(base, 1_320)),
        other => panic!("expected Close, got {other:?}"),
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
        recovery_period_secs: 0,
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

#[test]
fn lookback_reaches_back_over_the_recovery_period() {
    let w = writer_with_lookback(
        Arc::new(InMemoryTargetStore::new()),
        Arc::new(InMemorySink::new()),
        Arc::new(InMemoryIncidentStore::new()),
        ChronoDuration::minutes(10),
    );
    let mut held = make_public_target("held");
    held.interval = StdDuration::from_secs(60);
    held.alert_confirmations = 2;
    held.recovery_period_secs = 900;
    assert_eq!(
        w.lookback_for(&held),
        ChronoDuration::seconds(2 * (2 * 60 + 900)),
        "the start of a recovery that has held must still be in view"
    );
}

#[tokio::test]
async fn tick_holds_the_close_for_the_recovery_period() {
    let mut target = make_public_target("api");
    target.recovery_period_secs = 300;
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    let at =
        |secs_ago: i64, status| result(target_id, now - ChronoDuration::seconds(secs_ago), status);

    seed_results(
        &sink,
        vec![at(600, CheckStatus::Down), at(570, CheckStatus::Down)],
    )
    .await;
    let w = writer(targets, sink.clone(), incidents.clone());
    w.tick_once().await.expect("tick opens");
    assert_eq!(incidents.insert_count(), 1);

    seed_results(
        &sink,
        vec![
            at(540, CheckStatus::Up),
            at(510, CheckStatus::Up),
            at(480, CheckStatus::Up),
        ],
    )
    .await;
    w.tick_once().await.expect("tick holds");
    assert!(
        incidents.all_for(target_id)[0].ended_at.is_none(),
        "a minute up is inside the hold"
    );

    seed_results(
        &sink,
        (0..9).map(|i| at(450 - 30 * i, CheckStatus::Up)).collect(),
    )
    .await;
    w.tick_once().await.expect("tick closes");
    let all = incidents.all_for(target_id);
    assert_eq!(all.len(), 1, "one incident throughout");
    assert_eq!(all[0].ended_at, Some(now - ChronoDuration::seconds(540)));
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

/// A human resolved the incident while the monitor was still failing. The
/// downs that opened it are still inside the lookback, and must not open it
/// again; only failures observed after the resolution may.
#[tokio::test]
async fn a_resolution_draws_a_line_the_old_evidence_cannot_cross() {
    let target = make_public_target("api");
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    let at = |secs_ago: i64| now - ChronoDuration::seconds(secs_ago);
    seed_results(
        &sink,
        vec![
            result(target_id, at(300), CheckStatus::Down),
            result(target_id, at(240), CheckStatus::Down),
        ],
    )
    .await;
    let w = writer(targets, sink.clone(), incidents.clone());
    w.tick_once().await.expect("open");
    let opened = incidents.all_for(target_id);
    assert_eq!(opened.len(), 1);

    // Resolved by hand at -180s, with no recovery in the results.
    let org = OrgId(Uuid::nil());
    assert!(incidents.close(org, opened[0].id, at(180)).await.unwrap());
    for _ in 0..3 {
        w.tick_once().await.expect("tick after resolve");
    }
    assert_eq!(
        incidents.insert_count(),
        1,
        "the downs from before the resolution must not reopen the incident"
    );

    // One fresh failure is not yet a confirmed run.
    seed_results(&sink, vec![result(target_id, at(120), CheckStatus::Down)]).await;
    w.tick_once().await.expect("tick on one fresh down");
    assert_eq!(incidents.insert_count(), 1);

    // Two are, and the new incident starts at the first of them, not at the
    // old evidence.
    seed_results(&sink, vec![result(target_id, at(60), CheckStatus::Down)]).await;
    w.tick_once().await.expect("tick on a confirmed fresh run");
    let all = incidents.all_for(target_id);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].started_at, at(120));
    assert!(all[1].ended_at.is_none());
}

/// The writer loaded its evidence, then a human resolved the incident before
/// it wrote. The store refuses the row, so the tick cannot reopen from what
/// the resolution already covered.
#[tokio::test]
async fn the_store_refuses_an_open_from_evidence_a_resolution_covered() {
    let target_id = Uuid::now_v7();
    let org = OrgId(Uuid::nil());
    let now = Utc::now();
    let incidents = InMemoryIncidentStore::new();
    let first = incidents
        .insert_open(org, new_open(target_id, now - ChronoDuration::seconds(300)))
        .await
        .unwrap()
        .expect("first open");
    assert!(
        incidents
            .close(org, first, now - ChronoDuration::seconds(100))
            .await
            .unwrap()
    );

    let stale = incidents
        .insert_open(org, new_open(target_id, now - ChronoDuration::seconds(300)))
        .await
        .unwrap();
    assert!(stale.is_none(), "evidence from before the close is refused");
    let fresh = incidents
        .insert_open(org, new_open(target_id, now - ChronoDuration::seconds(50)))
        .await
        .unwrap();
    assert!(fresh.is_some(), "evidence after the close opens");
}

fn new_open(target_id: Uuid, started_at: DateTime<Utc>) -> NewOpenIncident {
    NewOpenIncident {
        target_id,
        started_at,
        status_at_start: CheckStatus::Down,
        check_count: 2,
        error_sample: None,
        region: None,
        regions_down: vec![],
        regions_up: vec![],
    }
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

// ── escalation and manual monitors ──────────────────────────────────────

fn open_at(target_id: Uuid, started_at: DateTime<Utc>, worst: CheckStatus) -> OpenIncident {
    OpenIncident {
        id: Uuid::now_v7(),
        target_id,
        started_at,
        region: None,
        regions_down: Vec::new(),
        worst_status: worst,
    }
}

/// A degraded outage that turns hard down raises the incident, so the status
/// page stops calling a major outage degraded performance.
#[test]
fn a_worse_confirmed_status_escalates_the_open_incident() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    let open = open_at(t, ts(base, 0), CheckStatus::Degraded);
    let mut down = result(t, ts(base, 60), CheckStatus::Down);
    down.error = Some("marked down: all trunks down".into());
    let by_region = vec![(
        String::new(),
        vec![result(t, ts(base, 0), CheckStatus::Degraded), down],
    )];
    assert_eq!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &by_region,
            1,
            1,
            ChronoDuration::zero()
        ),
        vec![Action::Escalate {
            incident_id: open.id,
            error_sample: Some("marked down: all trunks down".into()),
        }]
    );
}

/// A diagnosed failure escalates with the diagnosis, not the bare status line,
/// the same cause an incident opening on it would carry.
#[test]
fn an_escalation_keeps_the_diagnosed_cause() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    let open = open_at(t, ts(base, 0), CheckStatus::Degraded);
    let by_region = vec![(
        String::new(),
        vec![
            result(t, ts(base, 0), CheckStatus::Degraded),
            tunnel_down(t, ts(base, 60)),
        ],
    )];
    let opening = match decide_multi(t, &[], &by_region, 1, 1, ChronoDuration::zero()).as_slice() {
        [Action::Open(new)] => new.error_sample.clone(),
        other => panic!("expected Open, got {other:?}"),
    };
    match decide_multi(
        t,
        std::slice::from_ref(&open),
        &by_region,
        1,
        1,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Escalate { error_sample, .. }] => {
            assert_eq!(*error_sample, opening);
            assert_ne!(error_sample.as_deref(), Some("unexpected status 530"));
        }
        other => panic!("expected Escalate, got {other:?}"),
    }
}

#[test]
fn escalation_diagnostics_require_quorum_and_keep_region_agreement() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let target = Uuid::now_v7();
    let regions = ["eu-helsinki", "us-east", "apac-sg"];
    let mut open = open_at(target, base, CheckStatus::Degraded);
    open.regions_down = regions.iter().map(|region| region.to_string()).collect();

    for diagnosed_regions in 1..=3 {
        let by_region: Vec<_> = regions
            .iter()
            .enumerate()
            .map(|(index, region)| {
                let mut down = tunnel_down(target, ts(base, 60));
                if index >= diagnosed_regions {
                    down.diagnostic = None;
                }
                (
                    region.to_string(),
                    vec![result(target, base, CheckStatus::Degraded), down],
                )
            })
            .collect();
        let opening =
            match decide_multi(target, &[], &by_region, 2, 2, ChronoDuration::zero()).as_slice() {
                [Action::Open(new)] => new.error_sample.clone(),
                other => panic!("expected Open, got {other:?}"),
            };
        match decide_multi(
            target,
            std::slice::from_ref(&open),
            &by_region,
            2,
            2,
            ChronoDuration::zero(),
        )
        .as_slice()
        {
            [
                Action::Escalate {
                    incident_id,
                    error_sample,
                },
            ] => {
                assert_eq!(*incident_id, open.id);
                assert_eq!(*error_sample, opening);
                let sample = error_sample.as_deref().expect("escalation cause");
                if diagnosed_regions >= 2 {
                    assert!(sample.contains("origin tunnel down behind the Cloudflare edge"));
                    assert!(sample.contains("restart the tunnel daemon"));
                    assert!(
                        sample.contains(&format!("{diagnosed_regions}/3 failing regions agree"))
                    );
                } else {
                    assert_eq!(sample, "unexpected status 530");
                }
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }
}

/// Escalation takes the quorum an opening needs: one region going hard down
/// while the others recover is not an outage the policy would call.
#[test]
fn one_region_below_quorum_does_not_escalate() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    let regions = ["eu-helsinki", "us-east", "apac-sg"];
    let mut open = open_at(t, base, CheckStatus::Degraded);
    open.regions_down = regions.iter().map(|r| r.to_string()).collect();
    let down = |n: usize| -> Vec<(String, Vec<CheckResult>)> {
        regions
            .iter()
            .enumerate()
            .map(|(i, region)| {
                let last = if i < n {
                    CheckStatus::Down
                } else {
                    CheckStatus::Up
                };
                (
                    region.to_string(),
                    vec![
                        result(t, ts(base, 0), CheckStatus::Degraded),
                        result(t, ts(base, 60), CheckStatus::Degraded),
                        result(t, ts(base, 120), last),
                    ],
                )
            })
            .collect()
    };
    assert_eq!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &down(1),
            2,
            2,
            ChronoDuration::zero()
        ),
        vec![],
        "one recovery check closes nothing, and one region escalates nothing"
    );
    assert!(matches!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &down(2),
            2,
            2,
            ChronoDuration::zero()
        )
        .as_slice(),
        [Action::Escalate { .. }]
    ));
}

/// An error is as often our probe as the service, so it never turns a
/// degraded incident into an outage.
#[test]
fn an_error_does_not_escalate_a_degraded_incident() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    let open = open_at(t, ts(base, 0), CheckStatus::Degraded);
    let by_region = vec![(
        String::new(),
        vec![
            result(t, ts(base, 0), CheckStatus::Degraded),
            result(t, ts(base, 60), CheckStatus::Error),
        ],
    )];
    assert!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &by_region,
            1,
            1,
            ChronoDuration::zero()
        )
        .is_empty()
    );
}

/// The cause is the newest failure's, so a second set landing before the
/// writer's tick is the one the incident opens with.
#[test]
fn an_incident_opens_with_the_newest_cause() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    let mut first = result(t, ts(base, 0), CheckStatus::Degraded);
    first.error = Some("marked degraded: one trunk of two".into());
    let mut second = result(t, ts(base, 10), CheckStatus::Down);
    second.error = Some("marked down: all trunks down".into());
    match decide_multi(
        t,
        &[],
        &[(String::new(), vec![first, second])],
        1,
        1,
        ChronoDuration::zero(),
    )
    .as_slice()
    {
        [Action::Open(new)] => {
            assert_eq!(new.status_at_start, CheckStatus::Down);
            assert_eq!(
                new.error_sample.as_deref(),
                Some("marked down: all trunks down")
            );
        }
        other => panic!("expected Open, got {other:?}"),
    }
}

/// The row keeps the worst the outage was: an improvement is the close's
/// business, and an equal status writes nothing.
#[test]
fn escalation_never_lowers_and_never_repeats() {
    let base = Utc.with_ymd_and_hms(2026, 5, 13, 12, 0, 0).unwrap();
    let t = Uuid::now_v7();
    let open = open_at(t, ts(base, 0), CheckStatus::Down);
    let by_region = vec![(
        String::new(),
        vec![
            result(t, ts(base, 0), CheckStatus::Down),
            result(t, ts(base, 60), CheckStatus::Degraded),
        ],
    )];
    assert!(
        decide_multi(
            t,
            std::slice::from_ref(&open),
            &by_region,
            1,
            1,
            ChronoDuration::zero()
        )
        .is_empty()
    );
}

/// As create stores it: the operator's set is the confirmation.
fn make_manual_target(name: &str) -> Target {
    Target {
        check: CheckSpec::Manual(crate::domain::ManualCheck {}),
        interval: StdDuration::from_secs(60),
        alert_confirmations: 1,
        ..make_public_target(name)
    }
}

#[tokio::test]
async fn a_manual_monitor_opens_on_its_first_bad_result() {
    let target = make_manual_target("sip trunks");
    let target_id = target.id;
    let now = Utc::now();
    let targets = Arc::new(InMemoryTargetStore::from_vec(vec![target]));
    let sink = Arc::new(InMemorySink::new());
    let incidents = Arc::new(InMemoryIncidentStore::new());
    seed_results(
        &sink,
        vec![result(
            target_id,
            now - ChronoDuration::seconds(5),
            CheckStatus::Degraded,
        )],
    )
    .await;
    let w = writer(targets, sink.clone(), incidents.clone());
    w.tick_once().await.expect("tick");
    assert_eq!(incidents.insert_count(), 1);
    assert_eq!(
        incidents.all_for(target_id)[0].status_at_start,
        CheckStatus::Degraded
    );

    seed_results(
        &sink,
        vec![result(
            target_id,
            now - ChronoDuration::seconds(2),
            CheckStatus::Down,
        )],
    )
    .await;
    w.tick_once().await.expect("tick");
    assert_eq!(incidents.insert_count(), 1, "still the same outage");
    assert_eq!(
        incidents.all_for(target_id)[0].status_at_start,
        CheckStatus::Down,
        "raised from degraded"
    );

    seed_results(&sink, vec![result(target_id, now, CheckStatus::Up)]).await;
    w.tick_once().await.expect("tick");
    assert_eq!(incidents.close_count(), 1, "one up closes it");
}
