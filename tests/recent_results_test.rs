//! ClickHouse integration tests for the multi-target raw results reads: the
//! batched read behind the incident writer (`target_id IN (...)`, the
//! per-`(target, region)` cap, region tagging, the `(org, target)` pair
//! filter), the monitors list's last check times, and each region's newest
//! check details.
//!
//! Skipped by default. Requires a ClickHouse the migrations have run against:
//!
//!     CLICKHOUSE_URL=http://127.0.0.1:8123 \
//!       cargo nextest run --test it --run-ignored only --no-capture -E 'test(/^recent_results_test::/)'

use crate::common;

use chrono::{Duration, Utc};
use std::sync::Arc;
use uptimepage::domain::{
    CheckDetails, CheckDiagnostic, CheckDiagnosticKind, CheckResult, CheckStatus,
    DiagnosticConfidence, DiagnosticEvidence, DiagnosticRemediation, DnsAnswer, DnsDetails,
    DnsRecordType, EdgeProvider, NetworkPath, OrgId,
};

use uptimepage::public_status::{InMemoryIncidentStore, IncidentWriter, IncidentWriterConfig};
use uptimepage::storage::{
    ClampedRange, ClickhouseResultSink, ClickhouseResultsStore, FailingServer, InMemoryTargetStore,
    OrgTtlDays, RecentOutcomes, RegionOutcome, ResultSink, ResultsStore, TimeRange,
};
use uuid::Uuid;

