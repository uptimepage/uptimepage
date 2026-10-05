//! Storage contract for browser-flow run telemetry: the trace round-trips as
//! parallel arrays, both retention windows are stamped from the org's plan, and
//! the page snapshot expires ahead of the run that carries it.

use crate::common;

use chrono::{Duration, Utc};
use clickhouse::Row;
use serde::Deserialize;
use uuid::Uuid;

use uptimepage::domain::CheckStatus;
use uptimepage::domain::agent_wire::{
    ConsoleLine, FlowEvidence, FlowRunRecord, StepOutcome, StepTrace,
};
use uptimepage::storage::traits::FlowRunSink;
use uptimepage::storage::{ClampedRange, ClickhouseFlowRunSink, TimeRange};

/// Wide enough to hold every run these tests write.
fn any_time() -> ClampedRange {
    ClampedRange::unclamped(TimeRange {
        from: Utc::now() - Duration::days(365),
        to: Utc::now() + Duration::days(1),
    })
}

fn step(op: &str, outcome: StepOutcome, ms: u32) -> StepTrace {
    StepTrace {
        op: op.into(),
        outcome,
        duration_ms: ms,
    }
}

fn failed_login(org_id: Uuid, target_id: Uuid) -> FlowRunRecord {
    FlowRunRecord {
        org_id,
        target_id,
        timestamp: Utc::now(),
        status: CheckStatus::Down,
        duration_ms: 2570,
        error: Some("step 4/5 assert_url: url does not contain \"/secure\"".into()),
        steps: vec![
            step("fill", StepOutcome::Passed, 12),
            step("fill", StepOutcome::Passed, 9),
            step("click", StepOutcome::Passed, 41),
            step("assert_url", StepOutcome::Failed, 2000),
            step("assert_text", StepOutcome::Skipped, 0),
        ],
        evidence: Some(FlowEvidence {
            final_url: Some("https://app.example.com/login".into()),
            title: Some("Sign in".into()),
            text_snippet: Some("Your password is invalid!".into()),
            console: vec![ConsoleLine {
                level: "error".into(),
                text: "token expired".into(),
            }],
        }),
    }
}

#[derive(Debug, Row, Deserialize)]
struct StoredRun {
    status: i8,
    duration_ms: u32,
    stopped_step: Option<u16>,
    error: String,
    step_op: Vec<String>,
    step_outcome: Vec<i8>,
    step_ms: Vec<u32>,
    final_url: String,
    title: String,
    text_snippet: String,
    console_text: Vec<String>,
    evidence_days: u16,
    ttl_days: u16,
}

const SELECT: &str = "SELECT status, duration_ms, stopped_step, error, step_op, step_outcome, \
     step_ms, final_url, title, text_snippet, console_text, evidence_days, ttl_days \
     FROM flow_runs WHERE org_id = ? ORDER BY timestamp";

async fn fetch(client: &clickhouse::Client, org_id: Uuid) -> Vec<StoredRun> {
    client
        .query(SELECT)
        .bind(org_id)
        .fetch_all::<StoredRun>()
        .await
        .expect("read flow_runs")
}

#[tokio::test]
#[ignore]
async fn a_failed_run_stores_its_trace_and_page_alongside_the_verdict() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );
    sink.write_runs(&[failed_login(org_id, target_id)]).await;

    let rows = fetch(&client, org_id).await;
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.status, CheckStatus::Down.as_enum8());
    assert_eq!(r.duration_ms, 2570);
    assert_eq!(
        r.stopped_step,
        Some(3),
        "the step the run stopped on is derived from the trace, 0-based"
    );
    assert!(r.error.contains("assert_url"));

    assert_eq!(
        r.step_op,
        vec!["fill", "fill", "click", "assert_url", "assert_text"]
    );
    assert_eq!(
        r.step_outcome,
        vec![1, 1, 1, 2, 3],
        "passed / failed / skipped round-trip as the declared enum"
    );
    assert_eq!(r.step_ms, vec![12, 9, 41, 2000, 0]);

    assert_eq!(r.final_url, "https://app.example.com/login");
    assert_eq!(r.title, "Sign in");
    assert_eq!(r.text_snippet, "Your password is invalid!");
    assert_eq!(r.console_text, vec!["token expired"]);

    // An org the TTL snapshot has never seen falls back to the column defaults
    // rather than over-retaining.
    assert_eq!(r.ttl_days, 30);
    assert_eq!(r.evidence_days, 7);
}

