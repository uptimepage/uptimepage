//! ClickHouse-backed test for the error-class sweep. Exercises the real
//! bind/deserialize path and the two-level aggregation that decides which
//! monitor owns a class; the folding and classification are unit-tested.
//!
//! Skipped by default: requires the dev compose stack.
//!
//!     docker compose -f compose.dev.yml up -d
//!     DATABASE_URL=postgres://monitor:monitor@127.0.0.1:5432/monitor \
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo test --test error_classes_test -- --ignored

mod common;

use chrono::Utc;
use uptimepage::domain::{CheckResult, CheckStatus};
use uptimepage::observability::error_classes;
use uptimepage::storage::{ClickhouseResultSink, OrgTtlDays, ResultSink};
use uuid::Uuid;

fn failure(target_id: Uuid, org_id: Uuid, error: &str) -> CheckResult {
    CheckResult {
        target_id,
        org_id,
        timestamp: Utc::now(),
        status: CheckStatus::Down,
        duration_ms: 10,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: None,
        response_size: None,
        diagnostic: None,
        error: Some(error.to_string()),
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via dev compose then `cargo test -- --ignored`"]
async fn collect_names_the_monitor_that_owns_an_error() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    // The sweep is fleet-wide, so tag the error text per run to keep leftover
    // rows and concurrent suites out of the assertions.
    let tag = Uuid::now_v7().simple().to_string();
    let stuck_error = format!("body {}", &tag[..8]);
    let spread_error = format!("no response {}", &tag[..8]);
    let org = Uuid::now_v7();
    let noisy = Uuid::now_v7();
    let quiet = Uuid::now_v7();

    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "eu-test".into(),
        "agent".into(),
        OrgTtlDays::new(),
    );

    let mut batch = Vec::new();
    for _ in 0..5 {
        batch.push(failure(noisy, org, &stuck_error));
    }
    batch.push(failure(quiet, org, &stuck_error));
    batch.push(failure(noisy, org, &spread_error));
    batch.push(failure(quiet, org, &spread_error));
    batch.push(CheckResult {
        error: None,
        status: CheckStatus::Up,
        ..failure(noisy, org, "unused")
    });
    sink.write_batch(&batch).await.expect("seed results");

    let rows = error_classes::collect(&ch, 3600).await.expect("collect");

    let stuck = rows
        .iter()
        .find(|r| r.error.as_deref() == Some(stuck_error.as_str()))
        .expect("stuck error present");
    assert_eq!(stuck.checks, 6, "every check for the error is counted");
    assert_eq!(
        stuck.top_monitor_checks, 5,
        "largest single monitor's count"
    );
    assert_eq!(
        stuck.top_monitor, noisy,
        "argMax picks the dominant monitor"
    );

    let spread = rows
        .iter()
        .find(|r| r.error.as_deref() == Some(spread_error.as_str()))
        .expect("spread error present");
    assert_eq!(spread.checks, 2);
    assert_eq!(
        spread.top_monitor_checks, 1,
        "an evenly shared error has no dominant monitor"
    );

    assert!(
        rows.iter().all(|r| r.error.as_deref() != Some("unused")),
        "results with no error must not appear"
    );
}

/// The sweep is the one read in the codebase that crosses tenants on purpose.
/// What makes that safe is that nothing org-scoped survives the aggregation:
/// two tenants hitting the same failure merge into one anonymous row.
#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL — run via dev compose then `cargo test -- --ignored`"]
async fn collect_merges_tenants_without_carrying_either_one() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    let tag = Uuid::now_v7().simple().to_string();
    let shared_error = format!("decode {}", &tag[..8]);
    let org_a = Uuid::now_v7();
    let org_b = Uuid::now_v7();
    let target_a = Uuid::now_v7();
    let target_b = Uuid::now_v7();

    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "eu-test".into(),
        "agent".into(),
        OrgTtlDays::new(),
    );
    let mut batch = vec![failure(target_a, org_a, &shared_error)];
    for _ in 0..3 {
        batch.push(failure(target_b, org_b, &shared_error));
    }
    sink.write_batch(&batch).await.expect("seed results");

    let rows = error_classes::collect(&ch, 3600).await.expect("collect");
    let matching: Vec<_> = rows
        .iter()
        .filter(|r| r.error.as_deref() == Some(shared_error.as_str()))
        .collect();

    assert_eq!(
        matching.len(),
        1,
        "one row per error, never one row per tenant"
    );
    assert_eq!(matching[0].checks, 4, "both tenants counted");
    assert_eq!(
        matching[0].top_monitor, target_b,
        "the busier tenant's monitor wins the argMax, and only as a log target"
    );
}

/// The read has no `org_id` filter, so it must stay a control-plane task. A
/// request handler reaching for it would serve one tenant another's failures.
#[test]
fn no_request_path_module_reaches_the_cross_tenant_sweep() {
    /// Returns the files scanned, so a renamed directory fails loudly instead
    /// of turning the guard into a no-op that passes on an empty walk.
    fn scan(dir: &std::path::Path, hits: &mut Vec<String>) -> usize {
        let mut scanned = 0;
        for entry in std::fs::read_dir(dir).expect("read source dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                scanned += scan(&path, hits);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                scanned += 1;
                if std::fs::read_to_string(&path)
                    .expect("read source file")
                    .contains("error_classes")
                {
                    hits.push(path.display().to_string());
                }
            }
        }
        scanned
    }

    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut hits = Vec::new();
    for area in [
        "web",
        "api",
        "mcp",
        "public_status",
        "marketing",
        "request",
        "targets",
        "channels",
        "templates",
    ] {
        let scanned = scan(&root.join(area), &mut hits);
        assert!(scanned > 0, "src/{area} scanned no files");
    }
    assert!(
        hits.is_empty(),
        "request-path modules must not query the cross-tenant sweep: {hits:?}"
    );
}