fn result(target: Uuid, org: Uuid, ts: chrono::DateTime<Utc>) -> CheckResult {
    CheckResult {
        target_id: target,
        org_id: org,
        timestamp: ts,
        status: CheckStatus::Up,
        duration_ms: 10,
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(200),
        response_size: None,
        network: None,
        diagnostic: None,
        error: None,
        details: None,
    }
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn batched_read_caps_per_target_region_and_filters_pairs() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };

    let sink_eu = ClickhouseResultSink::new(
        ch.clone(),
        "eu".into(),
        "agent-eu".into(),
        OrgTtlDays::new(),
    );
    let sink_us = ClickhouseResultSink::new(
        ch.clone(),
        "us".into(),
        "agent-us".into(),
        OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch);

    let org = Uuid::now_v7();
    let t1 = Uuid::now_v7();
    let t2 = Uuid::now_v7();
    let now = Utc::now();
    let ago = |s: i64| now - Duration::seconds(s);
    let diagnosed_at = ago(10);
    let mut diagnosed = result(t1, org, diagnosed_at);
    diagnosed.status = CheckStatus::Down;
    diagnosed.response_code = Some(403);
    diagnosed.error = Some("unexpected status 403".into());
    diagnosed.diagnostic = Some(CheckDiagnostic::access_interference(
        DiagnosticConfidence::High,
        Some(EdgeProvider::Akamai),
        vec![
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::BlockPage,
            DiagnosticEvidence::ReferenceId,
        ],
    ));

    let origin_at = ago(45);
    let mut origin_down = result(t1, org, origin_at);
    origin_down.status = CheckStatus::Down;
    origin_down.response_code = Some(530);
    origin_down.error = Some("unexpected status 530".into());
    origin_down.diagnostic = Some(CheckDiagnostic::origin_unreachable(
        vec![
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::ReferenceId,
            DiagnosticEvidence::OriginErrorCode,
        ],
        true,
    ));

    // t1: 5 eu results + 1 us result. t2: 2 eu results.
    sink_eu
        .write_batch(&[
            result(t1, org, ago(90)),
            result(t1, org, ago(60)),
            result(t1, org, ago(30)),
            diagnosed,
            origin_down,
            result(t2, org, ago(40)),
            result(t2, org, ago(20)),
        ])
        .await
        .expect("seed eu");
    sink_us
        .write_batch(&[result(t1, org, ago(15))])
        .await
        .expect("seed us");

    let since = ago(3600);

    // One query for both targets, generous cap → every seeded row, region-tagged.
    let all = store
        .recent_outcomes(&[(OrgId(org), t1), (OrgId(org), t2)], since, None, 100)
        .await
        .expect("batched read");
    let t1_rows: Vec<_> = all.iter().filter(|r| r.outcome.target_id == t1).collect();
    let t2_rows: Vec<_> = all.iter().filter(|r| r.outcome.target_id == t2).collect();
    assert_eq!(t1_rows.len(), 6, "t1: 5 eu + 1 us");
    assert_eq!(t2_rows.len(), 2, "t2: 2 eu");
    let stored_diagnostic = t1_rows
        .iter()
        .find(|r| r.outcome.timestamp.timestamp() == diagnosed_at.timestamp())
        .and_then(|r| r.outcome.diagnostic.as_ref())
        .expect("diagnosis round-trips through ClickHouse");
    assert_eq!(stored_diagnostic.provider, Some(EdgeProvider::Akamai));
    assert_eq!(
        stored_diagnostic.remediations,
        vec![
            DiagnosticRemediation::UseAuthenticatedHealthEndpoint,
            DiagnosticRemediation::AllowMonitorThroughEdgeRules,
        ]
    );
    let stored_origin = t1_rows
        .iter()
        .find(|r| r.outcome.timestamp.timestamp() == origin_at.timestamp())
        .and_then(|r| r.outcome.diagnostic.as_ref())
        .expect("origin diagnosis round-trips through ClickHouse");
    assert_eq!(stored_origin.kind, CheckDiagnosticKind::OriginTunnelDown);
    assert_eq!(stored_origin.provider, Some(EdgeProvider::Cloudflare));
    assert_eq!(
        stored_origin.evidence,
        vec![
            DiagnosticEvidence::EdgeServer,
            DiagnosticEvidence::ReferenceId,
            DiagnosticEvidence::OriginErrorCode,
        ]
    );
    assert_eq!(
        stored_origin.remediations,
        vec![
            DiagnosticRemediation::VerifyEdgeTunnel,
            DiagnosticRemediation::VerifyOriginReachable,
        ],
        "remediations must survive a round trip that stores only the kind"
    );
    assert!(
        t1_rows.iter().any(|r| r.region == "eu") && t1_rows.iter().any(|r| r.region == "us"),
        "t1 carries both region tags"
    );

    // per-target cap = 2 keeps the 2 newest per (target, region).
    let capped = store
        .recent_outcomes(&[(OrgId(org), t1)], since, None, 2)
        .await
        .expect("capped read");
    let mut eu_ts: Vec<i64> = capped
        .iter()
        .filter(|r| r.region == "eu" && r.outcome.target_id == t1)
        .map(|r| r.outcome.timestamp.timestamp())
        .collect();
    eu_ts.sort_unstable();
    assert_eq!(
        eu_ts,
        vec![ago(30).timestamp(), ago(10).timestamp()],
        "newest 2 eu rows for t1"
    );
    assert_eq!(
        capped
            .iter()
            .filter(|r| r.region == "us" && r.outcome.target_id == t1)
            .count(),
        1,
        "us has one row, under the cap"
    );

    // Wrong org for t1 → pair filter yields nothing.
    let foreign = store
        .recent_outcomes(&[(OrgId(Uuid::now_v7()), t1)], since, None, 100)
        .await
        .expect("foreign read");
    assert!(
        foreign.is_empty(),
        "mismatched (org, target) pair returns no rows"
    );

    // Empty input is a no-op, not an error.
    let empty = store
        .recent_outcomes(&[], since, None, 100)
        .await
        .expect("empty read");
    assert!(empty.is_empty());
}

