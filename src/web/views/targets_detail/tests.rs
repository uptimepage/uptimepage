use super::charts::{KpiInputs, build_kpi_trend, status_segments};
use super::*;
use crate::domain::agent_wire::StepOutcome;
use crate::domain::metrics::AvailabilityBucket;
use crate::domain::{
    CheckDiagnostic, CheckResult, CheckStatus, DiagnosticConfidence, DiagnosticEvidence,
    EdgeProvider,
};
use crate::storage::{ClampedRange, UptimeStats};

#[test]
fn region_breakdown_greys_dead_region_keeps_live() {
    let rollup = |region: &str, last: &str| crate::domain::metrics::RegionRollup {
        region: region.into(),
        samples: 10,
        up: 10,
        p50_ms: 100,
        p95_ms: 200,
        p99_ms: 300,
        last_status: last.into(),
    };
    let live: std::collections::HashSet<String> = ["eu-west".to_string()].into_iter().collect();

    let row = |region: &str, live: Option<&std::collections::HashSet<String>>| {
        RegionBreakdownRow::from_rollup(
            rollup(region, "up"),
            None,
            &[],
            live,
            "/targets/x",
            "24h",
            0,
        )
    };

    assert_eq!(row("eu-west", Some(&live)).last_status, "up");
    assert_eq!(row("apac-sg", Some(&live)).last_status, "no_data");
    // Liveness unknown (query failed) leaves the status untouched.
    assert_eq!(row("apac-sg", None).last_status, "up");
}

#[test]
fn region_row_href_applies_the_filter_and_the_selected_row_clears_it() {
    let rollup = |region: &str| crate::domain::metrics::RegionRollup {
        region: region.into(),
        samples: 10,
        up: 10,
        p50_ms: 100,
        p95_ms: 200,
        p99_ms: 300,
        last_status: "up".into(),
    };
    let row = |region: &str, selected: Option<&str>| {
        RegionBreakdownRow::from_rollup(rollup(region), selected, &[], None, "/targets/x", "24h", 0)
    };

    assert_eq!(
        row("apac-sg", None).filter_href,
        "/targets/x?range=24h&region=apac-sg"
    );
    // Clicking the row already filtered to returns to all regions.
    assert_eq!(
        row("apac-sg", Some("apac-sg")).filter_href,
        "/targets/x?range=24h"
    );
}

#[test]
fn status_segment_labels_carry_date_only_on_multi_day_ranges() {
    use crate::storage::TimeRange;
    use chrono::TimeZone;
    let day = |d: u32| chrono::Utc.with_ymd_and_hms(2026, 5, d, 12, 0, 0).unwrap();
    let counts = [(10u64, 10u64)];

    let week = ClampedRange::unclamped(TimeRange {
        from: day(6),
        to: day(13),
    });
    let seg = &status_segments(&counts, week, 6 * 3600)[0];
    assert!(seg.time.contains("May"), "multi-day label: {}", seg.time);

    let daylong = ClampedRange::unclamped(TimeRange {
        from: day(12),
        to: day(13),
    });
    let seg = &status_segments(&counts, daylong, 1800)[0];
    assert!(!seg.time.contains("May"), "24h label: {}", seg.time);
}

fn sample_page() -> DetailPage {
    DetailPage {
        active_tab: "targets",
        subtab: SUBTAB_MONITOR,
        ongoing_count: 0,
        alerts_nobody: false,
        id: "00000000-0000-0000-0000-000000000001".into(),
        name: "api".into(),
        kind: "HTTP",
        address: "https://example.com".into(),
        registered_domain: None,
        coverage: None,
        interval_s: 60,
        enabled: true,
        tags: vec!["prod".into()],
        managed_by: None,
        share_count: 0,
        flapping_opens: None,
        flap_hold_minutes: 10,
        last_status: "up",
        last_at_iso: Arc::from("2026-05-13T12:00:00Z"),
        uptime: Arc::new(UptimeStatsView {
            total: 100,
            up: 99,
            down: 1,
            degraded: 0,
            error: 0,
            uptime_pct: Some("99.00".into()),
        }),
        kpi: Arc::new(KpiTrend::default()),
        pings: None,
        segments: Arc::from(vec![
            StatusSeg {
                class: "op",
                time: "12:00".into(),
                stat: "100.0%".into(),
                from_iso: "2026-05-13T12:00:00Z".into(),
                to_iso: "2026-05-13T12:30:00Z".into(),
                total: 60,
                bad: 0,
            },
            StatusSeg {
                class: "maj",
                time: "12:30".into(),
                stat: "0.0%".into(),
                from_iso: "2026-05-13T12:30:00Z".into(),
                to_iso: "2026-05-13T13:00:00Z".into(),
                total: 60,
                bad: 60,
            },
        ]),
        ribbon_oob: false,
        liveness: None,
        liveness_oob: false,
        flow_runs: Vec::new(),
        config_json: r#"{"type":"http"}"#.into(),
        range: "24h",
        range_options: build_range_options("24h", &RANGE_KEYS),
        range_base_path: "/targets/00000000-0000-0000-0000-000000000001".into(),
        from_iso: "2026-05-12T12:00:00Z".into(),
        to_iso: "2026-05-13T12:00:00Z".into(),
        from_human: "2026-05-12 12:00 UTC".into(),
        to_human: "2026-05-13 12:00 UTC".into(),
        regions: Vec::new(),
        selected_region: None,
        region_breakdown: Vec::new(),
        heartbeat: None,
    }
}

#[test]
fn a_never_pinged_heartbeat_reads_waiting_not_down() {
    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    p.last_status = super::WAITING_FOR_PING;
    let html = p.render().unwrap();
    assert!(html.contains("waiting for first ping"));
    assert!(html.contains("status-badge--pending"), "grey, not red");
    assert!(
        !html.contains(">down<") && !html.contains("checking\u{2026}"),
        "an unwired job is neither down nor being checked"
    );
}

#[test]
fn a_pending_heartbeat_card_explains_the_wait() {
    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    p.last_status = super::WAITING_FOR_PING;
    p.heartbeat = Some(crate::targets::HeartbeatInfo {
        ping_url: Some("https://app.example.com/ping/tok123".into()),
        first_ping_at: None,
        pending: true,
        created_at: Some(chrono::Utc::now() - chrono::Duration::days(2)),
        last_ping_at: None,
        last_start_at: None,
        last_fail_at: None,
        last_exit_code: None,
        last_failure_output: None,
        due_at: None,
        down_at: None,
        declared_period_secs: 600,
        observed_period_secs: None,
        cadence_advice: None,
        rotated_at: None,
        previous_url_expires_at: None,
        previous_url_last_used_at: None,
    });
    p.liveness = Some(super::HeartbeatLiveness {
        pending: true,
        since: Some(chrono::Utc::now() - chrono::Duration::days(2)),
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    });
    let html = p.render().unwrap();
    assert!(html.contains("waiting for first ping"));
    assert!(html.contains("nobody alerted"));
    assert!(html.contains("the schedule starts at the first ping"));

    let mut wired = sample_page();
    wired.kind = "HEARTBEAT";
    let mut hb = p.heartbeat.take().unwrap();
    hb.pending = false;
    hb.first_ping_at = Some(chrono::Utc::now());
    wired.heartbeat = Some(hb);
    wired.liveness = Some(super::HeartbeatLiveness {
        pending: false,
        since: None,
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    });
    let html = wired.render().unwrap();
    assert!(!html.contains("waiting for first ping"));
}