#[tokio::test]
#[ignore]
async fn a_passing_run_stores_the_trace_and_no_page() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );
    sink.write_runs(&[FlowRunRecord {
        org_id,
        target_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        status: CheckStatus::Up,
        duration_ms: 1900,
        error: None,
        steps: vec![
            step("fill", StepOutcome::Passed, 12),
            step("assert_url", StepOutcome::Passed, 300),
        ],
        evidence: None,
    }])
    .await;

    let rows = fetch(&client, org_id).await;
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.stopped_step, None, "nothing failed, so nothing is blamed");
    assert_eq!(r.step_outcome, vec![1, 1]);
    assert!(r.final_url.is_empty());
    assert!(r.text_snippet.is_empty());
    assert!(r.console_text.is_empty());
    assert_eq!(
        r.step_ms,
        vec![12, 300],
        "durations are kept on a pass — that is what makes a step trend"
    );
}

// The whole point of the two windows: a run old enough to have shed its page
// snapshot still answers what ran and how long it took.
#[tokio::test]
#[ignore]
async fn page_evidence_expires_while_the_trace_survives() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );

    let mut stale = failed_login(org_id, target_id);
    stale.timestamp = Utc::now() - Duration::days(10);
    let fresh = failed_login(org_id, target_id);
    sink.write_runs(&[stale, fresh]).await;

    // TTLs are applied on merge, so force one rather than waiting for it.
    client
        .query("OPTIMIZE TABLE flow_runs FINAL")
        .execute()
        .await
        .expect("optimize flow_runs");

    let rows = fetch(&client, org_id).await;
    assert_eq!(rows.len(), 2, "the row itself lives its full ttl_days");
    let (old, new) = (&rows[0], &rows[1]);

    assert!(
        old.text_snippet.is_empty() && old.final_url.is_empty() && old.title.is_empty(),
        "page content past evidence_days must be gone: {old:?}"
    );
    assert!(old.console_text.is_empty());
    assert_eq!(
        old.step_op.len(),
        5,
        "the trace carries no page content, so it stays"
    );
    assert_eq!(old.step_outcome, vec![1, 1, 1, 2, 3]);
    assert_eq!(old.stopped_step, Some(3));

    assert_eq!(
        new.text_snippet, "Your password is invalid!",
        "a run inside the window keeps its page"
    );
}

#[tokio::test]
#[ignore]
async fn the_monitor_page_reads_runs_newest_first() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );

    let mut older = failed_login(org_id, target_id);
    older.timestamp = Utc::now() - Duration::days(10);
    let mut passing = failed_login(org_id, target_id);
    passing.timestamp = Utc::now();
    passing.status = CheckStatus::Up;
    passing.error = None;
    passing.evidence = None;
    passing.steps = vec![step("fill", StepOutcome::Passed, 12)];
    sink.write_runs(&[older, passing]).await;
    client
        .query("OPTIMIZE TABLE flow_runs FINAL")
        .execute()
        .await
        .expect("optimize flow_runs");

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    let runs = store
        .flow_runs(OrgId(org_id), target_id, any_time(), None, 50)
        .await
        .expect("read flow runs");

    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].status, CheckStatus::Up, "newest first");
    assert!(runs[0].error.is_none());
    assert!(runs[0].evidence.is_none(), "a pass captured no page");
    assert_eq!(runs[0].stopped_step, None);
    assert_eq!(runs[0].steps.len(), 1);

    let old = &runs[1];
    assert_eq!(old.status, CheckStatus::Down);
    assert_eq!(old.region, "eu-helsinki");
    assert_eq!(old.stopped_step, Some(3));
    assert_eq!(old.steps.len(), 5);
    assert_eq!(old.steps[3].outcome, StepOutcome::Failed);
    assert_eq!(old.steps[4].outcome, StepOutcome::Skipped);
    assert!(
        old.error
            .as_deref()
            .is_some_and(|e| e.contains("assert_url"))
    );
    assert!(
        old.evidence.is_none(),
        "past its window the page reads absent, not blank: {:?}",
        old.evidence
    );
    assert!(
        old.evidence_expired,
        "the window is what took it, which the panel must be able to say"
    );
    assert!(!runs[0].evidence_expired, "a fresh run has lost nothing");
}

// Region is a filter on the same key prefix, not a post-filter.
#[tokio::test]
#[ignore]
async fn reading_one_region_excludes_the_others() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let ttl = uptimepage::storage::OrgTtlDays::new();
    for region in ["eu-helsinki", "us-east"] {
        ClickhouseFlowRunSink::new(client.clone(), region.into(), ttl.clone())
            .write_runs(&[failed_login(org_id, target_id)])
            .await;
    }

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    let all = store
        .flow_runs(OrgId(org_id), target_id, any_time(), None, 50)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);

    let one = store
        .flow_runs(OrgId(org_id), target_id, any_time(), Some("us-east"), 50)
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].region, "us-east");
}