/// A late result is new when it lands, whatever its own timestamp says, and
/// one that landed long ago is not, however recent its timestamp.
#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn a_read_of_recent_arrivals_goes_by_when_a_row_landed() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "eu".into(),
        "agent-eu".into(),
        OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch.clone());

    let org = Uuid::now_v7();
    let target = Uuid::now_v7();
    let now = Utc::now();
    let ago = |s: i64| now - Duration::seconds(s);
    sink.write_batch(&[result(target, org, ago(3000)), result(target, org, ago(10))])
        .await
        .expect("seed");
    ch.query(
        "INSERT INTO check_results \
         (org_id, target_id, region, timestamp, ingested_at, agent_id, status, duration_ms) \
         VALUES (?, ?, 'eu', fromUnixTimestamp(?), now() - INTERVAL 1 HOUR, 'agent-eu', 'down', 10)",
    )
    .bind(org)
    .bind(target)
    .bind(ago(5).timestamp())
    .execute()
    .await
    .expect("seed a row that landed an hour ago");

    let pairs = [(OrgId(org), target)];
    let stamps = |rows: &[RegionOutcome]| {
        let mut at: Vec<i64> = rows
            .iter()
            .map(|r| r.outcome.timestamp.timestamp())
            .collect();
        at.sort_unstable();
        at
    };
    let arrived = store
        .recent_outcomes(
            &pairs,
            ago(3600),
            Some(std::time::Duration::from_secs(600)),
            100,
        )
        .await
        .expect("arrivals read");
    assert_eq!(
        stamps(&arrived),
        vec![ago(3000).timestamp(), ago(10).timestamp()]
    );

    let newest_arrival = store
        .recent_outcomes(
            &pairs,
            ago(3600),
            Some(std::time::Duration::from_secs(600)),
            1,
        )
        .await
        .expect("capped arrivals read");
    assert_eq!(
        stamps(&newest_arrival),
        vec![ago(10).timestamp()],
        "the cap counts what arrived, not the row that landed before"
    );

    let whole = store
        .recent_outcomes(&pairs, ago(3600), None, 100)
        .await
        .expect("whole read");
    assert_eq!(
        stamps(&whole),
        vec![
            ago(3000).timestamp(),
            ago(10).timestamp(),
            ago(5).timestamp()
        ]
    );
    let arrival = |secs_ago: i64| {
        whole
            .iter()
            .find(|r| r.outcome.timestamp.timestamp() == ago(secs_ago).timestamp())
            .map(|r| r.arrival)
            .expect("row")
    };
    assert!(
        arrival(5) < arrival(10) && arrival(10) == arrival(3000),
        "rows are ordered by when they landed"
    );
}