fn heartbeat_info(pending: bool) -> crate::targets::HeartbeatInfo {
    crate::targets::HeartbeatInfo {
        ping_url: Some("https://app.example.com/ping/tok123".into()),
        first_ping_at: (!pending).then(chrono::Utc::now),
        pending,
        created_at: Some(chrono::Utc::now() - chrono::Duration::days(2)),
        last_ping_at: None,
        last_start_at: None,
        last_fail_at: None,
        last_exit_code: None,
        last_failure_output: None,
        due_at: None,
        down_at: None,
        declared_period_secs: 600,
        observed_period_secs: None,
        cadence_advice: None,
        rotated_at: None,
        previous_url_expires_at: None,
        previous_url_last_used_at: None,
    }
}

/// The projection decides `due_at`/`down_at`; this is what the page makes of them.
#[test]
fn liveness_reads_the_window_the_projection_handed_it() {
    let now = chrono::Utc::now();
    let due = now - chrono::Duration::minutes(3);
    let windowed = |due_at, down_at| crate::targets::HeartbeatInfo {
        pending: false,
        due_at,
        down_at,
        ..heartbeat_info(false)
    };

    let inside = super::HeartbeatLiveness::derive(
        &windowed(Some(due), Some(now + chrono::Duration::minutes(7))),
        now,
        true,
    );
    assert!(inside.late && !inside.overdue);

    let past = super::HeartbeatLiveness::derive(
        &windowed(Some(due), Some(now - chrono::Duration::minutes(1))),
        now,
        true,
    );
    assert!(past.overdue && !past.late, "past the window is not late");

    let early = super::HeartbeatLiveness::derive(
        &windowed(
            Some(now + chrono::Duration::minutes(2)),
            Some(now + chrono::Duration::minutes(12)),
        ),
        now,
        true,
    );
    assert!(!early.late && !early.overdue);

    let no_grace = super::HeartbeatLiveness::derive(&windowed(Some(due), Some(due)), now, true);
    assert_eq!(no_grace.down_at, None);
    assert!(!no_grace.late, "there is no window to be inside");
    assert!(no_grace.overdue);

    // Pending, paused or failing: the projection withheld the window.
    let silent = super::HeartbeatLiveness::derive(&windowed(None, None), now, true);
    assert!(!silent.late && !silent.overdue);
    assert!(!super::HeartbeatLiveness::derive(&heartbeat_info(true), now, false).pending);
}

/// A heartbeat's rows count our evaluations, so "59 checks" read as 59 job runs
/// on a monitor its owner had told us to expect once a day.
#[test]
fn only_a_kind_that_receives_pings_counts_them() {
    let counted = |n| super::DetailPage {
        pings: Some(super::PingTally::Counted(n)),
        ..sample_page()
    };
    assert!(counted(3).render().unwrap().contains("3 pings received"));
    assert!(counted(1).render().unwrap().contains("1 ping received"));

    // A failed read must not fall back to the count this card exists to drop.
    let blind = super::DetailPage {
        pings: Some(super::PingTally::Unavailable),
        ..sample_page()
    };
    let html = blind.render().unwrap();
    assert!(html.contains("ping count unavailable"));
    assert!(
        !html.contains("100 checks"),
        "our evaluations are not its runs"
    );

    // Probed kinds keep the count they always had.
    let probed = sample_page().render().unwrap();
    assert!(probed.contains("100 checks"));
    assert!(!probed.contains("ping"));
}

/// Grace is the lateness that does not alert, so the evaluator keeps calling
/// it up.
#[test]
fn a_heartbeat_inside_its_grace_reads_late_not_up() {
    let now = chrono::Utc::now();
    let late = super::HeartbeatLiveness {
        pending: false,
        since: None,
        due_at: Some(now - chrono::Duration::minutes(3)),
        down_at: Some(now + chrono::Duration::minutes(7)),
        late: true,
        overdue: false,
    };
    assert_eq!(super::badge_status("up", Some(&late)), "late");
    assert_eq!(super::badge_status("", Some(&late)), "late");
    // A monitor already failing has a real answer; late would soften it.
    for worse in ["down", "error", "degraded"] {
        assert_eq!(super::badge_status(worse, Some(&late)), worse);
    }

    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    p.last_status = "late";
    p.heartbeat = Some(heartbeat_info(false));
    p.liveness = Some(late);
    let html = p.render().unwrap();
    assert!(html.contains("status-badge--late"));
    assert!(html.contains("this ping is late"));
    assert!(html.contains("counts as down"));
}