// The panel shows the range the page is on, so a run outside it is not listed
// even though the row is still stored.
#[tokio::test]
#[ignore]
async fn a_run_outside_the_selected_range_is_not_listed() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );
    let mut old = failed_login(org_id, target_id);
    old.timestamp = Utc::now() - Duration::days(20);
    let recent = failed_login(org_id, target_id);
    sink.write_runs(&[old, recent]).await;

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    assert_eq!(
        store
            .flow_runs(OrgId(org_id), target_id, any_time(), None, 50)
            .await
            .unwrap()
            .len(),
        2,
        "both rows are stored"
    );

    let last_day = ClampedRange::unclamped(TimeRange {
        from: Utc::now() - Duration::days(1),
        to: Utc::now() + Duration::minutes(1),
    });
    let listed = store
        .flow_runs(OrgId(org_id), target_id, last_day, None, 50)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "only the run inside the range is listed");
}

// At the interval floor the newest page reaches back hours while the table
// holds weeks, so a failure older than that must still be listed.
#[tokio::test]
#[ignore]
async fn an_old_failure_is_listed_even_when_newer_runs_fill_the_page() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );

    let mut runs = Vec::new();
    let mut old = failed_login(org_id, target_id);
    old.timestamp = Utc::now() - Duration::days(6);
    runs.push(old);
    for i in 0..20 {
        let mut ok = failed_login(org_id, target_id);
        ok.timestamp = Utc::now() - Duration::minutes(i * 5);
        ok.status = CheckStatus::Up;
        ok.error = None;
        ok.evidence = None;
        ok.steps = vec![step("fill", StepOutcome::Passed, 12)];
        runs.push(ok);
    }
    sink.write_runs(&runs).await;

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    // A page smaller than the run of passes above it: newest-only would stop
    // long before reaching the failure.
    let listed = store
        .flow_runs(OrgId(org_id), target_id, any_time(), None, 5)
        .await
        .unwrap();

    assert!(
        listed.iter().any(|r| r.status == CheckStatus::Down),
        "the failure this monitor is kept for was not listed: {:?}",
        listed.iter().map(|r| r.status).collect::<Vec<_>>()
    );
    assert_eq!(listed[0].status, CheckStatus::Up, "still newest first");
    let times: Vec<_> = listed.iter().map(|r| r.timestamp).collect();
    let mut sorted = times.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(times, sorted, "the merged list stays chronological");
}

#[tokio::test]
#[ignore]
async fn a_step_never_reached_contributes_no_duration() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );
    sink.write_runs(&[failed_login(org_id, target_id)]).await;

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    let steps = store
        .flow_step_buckets(OrgId(org_id), target_id, any_time(), 60, None)
        .await
        .unwrap();

    let indexes: Vec<u16> = steps.iter().map(|s| s.step).collect();
    assert_eq!(
        indexes,
        vec![0, 1, 2, 3],
        "the skipped fifth step must not appear at all"
    );
    assert_eq!(steps[3].op, "assert_url");
    assert_eq!(
        steps[3].buckets[0].avg, None,
        "the step was reached but never passed, so it has no duration to report"
    );
    assert_eq!(steps[3].buckets[0].samples, 0);
    assert_eq!(steps[3].buckets[0].failed, 1);
    assert_eq!(
        steps[0].buckets[0].avg,
        Some(12),
        "a passing step still times"
    );
}

// A failed step spent its whole step timeout, so averaging it in would bury the
// runs around it — the reading it distorts is the one the chart exists for.
#[tokio::test]
#[ignore]
async fn a_failure_is_counted_but_kept_out_of_the_mean() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );

    // Three passes near 200ms and one 10s timeout, all inside one bucket.
    let mut runs: Vec<FlowRunRecord> = (0..3)
        .map(|i| {
            let mut run = failed_login(org_id, target_id);
            run.timestamp = Utc::now() - Duration::minutes(10 + i);
            run.status = CheckStatus::Up;
            run.error = None;
            run.evidence = None;
            run.steps = vec![step("assert_url", StepOutcome::Passed, 200)];
            run
        })
        .collect();
    let mut timed_out = failed_login(org_id, target_id);
    timed_out.timestamp = Utc::now() - Duration::minutes(5);
    timed_out.steps = vec![step("assert_url", StepOutcome::Failed, 10_000)];
    timed_out.evidence = None;
    runs.push(timed_out);
    sink.write_runs(&runs).await;

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    let steps = store
        .flow_step_buckets(OrgId(org_id), target_id, any_time(), 86_400, None)
        .await
        .unwrap();

    assert_eq!(steps.len(), 1);
    let bucket = &steps[0].buckets[0];
    assert_eq!(
        bucket.avg,
        Some(200),
        "the 10s timeout dragged the mean; averaging all four gives 2600"
    );
    assert_eq!(bucket.samples, 3);
    assert_eq!(bucket.failed, 1);
}