/// The real store sends a row again on every read inside the overlap. One
/// failure read three times is still one failure.
#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn the_writer_counts_a_stored_result_once_however_often_it_reads_it() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "eu".into(),
        "agent-eu".into(),
        OrgTtlDays::new(),
    );
    let target = common::http_target("127.0.0.1:1".parse().unwrap(), "/", 30_000);
    let target_id = target.id;
    let incidents = Arc::new(InMemoryIncidentStore::new());
    let writer = IncidentWriter::new(
        Arc::new(InMemoryTargetStore::from_vec(vec![target])),
        Arc::new(ClickhouseResultsStore::from_client(ch)),
        incidents.clone(),
        IncidentWriterConfig::default(),
    );
    let now = Utc::now();
    let down = |secs_ago: i64| {
        let mut r = result(target_id, Uuid::nil(), now - Duration::seconds(secs_ago));
        r.status = CheckStatus::Down;
        r
    };

    sink.write_batch(&[down(60)]).await.expect("seed");
    for _ in 0..3 {
        writer.tick_once().await.expect("tick");
    }
    assert_eq!(incidents.insert_count(), 0, "one failure does not confirm");

    sink.write_batch(&[down(90)])
        .await
        .expect("seed the earlier");
    writer.tick_once().await.expect("tick");
    let all = incidents.all_for(target_id);
    assert_eq!(all.len(), 1, "the failure that landed late confirms it");
    assert_eq!(
        all[0].started_at.timestamp(),
        (now - Duration::seconds(90)).timestamp()
    );
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn list_failures_by_region_drops_up_and_scopes_region() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink_eu = ClickhouseResultSink::new(
        ch.clone(),
        "eu".into(),
        "agent-eu".into(),
        OrgTtlDays::new(),
    );
    let sink_us = ClickhouseResultSink::new(
        ch.clone(),
        "us".into(),
        "agent-us".into(),
        OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch);

    let org = Uuid::now_v7();
    let t = Uuid::now_v7();
    let now = Utc::now();
    let ago = |s: i64| now - Duration::seconds(s);
    let with = |ts, status| CheckResult {
        status,
        ..result(t, org, ts)
    };

    // eu: 2 up + 1 down + 1 error. us: 1 up + 1 degraded.
    sink_eu
        .write_batch(&[
            with(ago(90), CheckStatus::Up),
            with(ago(75), CheckStatus::Down),
            with(ago(60), CheckStatus::Up),
            with(ago(45), CheckStatus::Error),
        ])
        .await
        .expect("seed eu");
    sink_us
        .write_batch(&[
            with(ago(50), CheckStatus::Up),
            with(ago(20), CheckStatus::Degraded),
        ])
        .await
        .expect("seed us");

    let range = ClampedRange::unclamped(TimeRange {
        from: ago(3600),
        to: now + Duration::seconds(60),
    });

    // All regions: only the three non-Up rows, region-tagged, newest first.
    let all = store
        .list_failures_by_region(OrgId(org), t, range, 100, 0, None)
        .await
        .expect("all failures");
    assert_eq!(all.len(), 3, "2 eu failures + 1 us failure, no Up rows");
    assert!(all.iter().all(|(_, r)| r.status != CheckStatus::Up));
    assert_eq!(
        all.first().map(|(_, r)| r.status),
        Some(CheckStatus::Degraded),
        "newest failure first (us, 20s ago)"
    );
    assert!(
        all.iter().any(|(region, _)| region == "eu")
            && all.iter().any(|(region, _)| region == "us"),
        "failures carry their region tag"
    );

    // Region filter scopes to one region's failures.
    let eu = store
        .list_failures_by_region(OrgId(org), t, range, 100, 0, Some("eu"))
        .await
        .expect("eu failures");
    assert_eq!(eu.len(), 2, "only eu's down + error");
    assert!(
        eu.iter()
            .all(|(region, r)| region == "eu" && r.status != CheckStatus::Up)
    );

    // Pagination covers the set without duplicating across the boundary.
    let p1 = store
        .list_failures_by_region(OrgId(org), t, range, 2, 0, None)
        .await
        .expect("page 1");
    let p2 = store
        .list_failures_by_region(OrgId(org), t, range, 2, 2, None)
        .await
        .expect("page 2");
    assert_eq!(p1.len(), 2);
    assert_eq!(p2.len(), 1);
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn last_check_times_take_the_newest_result_across_regions() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink_eu = ClickhouseResultSink::new(
        ch.clone(),
        "eu".into(),
        "agent-eu".into(),
        OrgTtlDays::new(),
    );
    let sink_us = ClickhouseResultSink::new(
        ch.clone(),
        "us".into(),
        "agent-us".into(),
        OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch);

    let org = Uuid::now_v7();
    let other_org = Uuid::now_v7();
    let fresh = Uuid::now_v7();
    let ahead = Uuid::now_v7();
    let stale = Uuid::now_v7();
    let unlisted = Uuid::now_v7();
    let foreign = Uuid::now_v7();
    let now = Utc::now();
    let ago = |s: i64| now - Duration::seconds(s);

    sink_eu
        .write_batch(&[
            result(fresh, org, ago(400)),
            result(fresh, org, ago(95)),
            result(ahead, org, now + Duration::seconds(3)),
            result(stale, org, ago(7200)),
            result(unlisted, org, ago(5)),
            result(foreign, other_org, ago(5)),
        ])
        .await
        .expect("seed eu");
    sink_us
        .write_batch(&[result(fresh, org, ago(37))])
        .await
        .expect("seed us");

    let last = store
        .last_check_times(OrgId(org), &[fresh, ahead, stale, foreign], ago(3600))
        .await
        .expect("last check times");

    assert_eq!(
        last.get(&fresh).map(|t| t.timestamp()),
        Some(ago(37).timestamp()),
        "newest result wins across regions, at second precision"
    );
    assert_eq!(
        last.get(&ahead).map(|t| t.timestamp()),
        Some((now + Duration::seconds(3)).timestamp()),
        "a result from an agent clock running ahead still counts"
    );
    assert!(
        !last.contains_key(&stale),
        "a check before the window is absent"
    );
    assert!(
        !last.contains_key(&unlisted),
        "a monitor not asked for is absent"
    );
    assert!(
        !last.contains_key(&foreign),
        "another org's monitor is absent"
    );
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn network_path_round_trips_through_every_raw_read() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink = ClickhouseResultSink::new(
        ch.clone(),
        "eu".into(),
        "agent-eu".into(),
        OrgTtlDays::new(),
    );
    let store = ClickhouseResultsStore::from_client(ch);

    let org = Uuid::now_v7();
    let target = Uuid::now_v7();
    let now = Utc::now();
    let v4: std::net::IpAddr = "203.0.113.7".parse().unwrap();
    let v6: std::net::IpAddr = "2001:db8::53".parse().unwrap();
    let full = NetworkPath {
        remote_ip: Some(v4),
        tried_ips: vec![v6, v4],
        hop_url: Some("https://example.com/login?token=***".into()),
        dns: Some(DnsAnswer {
            resolver: "1.1.1.1".parse().unwrap(),
            addresses: vec![v6, v4],
            ttl_secs: 2340,
        }),
    };
    let only_ip = NetworkPath {
        remote_ip: Some(v6),
        ..NetworkPath::default()
    };
    let with = |secs_ago: i64, network: Option<NetworkPath>| {
        let mut r = result(target, org, now - Duration::seconds(secs_ago));
        r.network = network;
        r
    };
    sink.write_batch(&[
        with(30, Some(full.clone())),
        with(20, Some(only_ip.clone())),
        with(10, None),
    ])
    .await
    .expect("seed");

    let range = ClampedRange::unclamped(TimeRange {
        from: now - Duration::seconds(3600),
        to: now + Duration::seconds(60),
    });
    let expected = vec![None, Some(only_ip), Some(full)];

    let listed = store
        .list_results(OrgId(org), target, range, 10, 0, None)
        .await
        .expect("list_results");
    let network: Vec<_> = listed.iter().map(|r| r.network.clone()).collect();
    assert_eq!(
        network, expected,
        "IPv4 comes back as IPv4, not IPv4-mapped"
    );

    let by_region = store
        .list_results_by_region(OrgId(org), target, range, 10, 0)
        .await
        .expect("list_results_by_region");
    let network: Vec<_> = by_region.iter().map(|(_, r)| r.network.clone()).collect();
    assert_eq!(network, expected);
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn failing_servers_group_failures_by_address() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink = |region: &str| {
        ClickhouseResultSink::new(
            ch.clone(),
            region.into(),
            format!("agent-{region}"),
            OrgTtlDays::new(),
        )
    };
    let store = ClickhouseResultsStore::from_client(ch.clone());

    let org = Uuid::now_v7();
    let target = Uuid::now_v7();
    let now = Utc::now();
    let stale: std::net::IpAddr = "2001:db8::53".parse().unwrap();
    let origin: std::net::IpAddr = "203.0.113.7".parse().unwrap();
    let at = |secs_ago: i64, status: CheckStatus, ip: Option<std::net::IpAddr>| {
        let mut r = result(target, org, now - Duration::seconds(secs_ago));
        r.status = status;
        r.network = ip.map(|ip| NetworkPath {
            remote_ip: Some(ip),
            ..NetworkPath::default()
        });
        r
    };
    sink("eu")
        .write_batch(&[
            at(50, CheckStatus::Down, Some(stale)),
            at(40, CheckStatus::Down, Some(stale)),
            at(30, CheckStatus::Error, None),
            at(25, CheckStatus::Degraded, Some(origin)),
            at(20, CheckStatus::Up, Some(origin)),
        ])
        .await
        .expect("seed eu");
    sink("us")
        .write_batch(&[
            at(45, CheckStatus::Down, Some(stale)),
            at(35, CheckStatus::Down, Some(origin)),
        ])
        .await
        .expect("seed us");

    let range = ClampedRange::unclamped(TimeRange {
        from: now - Duration::seconds(3600),
        to: now + Duration::seconds(60),
    });
    let servers = store
        .failing_servers(OrgId(org), target, range, 5)
        .await
        .expect("failing_servers");

    assert_eq!(
        servers,
        vec![
            FailingServer {
                ip: stale,
                regions: vec!["eu".into(), "us".into()],
                checks: 3,
            },
            FailingServer {
                ip: origin,
                regions: vec!["us".into()],
                checks: 1,
            },
        ],
        "passing, degraded, and unconnected checks stay out"
    );
}