/// On time, the same panel says when the next ping is due and when silence
/// would become an incident: the grace window, stated rather than implied.
#[test]
fn a_heartbeat_on_time_still_shows_the_window_it_is_inside() {
    let now = chrono::Utc::now();
    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    p.heartbeat = Some(heartbeat_info(false));
    p.liveness = Some(super::HeartbeatLiveness {
        pending: false,
        since: None,
        due_at: Some(now + chrono::Duration::minutes(2)),
        down_at: Some(now + chrono::Duration::minutes(12)),
        late: false,
        overdue: false,
    });
    let html = p.render().unwrap();
    assert!(html.contains("next ping due"));
    assert!(html.contains("counts as down"));
    assert!(!html.contains("status-badge--late"));
    // The clock crosses the due time between polls, so the note ships hidden
    // beside the instants that decide when to reveal it.
    assert!(html.contains("data-hb-due="));
    assert!(html.contains("data-hb-down="));
    assert!(html.contains(r#"hidden data-hb-late-note"#));
}

/// The override belongs to one kind. Every other monitor gets its stored
/// status back, which is what stops a heartbeat rule leaking across the eight.
#[test]
fn no_other_kind_can_be_told_it_is_late() {
    for status in ["up", "down", "degraded", "error", ""] {
        assert_eq!(super::badge_status(status, None), status);
    }
}

/// The notice lives in the ping card, which the live poll does not own, so the
/// poll clears it out of band. Without that the card still claims nothing has
/// arrived while the KPIs beside it count the ping.
#[test]
fn the_live_poll_clears_the_waiting_notice_out_of_band() {
    let mut live = sample_live();
    live.liveness = Some(super::HeartbeatLiveness {
        pending: true,
        since: Some(chrono::Utc::now()),
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    });
    let html = live.render().unwrap();
    assert!(html.contains(r#"id="hb-liveness" hx-swap-oob="true""#));
    assert!(html.contains("waiting for first ping"));

    live.liveness = Some(super::HeartbeatLiveness {
        pending: false,
        since: None,
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    });
    let html = live.render().unwrap();
    assert!(html.contains(r#"id="hb-liveness" hx-swap-oob="true""#));
    assert!(!html.contains("waiting for first ping"));

    // Nothing to clear on a probed monitor, so nothing is swapped at it.
    live.liveness = None;
    assert!(!live.render().unwrap().contains("hb-liveness"));
}

#[test]
fn heartbeat_detail_renders_ping_card_without_probe_surfaces() {
    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    p.heartbeat = Some(crate::targets::HeartbeatInfo {
        ping_url: Some("https://app.example.com/ping/tok123".into()),
        first_ping_at: Some(chrono::Utc::now()),
        pending: false,
        created_at: Some(chrono::Utc::now()),
        last_ping_at: None,
        last_start_at: None,
        last_fail_at: Some(chrono::Utc::now()),
        last_exit_code: Some(137),
        last_failure_output: Some("rsync: connection unexpectedly closed".into()),
        due_at: None,
        down_at: None,
        declared_period_secs: 600,
        observed_period_secs: Some(4980),
        cadence_advice: Some(crate::targets::CadenceAdviceView {
            kind: "too_tight".into(),
            suggested_period_secs: 5400,
        }),
        rotated_at: None,
        previous_url_expires_at: None,
        previous_url_last_used_at: None,
    });
    let html = p.render().unwrap();
    assert!(html.contains("https://app.example.com/ping/tok123"));
    assert!(html.contains(r##"data-copy="#hb-ping-url""##));
    assert!(html.contains("last success"));
    assert!(html.contains("never"));
    assert!(html.contains("https://app.example.com/ping/tok123/start"));
    assert!(html.contains("exit 137"), "a failure names its exit status");
    assert!(
        html.contains("rsync: connection unexpectedly closed"),
        "the job's own account of the failure"
    );
    assert!(!html.contains("last start"), "no start recorded, no row");
    assert!(html.contains("runs less often than you told us"));
    assert!(html.contains("83m"), "the observed cadence");
    assert!(
        html.contains("10m"),
        "the declared cadence it disagrees with"
    );
    assert!(html.contains("90m"), "the period to set instead");
    // No probe surfaces for a passive kind.
    assert!(!html.contains("data-detail-test-now"));
    assert!(!html.contains("latency (p50/p95/p99)"));
    assert!(
        html.contains("data-hb-rotate"),
        "the rotate control is offered"
    );
    assert!(
        !html.contains("data-hb-revoke-prev"),
        "no overlap open, nothing to end early"
    );
}

#[test]
fn an_open_rotation_overlap_shows_its_window_and_last_use() {
    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    let mut hb = heartbeat_info(false);
    hb.rotated_at = Some(chrono::Utc::now());
    hb.previous_url_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(23));
    p.heartbeat = Some(hb);
    let html = p.render().unwrap();
    assert!(html.contains("pre-rotation URL still works until"));
    assert!(
        html.contains("has not been called since the rotation"),
        "silence is reported without being read as a verdict"
    );
    assert!(html.contains("Confirm every job uses the new"));
    assert!(html.contains("data-hb-revoke-prev"));

    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    let mut hb = heartbeat_info(false);
    hb.previous_url_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(23));
    hb.previous_url_last_used_at = Some(chrono::Utc::now() - chrono::Duration::minutes(4));
    p.heartbeat = Some(hb);
    let html = p.render().unwrap();
    assert!(
        html.contains("something still carries it"),
        "a live old URL warns against ending the overlap"
    );
}

/// An unreadable current URL is the case that most needs the overlap ended,
/// so the notice cannot live inside the branch that has a URL to show.
#[test]
fn an_unreadable_url_still_offers_to_end_the_overlap() {
    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    let mut hb = heartbeat_info(false);
    hb.ping_url = None;
    hb.previous_url_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(23));
    p.heartbeat = Some(hb);
    let html = p.render().unwrap();
    assert!(
        html.contains("Ping URL not available yet"),
        "the KEK branch"
    );
    assert!(html.contains("pre-rotation URL still works until"));
    assert!(html.contains("data-hb-revoke-prev"));
}

#[test]
fn status_segments_map_availability_to_ribbon_classes() {
    let base = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let range = ClampedRange::unclamped(TimeRange {
        from: base,
        to: base + Duration::hours(1),
    });
    // 900s buckets → four cells; classes follow the dashboard thresholds.
    let counts = [(100, 100), (100, 97), (100, 80), (0, 0)];
    let segs = status_segments(&counts, range, 900);
    let classes: Vec<&str> = segs.iter().map(|s| s.class).collect();
    assert_eq!(classes, ["op", "deg", "maj", "none"]);
    assert_eq!(segs[0].stat, "100.0%");
    assert_eq!(segs[3].stat, "no data");
    // Counts carry through for the drawer's scale line.
    assert_eq!((segs[2].total, segs[2].bad), (100, 20));
    assert_eq!((segs[3].total, segs[3].bad), (0, 0));
    // Each cell carries a non-empty, contiguous window for the drill drawer.
    assert!(!segs[0].from_iso.is_empty() && !segs[0].to_iso.is_empty());
    assert!(segs[0].from_iso < segs[0].to_iso);
    assert_eq!(segs[0].to_iso, segs[1].from_iso, "buckets are contiguous");
}

#[test]
fn drawer_check_rows_render_region_column_and_expand() {
    let mut r = CheckResult {
        target_id: Uuid::nil(),
        org_id: Uuid::nil(),
        timestamp: "2026-05-13T12:00:00Z".parse().unwrap(),
        status: crate::domain::CheckStatus::Error,
        duration_ms: 1204,
        dns_ms: Some(12),
        connect_ms: Some(40),
        tls_ms: None,
        ttfb_ms: None,
        response_code: Some(503),
        response_size: None,
        diagnostic: Some(CheckDiagnostic::access_interference(
            DiagnosticConfidence::High,
            Some(EdgeProvider::Akamai),
            vec![DiagnosticEvidence::BlockPage],
        )),
        error: Some("connection refused".into()),
    };
    let rows: Arc<[ResultRow]> =
        Arc::from(vec![ResultRow::with_region("apac-sg".into(), r.clone())]);
    let html = DetailCheckRows {
        results: Arc::clone(&rows),
        show_region: true,
        show_guidance: true,
    }
    .render()
    .unwrap();
    // Region cell present; failing row is expandable with a timing detail row.
    assert!(html.contains("apac-sg"));
    assert!(html.contains("data-result-row"));
    assert!(html.contains("data-result-detail"));
    assert!(html.contains("access-policy block detected at the Akamai edge"));
    assert!(html.contains("recommended:"));
    assert!(html.contains("use an authenticated health endpoint"));

    // The anonymous share surface names the cause but withholds advice aimed
    // at whoever administers the blocked origin.
    let shared = DetailCheckRows {
        results: rows,
        show_region: false,
        show_guidance: false,
    }
    .render()
    .unwrap();
    assert!(shared.contains("access-policy block detected at the Akamai edge"));
    assert!(!shared.contains("recommended:"));
    assert!(!shared.contains("use an authenticated health endpoint"));

    // Region-agnostic table (recent results) hides the column.
    r.error = None;
    let plain: Arc<[ResultRow]> = Arc::from(vec![ResultRow::from(r)]);
    let plain_html = DetailCheckRows {
        results: plain,
        show_region: false,
        show_guidance: true,
    }
    .render()
    .unwrap();
    assert!(!plain_html.contains("apac-sg"));
}

#[test]
fn empty_window_renders_no_data_not_a_zero_rate() {
    let mut p = sample_page();
    p.uptime = Arc::new(UptimeStatsView {
        total: 0,
        up: 0,
        down: 0,
        degraded: 0,
        error: 0,
        uptime_pct: None,
    });
    let html = p.render().unwrap();
    assert!(
        html.contains("no data"),
        "uptime card and ribbon read no data"
    );
    assert!(
        !html.contains(r#"<span class="dashboard-kpi-card__unit">%</span>"#),
        "no percent unit without a rate to qualify"
    );
}

fn trace(op: &str, outcome: StepOutcome, ms: u32) -> crate::domain::agent_wire::StepTrace {
    crate::domain::agent_wire::StepTrace {
        op: op.into(),
        outcome,
        duration_ms: ms,
    }
}

fn failed_run() -> crate::storage::traits::FlowRunView {
    crate::storage::traits::FlowRunView {
        timestamp: chrono::Utc::now(),
        region: "eu-helsinki".into(),
        status: CheckStatus::Down,
        duration_ms: 2570,
        stopped_step: Some(1),
        error: Some("step 2/3 assert_url: url does not contain \"/secure\"".into()),
        steps: vec![
            trace("fill", StepOutcome::Passed, 12),
            trace("assert_url", StepOutcome::Failed, 2000),
            trace("assert_text", StepOutcome::Skipped, 0),
        ],
        evidence: Some(crate::domain::agent_wire::FlowEvidence {
            final_url: Some("https://app.example.com/login".into()),
            title: Some("Sign in".into()),
            text_snippet: Some("Your password is invalid!".into()),
            console: vec![crate::domain::agent_wire::ConsoleLine {
                level: "error".into(),
                text: "token expired".into(),
            }],
        }),
        evidence_expired: false,
    }
}

fn flow_page(runs: Vec<crate::storage::traits::FlowRunView>) -> DetailPage {
    let mut p = sample_page();
    p.kind = "FLOW";
    p.flow_runs = runs.into_iter().map(FlowRunRow::from_view).collect();
    p
}

#[test]
fn flow_run_panel_shows_the_trace_and_the_page_behind_a_failure() {
    let html = flow_page(vec![failed_run()]).render().unwrap();
    assert!(html.contains("flow runs"));
    assert!(html.contains("stopped at step 2/3 assert_url"));
    assert!(html.contains("flow-step--pass"));
    assert!(html.contains("flow-step--fail"));
    assert!(html.contains("flow-step--skip"));
    assert!(html.contains("2000 ms"));
    assert!(html.contains("url does not contain"));
    assert!(!html.contains("step 2/3 assert_url: url"));
    assert!(html.contains("Your password is invalid!"));
    assert!(html.contains("token expired"));
}

// Page text is whatever the customer's site rendered, so it reaches the
// template as content and never as markup.
#[test]
fn captured_page_text_cannot_inject_markup() {
    let run = crate::storage::traits::FlowRunView {
        evidence: Some(crate::domain::agent_wire::FlowEvidence {
            final_url: Some("https://app.example.com/?q=<img src=x onerror=alert(1)>".into()),
            title: Some("<script>alert('title')</script>".into()),
            text_snippet: Some("<script>alert('body')</script>".into()),
            console: vec![crate::domain::agent_wire::ConsoleLine {
                level: "error".into(),
                text: "<iframe src=javascript:alert(1)>".into(),
            }],
        }),
        ..failed_run()
    };
    let html = flow_page(vec![run]).render().unwrap();
    assert!(
        !html.contains("<script>alert"),
        "page text reached the DOM as markup"
    );
    assert!(
        !html.contains("<iframe"),
        "console text reached the DOM as markup"
    );
    assert!(
        !html.contains("<img src=x"),
        "url reached the DOM as markup"
    );
    assert!(html.contains("alert("), "the text itself is still shown");
}

#[test]
fn a_passing_run_lists_its_steps_and_no_page() {
    let run = crate::storage::traits::FlowRunView {
        status: CheckStatus::Up,
        stopped_step: None,
        error: None,
        evidence: None,
        steps: vec![trace("fill", StepOutcome::Passed, 12)],
        ..failed_run()
    };
    let html = flow_page(vec![run]).render().unwrap();
    assert!(html.contains("all 1 steps passed"));
    assert!(!html.contains("flow-evidence"), "a pass captured no page");
    assert!(
        !html.contains("retention window"),
        "a pass never had a page to expire"
    );
}

// Trace kept, page gone: an ordinary older run, not an error.
#[test]
fn a_failure_past_its_evidence_window_says_so() {
    let run = crate::storage::traits::FlowRunView {
        evidence: None,
        evidence_expired: true,
        ..failed_run()
    };
    assert!(run.stopped_step.is_some(), "it stopped at a step");
    let html = flow_page(vec![run]).render().unwrap();
    assert!(html.contains("retention window"));
    assert!(html.contains("flow-step--fail"), "the trace still renders");
}

// The browser never started, so there is no step list and no page. It must
// not read as a clean run, and its reason has nowhere else to go.
#[test]
fn a_run_that_never_reached_its_steps_explains_itself() {
    let run = crate::storage::traits::FlowRunView {
        status: CheckStatus::Error,
        stopped_step: None,
        steps: Vec::new(),
        evidence: None,
        error: Some("engine did not start after retries: exit status 1".into()),
        ..failed_run()
    };
    let html = flow_page(vec![run]).render().unwrap();
    assert!(
        !html.contains("all 0 steps passed"),
        "an error is not a pass"
    );
    assert!(html.contains("never reached its steps"));
    assert!(html.contains("engine did not start after retries"));
    assert!(
        !html.contains("retention window"),
        "it never captured a page, so nothing of it expired"
    );
}

// Evidence capture is best-effort, so a page that died can leave a
// seconds-old failure with nothing captured. That is not an expiry.
#[test]
fn a_failure_that_captured_nothing_is_not_called_expired() {
    let run = crate::storage::traits::FlowRunView {
        evidence: None,
        evidence_expired: false,
        ..failed_run()
    };
    let html = flow_page(vec![run]).render().unwrap();
    assert!(
        !html.contains("retention window"),
        "nothing expired — the run never captured a page"
    );
    assert!(html.contains("flow-step--fail"), "the trace still renders");
}

#[test]
fn a_flow_with_no_runs_yet_renders_an_empty_state() {
    let html = flow_page(Vec::new()).render().unwrap();
    assert!(html.contains("No runs recorded yet"));
}

#[test]
fn the_run_panel_is_flow_only() {
    let html = sample_page().render().unwrap();
    assert!(
        !html.contains("flow runs"),
        "an HTTP monitor must not render the flow panel"
    );
}

#[test]
fn a_flow_charts_its_steps_where_other_kinds_chart_their_phases() {
    let flow = flow_page(Vec::new()).render().unwrap();
    assert!(flow.contains(r#"id="flow-steps""#));
    assert!(
        !flow.contains("breakdown-chart"),
        "a journey has no network phases to break down"
    );

    let http = sample_page().render().unwrap();
    assert!(http.contains("breakdown-chart"));
    assert!(!http.contains(r#"id="flow-steps""#));
}

#[test]
fn the_step_trend_carries_the_pages_range_and_region() {
    let mut p = flow_page(Vec::new());
    p.selected_region = Some("eu-helsinki".into());
    let html = p.render().unwrap();
    let at = html.find(r#"id="flow-steps""#).expect("panel rendered");
    let panel = &html[at..at + 400];
    assert!(panel.contains("/flow-steps?from="));
    assert!(panel.contains("&region=eu-helsinki"));
}

// A monitor that fails once and recovers opens no incident, so the banner
// must not send the reader to an empty tab.
#[test]
fn a_failing_check_with_no_open_incident_does_not_point_at_the_tab() {
    let mut p = sample_page();
    p.last_status = "error";
    p.ongoing_count = 0;
    let html = p.render().unwrap();
    assert!(html.contains("Monitor check failed"));
    assert!(
        !html.contains("Incidents tab"),
        "there is no incident to go and read"
    );

    let mut p = sample_page();
    p.last_status = "down";
    p.ongoing_count = 0;
    let html = p.render().unwrap();
    // A count of zero is also what a failed count reads as, so the banner
    // says how incidents come about rather than that none exists.
    assert!(html.contains("An incident opens once the failures persist"));
    assert!(!html.contains("Incidents tab"));
}

#[test]
fn an_open_incident_is_what_points_at_the_tab() {
    let mut p = sample_page();
    p.last_status = "down";
    p.ongoing_count = 1;
    let html = p.render().unwrap();
    assert!(html.contains("An incident is open"));
    assert!(html.contains("Incidents tab"));
}

#[test]
fn detail_renders_status_ribbon() {
    let html = sample_page().render().unwrap();
    assert!(html.contains("dashboard-ribbon"));
    assert!(html.contains("dashboard-ribbon__seg--op"));
    assert!(html.contains("dashboard-ribbon__seg--maj"));
    assert!(html.contains(r#"data-tip-stat="100.0%""#));
    // Failing cell is a drill button carrying its window; healthy cell is not.
    assert!(html.contains("data-ribbon-drill"));
    assert!(html.contains(r#"data-from="2026-05-13T12:30:00Z""#));
}

#[test]
fn detail_renders_header_and_widgets() {
    let html = sample_page().render().unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("api"));
    assert!(html.contains("Uptime"));
    assert!(html.contains("99.00"));
    assert!(html.contains("data-endpoint"));
    // Charts read the server-bucketed latency endpoint; recent results are
    // a server-rendered table, not an API fetch.
    assert!(html.contains("/api/v1/targets/00000000-0000-0000-0000-000000000001/latency"));
}

#[test]
fn coverage_panel_renders_a_card_per_suggestion() {
    let check: crate::domain::CheckSpec = serde_json::from_str(
        r#"{"type":"tls_cert","host":"app.acme.com","port":443,
                "warn_days":14,"critical_days":3,"timeout":5000}"#,
    )
    .unwrap();
    let mut page = sample_page();
    page.coverage = coverage::panel(&check, &[]);
    let html = page.render().unwrap();
    assert!(html.contains(r#"data-coverage="app.acme.com""#));
    assert!(html.contains("also worth watching"));
    assert!(html.contains("check-type-card"));
    assert!(html.contains(">domain<"));
    assert!(html.contains(">dns<"));
    assert!(!html.contains(">tls cert<"));
    // Overrides the card text, so it has to carry the reason too.
    assert!(
        html.contains(
            r#"aria-label="Add a DNS record check for app.acme.com. Resolution can break"#
        )
    );
    // Entity-escaped separator, so match the halves rather than the raw URL.
    assert!(html.contains("/targets/new?kind=domain_expiry"));
    assert!(html.contains("host=acme.com"));
}

#[test]
fn detail_header_folds_secondary_actions_into_overflow_menu() {
    let html = sample_page().render().unwrap();
    // Primary actions stay visible.
    assert!(html.contains("run check now"));
    assert!(html.contains("data-share-open"));
    // Secondary actions live inside the ⋯ overflow menu.
    assert!(html.contains("hdr-menu__panel"));
    assert!(html.contains(r#"class="hdr-menu__item""#));
    assert!(html.contains("hdr-menu__item--danger"));
}

#[test]
fn detail_delete_uses_shared_confirm_modal_not_browser_dialog() {
    let html = sample_page().render().unwrap();
    assert!(html.contains("data-confirm-modal"));
    assert!(html.contains(r#"data-confirm-title="Delete monitor?""#));
    assert!(html.contains("data-confirm-danger"));
    assert!(!html.contains("hx-confirm"));
}

#[test]
fn detail_header_shows_shared_chip_only_when_links_exist() {
    // No links → no chip.
    assert!(!sample_page().render().unwrap().contains("[ shared:"));
    // Live links → a chip that opens the share modal.
    let mut p = sample_page();
    p.share_count = 2;
    let html = p.render().unwrap();
    assert!(html.contains("[ shared:2 ]"));
    assert!(html.contains("data-share-open"));
}

#[test]
fn detail_header_warns_when_the_monitor_reaches_nobody() {
    // Routed somewhere: no chip, no banner, no noise.
    let html = sample_page().render().unwrap();
    assert!(!html.contains("[ alerts:none ]"));
    assert!(!html.contains("alerts nobody"));

    let mut p = sample_page();
    p.alerts_nobody = true;
    let html = p.render().unwrap();
    assert!(html.contains("[ alerts:none ]"));
    assert!(html.contains("This monitor alerts nobody."));
    // A warning with nowhere to go is just an accusation.
    assert!(html.contains("/settings/notifications"));
}

#[test]
fn range_options_mark_active() {
    let opts = build_range_options("7d", &RANGE_KEYS);
    assert!(opts.iter().any(|o| o.key == "7d" && o.selected));
    assert_eq!(opts.iter().filter(|o| o.selected).count(), 1);
}

#[test]
fn wider_status_window_returns_some_for_short_user_range() {
    let to = DateTime::parse_from_rfc3339("2026-05-13T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let win = wider_status_window(to - Duration::hours(1), to).expect("widen");
    assert_eq!(win.from, to - Duration::days(LAST_RESULT_WINDOW_DAYS));
    assert_eq!(win.to, to);
}

#[test]
fn wider_status_window_returns_none_for_wide_user_range() {
    let to = DateTime::parse_from_rfc3339("2026-05-13T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert!(wider_status_window(to - Duration::days(30), to).is_none());
    assert!(wider_status_window(to - Duration::days(LAST_RESULT_WINDOW_DAYS), to).is_none());
}

fn sample_live() -> DetailLive {
    DetailLive {
        id: "00000000-0000-0000-0000-000000000001".into(),
        name: "api".into(),
        range: "24h",
        enabled: true,
        last_status: "up",
        uptime: Arc::new(UptimeStatsView {
            total: 100,
            up: 99,
            down: 1,
            degraded: 0,
            error: 0,
            uptime_pct: Some("99.00".into()),
        }),
        kpi: Arc::new(KpiTrend::default()),
        pings: None,
        last_at_iso: Arc::from("2026-05-13T12:00:00Z"),
        selected_region: None,
        segments: Arc::from(Vec::<StatusSeg>::new()),
        ribbon_oob: true,
        liveness: None,
        liveness_oob: true,
    }
}

#[test]
fn live_partial_renders_kpi_swap_target_plus_oob_ribbon() {
    let html = sample_live().render().unwrap();
    assert!(!html.contains("<!doctype html>"));
    assert!(html.contains(r#"id="detail-live-kpi""#));
    assert!(html.contains(
        "hx-get=\"/web/partials/targets/00000000-0000-0000-0000-000000000001/live?range=24h\""
    ));
    assert!(html.contains(
        r#"hx-trigger="every 60s, sm:refresh-live from:body, sm:poll-resume from:body""#
    ));
    assert!(html.contains("data-poll-pause"));
    assert!(html.contains("data-reload-on-404"));
    assert!(!html.contains("hx-on::response-error"));
    assert!(html.contains(r#"hx-swap="outerHTML""#));
    assert!(html.contains(r#"data-newest-ts="2026-05-13T12:00:00Z""#));
    // An outerHTML swap inserts every top-level node beside the target.
    assert!(html.starts_with("<section id=\"detail-live-kpi\""));
    assert!(html.contains("</section><div id=\"detail-ribbon\""));
    assert!(html.contains("</div><span id=\"detail-status-badge\""));
    assert!(html.ends_with("</span>"));
    // The ribbon rides along as an out-of-band swap so the newest cell stays
    // current without a full-page reload; the recent-results table is gone.
    assert!(html.contains(r#"id="detail-ribbon""#));
    assert!(html.contains(r#"hx-swap-oob="true""#));
    assert!(!html.contains(r#"id="detail-live-recent""#));
    assert!(html.contains("99.00"));
}

/// A heartbeat monitor renders a third top-level node between the ribbon and
/// the badge, so it has a seam the probed shape never exercises.
#[test]
fn live_partial_seams_stay_bare_for_a_heartbeat_monitor() {
    let mut live = sample_live();
    live.liveness = Some(super::HeartbeatLiveness {
        pending: false,
        since: None,
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    });
    let html = live.render().unwrap();
    assert!(html.starts_with("<section id=\"detail-live-kpi\""));
    assert!(html.contains("</section><div id=\"detail-ribbon\""));
    assert!(html.contains("</div><div id=\"hb-liveness\""));
    assert!(html.contains("</div><span id=\"detail-status-badge\""));
    assert!(html.ends_with("</span>"));
}

#[test]
fn detail_page_loads_the_polling_module() {
    let html = sample_page().render().unwrap();
    assert!(html.contains("data-poll-pause"));
    // A deleted monitor 404s the live endpoint; reload onto the real 404.
    assert!(html.contains("data-reload-on-404"));
    // Without the module nothing honours either marker.
    assert!(html.contains(&crate::web::assets::url("js/ui/polling.js")));
    // The hx-on it replaced was dead twice over: Function() under the CSP,
    // and dropping hx-trigger cancels neither timer nor from:body listeners.
    assert!(!html.contains("hx-on::response-error"));
}

#[test]
fn detail_page_orders_kpi_charts_ribbon() {
    let html = sample_page().render().unwrap();
    assert!(html.contains(r#"id="detail-ribbon""#));
    assert!(html.contains(r#"id="detail-live-kpi""#));
    assert!(html.contains(r#"id="latency-chart""#));
    // Recent-results table replaced by the ribbon + drill drawer, which now
    // sits below the charts (and the by-region table).
    assert!(!html.contains(r#"id="detail-live-recent""#));
    let kpi_pos = html.find(r#"id="detail-live-kpi""#).expect("kpi present");
    let chart_pos = html.find(r#"id="latency-chart""#).expect("chart present");
    let ribbon_pos = html.find(r#"id="detail-ribbon""#).expect("ribbon present");
    assert!(kpi_pos < chart_pos, "KPI renders before charts");
    assert!(chart_pos < ribbon_pos, "ribbon renders after charts");
}

fn breakdown_row(label: &str) -> RegionBreakdownRow {
    RegionBreakdownRow {
        region: "eu-helsinki".into(),
        filter_href: "/targets/x?range=24h&region=eu-helsinki".into(),
        region_label: label.into(),
        uptime_label: "100.00".into(),
        p50_label: "100 ms".into(),
        p95_label: "200 ms".into(),
        p99_label: "300 ms".into(),
        last_status: "up".into(),
        selected: false,
        flaps: 0,
    }
}

fn multi_region_page() -> DetailPage {
    let mut p = sample_page();
    p.regions = vec![
        crate::web::views::region_display::LabeledRegion {
            id: "eu-helsinki".into(),
            label: "EU North".into(),
        },
        crate::web::views::region_display::LabeledRegion {
            id: "us-east".into(),
            label: "US East".into(),
        },
    ];
    p
}

#[test]
fn multi_region_latency_chart_plots_median_and_says_so() {
    let mut p = multi_region_page();
    p.region_breakdown = vec![breakdown_row("EU North"), breakdown_row("US East")];
    let html = p.render().unwrap();
    assert!(html.contains("latency by region (median)"));
    assert!(!html.contains("latency (p50/p95/p99)"));
    // Both charts go cross-region: median per region, phase bar per region.
    assert_eq!(html.matches("data-overlay-endpoint").count(), 2);
    assert!(html.contains("latency breakdown by region"));
    // A bar click resolves to a region by finding its row, so the id must be on it.
    assert!(html.contains(r#"data-region="eu-helsinki""#));
    // Tail quantiles move to the by-region table.
    assert!(html.contains("300 ms"));
    // Each row filters via a real link, so the row click has something to drive.
    let pos = html.find("region-row__link").expect("region link present");
    let anchor = &html[pos..pos + html[pos..].find("</a>").expect("anchor terminator")];
    assert!(anchor.contains("href=\"/targets/x?range=24h"));
    assert!(anchor.contains("region=eu-helsinki"));
}

#[test]
fn by_region_table_reports_state_changes_per_region() {
    let mut p = multi_region_page();
    let mut quiet = breakdown_row("EU North");
    let mut noisy = breakdown_row("US East");
    noisy.flaps = 271;
    quiet.flaps = 0;
    p.region_breakdown = vec![quiet, noisy];
    let html = p.render().unwrap();
    assert!(html.contains(">flaps 24h<"));
    assert!(html.contains(">271<"));
    assert!(html.contains("tabular-nums text-quiet\">0<"));
}

fn flaps(region: &str, failures: u64, transitions: u64) -> crate::storage::traits::RegionFlaps {
    crate::storage::traits::RegionFlaps {
        region: region.into(),
        failures,
        transitions,
    }
}

#[test]
fn a_clean_window_has_nothing_to_explain() {
    assert!(
        UnconfirmedFailures::new(
            &[flaps("eu-helsinki", 0, 0)],
            &[],
            2,
            crate::domain::RegionIncidentPolicy::Majority,
        )
        .is_none()
    );
    assert!(
        UnconfirmedFailures::new(&[], &[], 2, crate::domain::RegionIncidentPolicy::Majority)
            .is_none()
    );
}

#[test]
fn unconfirmed_failures_rank_regions_and_spell_out_the_quorum() {
    let u = UnconfirmedFailures::new(
        &[
            flaps("eu-helsinki", 240, 273),
            flaps("apac-sg", 0, 0),
            flaps("us-east", 205, 235),
        ],
        &[],
        2,
        crate::domain::RegionIncidentPolicy::Majority,
    )
    .expect("failures present");
    assert_eq!(u.failures, 445);
    assert_eq!(u.transitions, 508);
    assert_eq!(u.regions, vec!["eu-helsinki", "us-east"]);
    assert_eq!(u.region_count, 3);
    assert_eq!(u.quorum, 2);
}

#[test]
fn incidents_page_explains_failures_that_never_opened_one() {
    let mut p = sample_incidents_page(vec![], 0);
    p.unconfirmed = UnconfirmedFailures::new(
        &[flaps("eu-helsinki", 240, 273), flaps("us-east", 205, 235)],
        &[],
        2,
        crate::domain::RegionIncidentPolicy::Majority,
    );
    let html = p.render().unwrap();
    assert!(html.contains("no incidents in the last 30d"));
    assert!(html.contains("445 checks failed in the last 24 hours"));
    assert!(html.contains("508 state changes"));
    assert!(html.contains("eu-helsinki, us-east"));
    assert!(html.contains("2 of the 2 regions reporting"));
    assert!(html.contains("/docs/hosted/regions"));
}

#[test]
fn one_failing_region_is_not_reported_as_every_region() {
    let page = |flaps: &[crate::storage::traits::RegionFlaps]| {
        let mut p = sample_incidents_page(vec![], 0);
        p.unconfirmed =
            UnconfirmedFailures::new(flaps, &[], 2, crate::domain::RegionIncidentPolicy::Majority);
        p.render().unwrap()
    };

    // One of three failing: blaming "every region" contradicts the line above it.
    let html = page(&[
        flaps("eu-helsinki", 3, 6),
        flaps("apac-sg", 0, 0),
        flaps("us-east", 0, 0),
    ]);
    assert!(html.contains("Failing region:"));
    assert!(!html.contains("from every region"));
    assert!(html.contains("One region failing"));

    // All three failing: the original wording is the accurate one, so it stays.
    let html = page(&[
        flaps("eu-helsinki", 3, 6),
        flaps("apac-sg", 2, 4),
        flaps("us-east", 1, 2),
    ]);
    assert!(html.contains("from every region"));
}

#[test]
fn an_old_incident_does_not_explain_failures_inside_the_flap_window() {
    let cutoff = Utc::now() - chrono::Duration::hours(FLAP_WINDOW_HOURS);
    let mut stale = resolved_row();
    stale.started_at = Utc::now() - chrono::Duration::days(29);
    stale.ended_at = Some(Utc::now() - chrono::Duration::days(29));
    assert!(!explained_by_incident(&[stale], cutoff));
    assert!(explained_by_incident(&[ongoing_row()], cutoff));

    let mut recent = resolved_row();
    recent.started_at = Utc::now() - chrono::Duration::hours(2);
    recent.ended_at = Some(Utc::now() - chrono::Duration::hours(1));
    assert!(explained_by_incident(&[recent], cutoff));
}

#[test]
fn a_clean_incidents_page_says_nothing_extra() {
    let html = sample_incidents_page(vec![], 0).render().unwrap();
    assert!(html.contains("no incidents in the last 30d"));
    assert!(!html.contains("state change"));
}

#[test]
fn single_reporting_region_keeps_quantile_chart() {
    let mut p = multi_region_page();
    p.region_breakdown = vec![breakdown_row("EU North")];
    let html = p.render().unwrap();
    // Nothing to compare across, so the overlay would drop the tail for nothing.
    assert!(html.contains("latency (p50/p95/p99)"));
    assert!(!html.contains("data-overlay-endpoint"));
    // The breakdown stays a time series, scoped to the one region reporting.
    assert!(!html.contains("latency breakdown by region"));
}

#[test]
fn selecting_a_region_restores_the_quantile_chart() {
    let mut p = multi_region_page();
    p.region_breakdown = vec![breakdown_row("EU North"), breakdown_row("US East")];
    p.selected_region = Some("us-east".into());
    let html = p.render().unwrap();
    assert!(html.contains("latency (p50/p95/p99)"));
    assert!(!html.contains("data-overlay-endpoint"));
    // Both charts scope to the picked region, server-side.
    assert!(!html.contains("latency breakdown by region"));
    let pos = html
        .find(r#"id="breakdown-chart""#)
        .expect("breakdown chart");
    let el = &html[pos..pos + html[pos..].find("></div>").expect("chart terminator")];
    assert!(el.contains("region=us-east"));
}

fn kpi_ranges() -> (ClampedRange, ClampedRange) {
    let base = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let range = ClampedRange::unclamped(TimeRange {
        from: base,
        to: base + Duration::hours(1),
    });
    let prior = ClampedRange::unclamped(TimeRange {
        from: base - Duration::hours(1),
        to: base,
    });
    (range, prior)
}

fn stats(total: u64, up: u64, down: u64, error: u64, pct: f64) -> UptimeStats {
    UptimeStats {
        total,
        up,
        down,
        degraded: 0,
        error,
        uptime_pct: Some(pct),
    }
}

fn trend(
    current: &UptimeStats,
    prior: &UptimeStats,
    avail: &[AvailabilityBucket],
    range: ClampedRange,
    prior_range: ClampedRange,
    confirmed: bool,
) -> KpiTrend {
    build_kpi_trend(KpiInputs {
        current,
        prior,
        cur_incidents: &[],
        prior_incidents: &[],
        avail,
        range,
        prior_range,
        spark_bucket_seconds: 60,
        confirmed,
    })
}

fn one_bucket(range: ClampedRange, total: u64, up: u64) -> [AvailabilityBucket; 1] {
    [AvailabilityBucket {
        bucket_ts: range.from.timestamp(),
        total,
        up,
    }]
}

#[test]
fn build_kpi_trend_deltas_vs_prior() {
    let (range, prior_range) = kpi_ranges();
    let avail = one_bucket(range, 100, 99);
    let kpi = trend(
        &stats(100, 99, 1, 0, 99.0),
        &stats(100, 95, 3, 2, 95.0),
        &avail,
        range,
        prior_range,
        false,
    );
    assert!(!kpi.spark_path.is_empty());
    assert_eq!(kpi.uptime_delta.unwrap().body, "+4.00 pp");
    assert_eq!(kpi.up_delta.unwrap().body, "+4");
    assert_eq!(kpi.down_delta.unwrap().body, "-2");
    assert_eq!(kpi.error_delta.unwrap().body, "-2");
}

#[test]
fn build_kpi_trend_no_prior_keeps_spark_drops_deltas() {
    let (range, prior_range) = kpi_ranges();
    let avail = one_bucket(range, 100, 99);
    let kpi = trend(
        &stats(100, 99, 1, 0, 99.0),
        &stats(0, 0, 0, 0, 0.0),
        &avail,
        range,
        prior_range,
        false,
    );
    assert!(!kpi.spark_path.is_empty());
    assert!(kpi.uptime_delta.is_none());
    assert!(kpi.up_delta.is_none());
}

#[test]
fn build_kpi_trend_empty_current_drops_deltas() {
    let (range, prior_range) = kpi_ranges();
    let avail = one_bucket(range, 0, 0);
    // No samples this window: an empty current must not read as -100pp.
    let kpi = trend(
        &stats(0, 0, 0, 0, 0.0),
        &stats(100, 95, 3, 2, 95.0),
        &avail,
        range,
        prior_range,
        false,
    );
    assert!(kpi.uptime_delta.is_none());
    assert!(kpi.up_delta.is_none());
}

#[test]
fn build_kpi_trend_truncated_prior_drops_deltas() {
    let base = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let range = ClampedRange::unclamped(TimeRange {
        from: base,
        to: base + Duration::hours(1),
    });
    // Prior shortened by the retention floor (30m vs the 1h current).
    let prior_range = ClampedRange::unclamped(TimeRange {
        from: base - Duration::minutes(30),
        to: base,
    });
    let avail = one_bucket(range, 100, 99);
    let kpi = trend(
        &stats(100, 99, 1, 0, 99.0),
        &stats(50, 48, 1, 1, 96.0),
        &avail,
        range,
        prior_range,
        false,
    );
    assert!(!kpi.spark_path.is_empty());
    assert!(
        kpi.up_delta.is_none(),
        "skewed unequal-window counts dropped"
    );
}

#[test]
fn confirmed_spark_decomposes_headline_not_raw_ratio() {
    let (range, prior_range) = kpi_ranges();
    // Raw bucket reads 50% up, but no confirmed incident overlaps it.
    let avail = one_bucket(range, 100, 50);
    let current = stats(100, 50, 50, 0, 50.0);
    let prior = stats(100, 100, 0, 0, 100.0);
    let confirmed = trend(&current, &prior, &avail, range, prior_range, true);
    let raw = trend(&current, &prior, &avail, range, prior_range, false);
    assert_ne!(confirmed.spark_path, raw.spark_path);
    // Confirmed: flat at the top (no incident → 100%). Raw: mid (50%).
    assert!(
        confirmed.spark_path.ends_with(" 0.0"),
        "confirmed flat-top: {}",
        confirmed.spark_path
    );
    assert!(
        raw.spark_path.ends_with(" 11.0"),
        "raw 50% mid: {}",
        raw.spark_path
    );
}

#[test]
fn resolve_range_key_clamps_to_allowed() {
    assert_eq!(
        resolve_range_key(Some("1h"), &RANGE_KEYS, DEFAULT_RANGE),
        "1h"
    );
    assert_eq!(
        resolve_range_key(Some("garbage"), &RANGE_KEYS, DEFAULT_RANGE),
        "24h"
    );
    assert_eq!(resolve_range_key(None, &RANGE_KEYS, DEFAULT_RANGE), "24h");
}

#[test]
fn resolve_incident_range_key_defaults_to_30d() {
    let k = |s| resolve_range_key(s, &INCIDENT_RANGE_KEYS, INCIDENT_DEFAULT_RANGE);
    assert_eq!(k(None), "30d");
    assert_eq!(k(Some("")), "30d");
    assert_eq!(k(Some("garbage")), "30d");
    assert_eq!(k(Some("24h")), "24h");
    assert_eq!(k(Some("90d")), "90d");
}

#[test]
fn incident_row_falls_back_to_start_end_when_duration_secs_missing() {
    use chrono::TimeZone;
    let start = Utc.with_ymd_and_hms(2026, 5, 12, 8, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 5, 12, 8, 7, 0).unwrap();
    let inc = crate::domain::Incident {
        id: Uuid::nil(),
        target_id: Some(Uuid::nil()),
        started_at: start,
        ended_at: Some(end),
        status: crate::domain::CheckStatus::Down,
        duration_secs: None,
        check_count: 7,
        counts_as_downtime: true,
        error_sample: None,
        severity: Default::default(),
        public_title: None,
        public_description: None,
        created_at: None,
        updated_at: None,
        updates: Vec::new(),
        regions_down: Vec::new(),
        regions_up: Vec::new(),
    };
    let row = IncidentRow::from(inc);
    assert!(!row.ongoing);
    assert_eq!(row.duration_secs, Some(7 * 60));
}

fn sample_incidents_page(incidents: Vec<IncidentRow>, ongoing_count: usize) -> IncidentsPage {
    IncidentsPage {
        active_tab: "targets",
        subtab: SUBTAB_INCIDENTS,
        ongoing_count,
        alerts_nobody: false,
        id: "00000000-0000-0000-0000-000000000001".into(),
        name: "api".into(),
        kind: "HTTP",
        address: "https://example.com".into(),
        interval_s: 60,
        enabled: true,
        tags: vec!["prod".into()],
        managed_by: None,
        share_count: 0,
        last_status: "down",
        last_at_iso: "2026-05-13T12:00:00Z".into(),
        incidents,
        incidents_has_more: false,
        results_base: "/api/v1/targets/00000000-0000-0000-0000-000000000001".into(),
        range: "30d",
        range_options: build_range_options("30d", &INCIDENT_RANGE_KEYS),
        range_base_path: "/targets/00000000-0000-0000-0000-000000000001/incidents".into(),
        from_iso: "2026-04-13T12:00:00Z".into(),
        to_iso: "2026-05-13T12:00:00Z".into(),
        from_human: "2026-04-13 12:00 UTC".into(),
        to_human: "2026-05-13 12:00 UTC".into(),
        selected_region: None,
        unconfirmed: None,
    }
}

fn ongoing_row() -> IncidentRow {
    use chrono::TimeZone;
    IncidentRow {
        id: Uuid::from_u128(0x0000_0001),
        severity: "down",
        started_at: Utc.with_ymd_and_hms(2026, 5, 13, 11, 50, 0).unwrap(),
        ended_at: None,
        duration_secs: None,
        check_count: 4,
        error_sample: "connection refused".into(),
        ongoing: true,
        counts_as_downtime: true,
    }
}

fn resolved_row() -> IncidentRow {
    use chrono::TimeZone;
    IncidentRow {
        id: Uuid::from_u128(0x0000_0002),
        severity: "down",
        started_at: Utc.with_ymd_and_hms(2026, 5, 12, 8, 0, 0).unwrap(),
        ended_at: Some(Utc.with_ymd_and_hms(2026, 5, 12, 8, 7, 0).unwrap()),
        duration_secs: Some(420),
        check_count: 7,
        error_sample: "HTTP 503 Service Unavailable".into(),
        ongoing: false,
        counts_as_downtime: true,
    }
}

#[test]
fn incidents_page_renders_empty_state_when_no_incidents() {
    let html = sample_incidents_page(vec![], 0).render().unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("no incidents in the last 30d"));
    assert!(!html.contains("<table"));
    assert!(html.contains("aria-current=\"page\""));
}

#[test]
fn incidents_page_renders_table_rows_with_ongoing_emphasis() {
    let html = sample_incidents_page(vec![ongoing_row(), resolved_row()], 1)
        .render()
        .unwrap();
    assert!(html.contains("<table"));
    // Ongoing emphasis: themed left border + pulsing badge + severity-tagged label.
    assert!(html.contains("sm-incident-ongoing"));
    assert!(html.contains("animate-pulse"));
    assert!(html.contains("ongoing · down"));
    // Resolved row uses the regular severity badge.
    assert!(html.contains(r#"status-badge status-badge--down">down<"#));
    // Each row has a hidden detail row + the chevron for expand.
    assert!(html.contains("data-incident-detail"));
    assert!(html.contains("data-incident-chevron"));
    // Row carries the window data the JS uses to fetch the timeline.
    assert!(html.contains(r#"data-from="2026-05-13T11:50:00Z""#));
}

/// An unexplained row beside a 100% figure is the confusion, pointed the other way.
#[test]
fn an_excluded_incident_says_so_beside_its_duration() {
    let counted = sample_incidents_page(vec![resolved_row()], 1)
        .render()
        .unwrap();
    assert!(!counted.contains("not counted"), "{counted}");

    let mut row = resolved_row();
    row.counts_as_downtime = false;
    let excluded = sample_incidents_page(vec![row], 1).render().unwrap();
    assert!(excluded.contains("not counted"), "{excluded}");
}

#[test]
fn incidents_page_ongoing_badge_appears_on_tab_strip() {
    let html = sample_incidents_page(vec![ongoing_row()], 1)
        .render()
        .unwrap();
    assert!(html.contains(r#"id="tab-incidents-badge""#));
    assert!(html.contains(r#"aria-label="1 ongoing">1<"#));
}

#[test]
fn incidents_page_omits_tab_badge_when_no_ongoing() {
    let html = sample_incidents_page(vec![resolved_row()], 0)
        .render()
        .unwrap();
    assert!(!html.contains(r#"id="tab-incidents-badge""#));
}

#[test]
fn detail_page_tab_strip_marks_monitor_subtab_active() {
    let html = sample_page().render().unwrap();
    // Both tabs link to their own paths.
    assert!(html.contains(r#"href="/targets/00000000-0000-0000-0000-000000000001""#));
    assert!(html.contains(r#"href="/targets/00000000-0000-0000-0000-000000000001/incidents""#));
    // The Monitor anchor must carry aria-current; the Incidents one must not.
    let monitor_href = r#"href="/targets/00000000-0000-0000-0000-000000000001""#;
    let monitor_pos = html.find(monitor_href).expect("monitor link present");
    let monitor_anchor_end = html[monitor_pos..]
        .find("</a>")
        .expect("monitor anchor terminator");
    let monitor_anchor = &html[monitor_pos..monitor_pos + monitor_anchor_end];
    assert!(monitor_anchor.contains("aria-current=\"page\""));
}

#[test]
fn incidents_page_subtab_active_is_incidents() {
    let html = sample_incidents_page(vec![], 0).render().unwrap();
    let incidents_href = r#"href="/targets/00000000-0000-0000-0000-000000000001/incidents""#;
    let pos = html.find(incidents_href).expect("incidents link present");
    let anchor_end = html[pos..].find("</a>").expect("anchor terminator");
    let anchor = &html[pos..pos + anchor_end];
    assert!(anchor.contains("aria-current=\"page\""));
}

/// Alerts going quiet has to be explained where the operator lands, not only
/// in the alert stream they stopped receiving.
#[test]
fn the_detail_banner_explains_a_held_alert() {
    let mut page = sample_page();
    assert!(!page.render().unwrap().contains("flapping"));

    page.flapping_opens = Some(7);
    page.flap_hold_minutes = 10;
    let html = page.render().unwrap();
    assert!(html.contains("This monitor is flapping"));
    assert!(html.contains("failed and recovered 7 times"));
    assert!(
        html.contains("more than 10"),
        "the banner must say a real outage still alerts"
    );
}

/// Every surface reads the badge off `badge_status`, so a pending heartbeat
/// that already has results cannot render one answer and swap to another on
/// the first live poll. Reachable by switching an http monitor to heartbeat:
/// the stored rows keep the target id, the ping state starts empty.
#[test]
fn a_pending_heartbeat_with_results_keeps_the_status_it_has() {
    let wired = super::HeartbeatLiveness {
        pending: false,
        since: None,
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    };
    let unwired = super::HeartbeatLiveness {
        pending: true,
        since: None,
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    };
    assert_eq!(super::badge_status("up", Some(&unwired)), "up");
    assert_eq!(super::badge_status("down", Some(&unwired)), "down");
    assert_eq!(
        super::badge_status("", Some(&unwired)),
        super::WAITING_FOR_PING
    );
    assert_eq!(super::badge_status("", Some(&wired)), "");
}

/// A paused monitor is not waiting on anything: nothing dispatches it and the
/// nudge skips it, so promising a schedule would be the one surface that
/// disagrees.
#[test]
fn a_paused_heartbeat_does_not_claim_to_be_waiting() {
    let mut p = sample_page();
    p.kind = "HEARTBEAT";
    p.enabled = false;
    p.last_status = "";
    p.liveness = Some(super::HeartbeatLiveness {
        pending: false,
        since: Some(chrono::Utc::now()),
        due_at: None,
        down_at: None,
        late: false,
        overdue: false,
    });
    let html = p.render().unwrap();
    assert!(!html.contains("waiting for first ping"));
    assert!(!html.contains("the schedule starts at the first ping"));
    assert!(html.contains("no data"));
}