// A step slowing down is visible while the journey is still passing.
#[tokio::test]
#[ignore]
async fn a_step_getting_slower_reads_as_a_rising_series() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );

    let runs: Vec<FlowRunRecord> = (0..4)
        .map(|i| {
            let mut run = failed_login(org_id, target_id);
            run.timestamp = Utc::now() - Duration::minutes(30 * (4 - i));
            run.status = CheckStatus::Up;
            run.error = None;
            run.evidence = None;
            run.steps = vec![
                step("fill", StepOutcome::Passed, 10),
                step("assert_url", StepOutcome::Passed, 100 * (i as u32 + 1)),
            ];
            run
        })
        .collect();
    sink.write_runs(&runs).await;

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    let steps = store
        .flow_step_buckets(OrgId(org_id), target_id, any_time(), 60, None)
        .await
        .unwrap();

    let slow = steps.iter().find(|s| s.step == 1).expect("second step");
    let series: Vec<Option<u32>> = slow.buckets.iter().map(|b| b.avg).collect();
    assert_eq!(series, vec![Some(100), Some(200), Some(300), Some(400)]);
    let times: Vec<i64> = slow.buckets.iter().map(|b| b.t).collect();
    let mut sorted = times.clone();
    sorted.sort_unstable();
    assert_eq!(times, sorted, "buckets read oldest first, as a chart plots");

    let flat = steps.iter().find(|s| s.step == 0).expect("first step");
    assert!(
        flat.buckets.iter().all(|b| b.avg == Some(10)),
        "the step that did not change must not move"
    );
}

// Editing a flow renames a step in place. The series is labelled with what the
// step runs today, not what it used to be.
#[tokio::test]
#[ignore]
async fn a_renamed_step_is_labelled_with_its_current_op() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let sink = ClickhouseFlowRunSink::new(
        client.clone(),
        "eu-helsinki".into(),
        uptimepage::storage::OrgTtlDays::new(),
    );

    // Both ops land in one bucket, so only the newest run can settle the label.
    let runs: Vec<FlowRunRecord> = [("wait_for", 40), ("assert_text", 10)]
        .into_iter()
        .map(|(op, mins)| {
            let mut run = failed_login(org_id, target_id);
            run.timestamp = Utc::now() - Duration::minutes(mins);
            run.status = CheckStatus::Up;
            run.error = None;
            run.evidence = None;
            run.steps = vec![step(op, StepOutcome::Passed, 25)];
            run
        })
        .collect();
    sink.write_runs(&runs).await;

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    let steps = store
        .flow_step_buckets(OrgId(org_id), target_id, any_time(), 86_400, None)
        .await
        .unwrap();

    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].buckets.len(), 1, "both runs share one bucket");
    assert_eq!(steps[0].op, "assert_text", "labelled with the retired op");
}

#[tokio::test]
#[ignore]
async fn step_durations_scope_to_one_region() {
    let Some(client) = common::ch_client_from_env().await else {
        eprintln!("CLICKHOUSE_URL unset; skipping");
        return;
    };
    use uptimepage::domain::OrgId;
    use uptimepage::storage::traits::ResultsStore;

    let org_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let ttl = uptimepage::storage::OrgTtlDays::new();
    let eu = ClickhouseFlowRunSink::new(client.clone(), "eu-helsinki".into(), ttl.clone());
    let us = ClickhouseFlowRunSink::new(client.clone(), "us-east".into(), ttl);

    let mut slow = failed_login(org_id, target_id);
    slow.steps = vec![step("fill", StepOutcome::Passed, 900)];
    let mut fast = failed_login(org_id, target_id);
    fast.steps = vec![step("fill", StepOutcome::Passed, 10)];
    eu.write_runs(&[fast]).await;
    us.write_runs(&[slow]).await;

    let store = uptimepage::storage::ClickhouseResultsStore::from_client(client);
    let steps = store
        .flow_step_buckets(OrgId(org_id), target_id, any_time(), 60, Some("us-east"))
        .await
        .unwrap();

    assert_eq!(steps.len(), 1);
    assert_eq!(
        steps[0].buckets[0].avg,
        Some(900),
        "the other region leaked in"
    );
}