fn a_records(answers: &[&str]) -> CheckDetails {
    CheckDetails::Dns(DnsDetails {
        domain: "example.com".into(),
        record_type: DnsRecordType::A,
        missing: None,
        unexpected: None,
        answers: answers.iter().map(|a| a.to_string()).collect(),
        aliases: Vec::new(),
        expected_contains: None,
        matched: None,
    })
}

#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn latest_details_are_each_regions_newest_reading() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink = |region: &str| {
        ClickhouseResultSink::new(
            ch.clone(),
            region.into(),
            format!("agent-{region}"),
            OrgTtlDays::new(),
        )
    };
    let store = ClickhouseResultsStore::from_client(ch.clone());

    let org = Uuid::now_v7();
    let target = Uuid::now_v7();
    let now = Utc::now();
    let ago = |s: i64| now - Duration::seconds(s);
    let graded = |secs: i64, status: CheckStatus, answers: &[&str]| {
        CheckResult::graded(target, org, ago(secs), 5, status, a_records(answers))
    };

    sink("eu")
        .write_batch(&[
            graded(90, CheckStatus::Up, &["192.0.2.1"]),
            graded(60, CheckStatus::Down, &["192.0.2.9"]),
            // A run that read nothing leaves the last reading in place.
            CheckResult::error_with_elapsed(target, org, ago(30), 5, "dns: lookup timed out"),
        ])
        .await
        .expect("seed eu");
    sink("us")
        .write_batch(&[graded(45, CheckStatus::Up, &["198.51.100.7"])])
        .await
        .expect("seed us");

    let range = ClampedRange::unclamped(TimeRange {
        from: ago(3600),
        to: now + Duration::seconds(60),
    });
    let rows = store
        .latest_details_by_region(OrgId(org), target, range, None)
        .await
        .expect("latest details");
    let read: Vec<(&str, CheckStatus, Option<&CheckDetails>)> = rows
        .iter()
        .map(|(region, r)| (region.as_str(), r.status, r.details.as_ref()))
        .collect();
    assert_eq!(
        read,
        [
            ("eu", CheckStatus::Down, Some(&a_records(&["192.0.2.9"]))),
            ("us", CheckStatus::Up, Some(&a_records(&["198.51.100.7"]))),
        ]
    );
    assert_eq!(
        rows[0].1.error,
        Some(a_records(&["192.0.2.9"]).error_json())
    );

    let listed = store
        .list_results(OrgId(org), target, range, 10, 0, Some("eu"))
        .await
        .expect("list results");
    let listed: Vec<Option<&CheckDetails>> = listed.iter().map(|r| r.details.as_ref()).collect();
    assert_eq!(
        listed,
        [
            None,
            Some(&a_records(&["192.0.2.9"])),
            Some(&a_records(&["192.0.2.1"])),
        ],
        "the results API carries each run's details"
    );

    let us_only = store
        .latest_details_by_region(OrgId(org), target, range, Some("us"))
        .await
        .expect("region filter");
    assert_eq!(us_only.len(), 1);
    assert_eq!(us_only[0].0, "us");

    let other_org = store
        .latest_details_by_region(OrgId(Uuid::now_v7()), target, range, None)
        .await
        .expect("other org");
    assert!(other_org.is_empty(), "details stay inside their org");
}

/// A bare `argMax` over the nullable error column skips NULLs, so a region
/// whose newest check passed would report an older failure's error.
#[tokio::test]
#[ignore = "requires ClickHouse (CLICKHOUSE_URL)"]
async fn newest_by_region_reads_each_regions_state_and_whether_it_passed() {
    let Some(ch) = common::ch_client_from_env().await else {
        eprintln!("skipped: CLICKHOUSE_URL not set");
        return;
    };
    let sink = |region: &str| {
        ClickhouseResultSink::new(
            ch.clone(),
            region.into(),
            format!("agent-{region}"),
            OrgTtlDays::new(),
        )
    };
    let store = ClickhouseResultsStore::from_client(ch.clone());

    let org = Uuid::now_v7();
    let target = Uuid::now_v7();
    let now = Utc::now();
    let ago = |s: i64| now - Duration::seconds(s);
    let failed = |secs: i64, status: CheckStatus, error: &str| CheckResult {
        status,
        response_code: None,
        error: Some(error.into()),
        ..result(target, org, ago(secs))
    };

    sink("eu")
        .write_batch(&[
            failed(90, CheckStatus::Down, "connect timeout"),
            result(target, org, ago(60)),
        ])
        .await
        .expect("seed eu");
    sink("us")
        .write_batch(&[
            failed(80, CheckStatus::Down, "connection refused"),
            failed(20, CheckStatus::Down, "connect timeout"),
        ])
        .await
        .expect("seed us");
    sink("ap")
        .write_batch(&[
            failed(70, CheckStatus::Degraded, "rate-limited 429"),
            CheckResult {
                diagnostic: Some(CheckDiagnostic::access_interference(
                    DiagnosticConfidence::High,
                    Some(EdgeProvider::Cloudflare),
                    vec![DiagnosticEvidence::BlockPage],
                )),
                ..failed(10, CheckStatus::Down, "unexpected status 403")
            },
        ])
        .await
        .expect("seed ap");

    let window = |from_secs: i64| {
        ClampedRange::unclamped(TimeRange {
            from: ago(from_secs),
            to: now + Duration::seconds(60),
        })
    };
    let rows = store
        .newest_by_region(OrgId(org), target, window(3600))
        .await
        .expect("newest by region");
    let read: Vec<(&str, CheckStatus, Option<&str>, bool, bool)> = rows
        .iter()
        .map(|r| {
            (
                r.region.as_str(),
                r.status,
                r.error.as_deref(),
                r.diagnostic.is_some(),
                r.passed,
            )
        })
        .collect();
    assert_eq!(
        read,
        [
            (
                "ap",
                CheckStatus::Down,
                Some("unexpected status 403"),
                true,
                true
            ),
            ("eu", CheckStatus::Up, None, false, true),
            (
                "us",
                CheckStatus::Down,
                Some("connect timeout"),
                false,
                false
            ),
        ]
    );
    // Enough of the diagnostic to say what it says and advise what it advises.
    let diagnostic = rows[0].diagnostic.as_ref().expect("ap is diagnosed");
    assert_eq!(
        diagnostic.summary(),
        "access-policy block detected at the Cloudflare edge"
    );
    assert_eq!(diagnostic.kind, CheckDiagnosticKind::AccessInterference);
    let us = &rows[2];
    assert!(us.at > ago(25) && us.at <= now, "newest at {}", us.at);

    // Narrowed to after eu's pass and ap's degraded run: neither counts.
    let recent = store
        .newest_by_region(OrgId(org), target, window(30))
        .await
        .expect("recent window");
    let recent: Vec<(&str, bool)> = recent
        .iter()
        .map(|r| (r.region.as_str(), r.passed))
        .collect();
    assert_eq!(recent, [("ap", false), ("us", false)]);

    let other_org = store
        .newest_by_region(OrgId(Uuid::now_v7()), target, window(3600))
        .await
        .expect("other org");
    assert!(other_org.is_empty(), "results stay inside their org");
}
