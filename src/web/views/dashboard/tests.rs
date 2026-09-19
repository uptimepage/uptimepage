use std::collections::HashMap;

use chrono::Duration;

use crate::domain::IncidentSeverity;
use crate::domain::metrics::{DashboardSparkBucket, FleetRibbonBucket, PriorPeriodSummary};
use crate::storage::IncidentBrief;

use super::charts::*;
use super::*;

fn sample_kpis() -> DashboardKpis {
    DashboardKpis {
        uptime_pct_label: "99.92%".into(),
        avg_response_ms_label: "142 ms".into(),
        checks_label: "17.3k".into(),
        checks_successful_label: "17.0k successful".into(),
        incidents: 3,
    }
}

fn sample_kpi_cards() -> Vec<KpiCardSpec> {
    let zero = PriorPeriodSummary::default();
    build_kpi_cards(
        &sample_kpis(),
        "24h",
        "17.0k successful".into(),
        &zero,
        &zero,
        &FleetSparks::default(),
    )
}

fn sample_row(name: &str, status: &'static str) -> DashboardRow {
    let spark = vec![Some(100.0), Some(120.0), None, Some(110.0)];
    let (spark_path, spark_fill, baseline_y) = render_spark_path(&spark);
    DashboardRow {
        id: "11111111-1111-1111-1111-111111111111".into(),
        name: name.into(),
        kind: "HTTP",
        address: "https://api.example.com".into(),
        enabled: true,
        last_status: status,
        p50_label: "100 ms".into(),
        p95_label: "120 ms".into(),
        err_pct_label: "0.0".into(),
        uptime_pct_label: "99.92".into(),
        samples: 720,
        spark,
        spark_path,
        spark_fill,
        spark_baseline_y: baseline_y,
    }
}

fn sample_ribbon() -> FleetRibbon {
    build_fleet_ribbon(&[], snapped_from(), &HashMap::new())
}

fn sample_page() -> DashboardPage {
    let rows = vec![sample_row("api", "up"), sample_row("worker", "degraded")];
    DashboardPage {
        active_tab: "dashboard",
        range: "24h",
        range_options: build_range_options("24h", &RANGE_KEYS),
        kpi_cards: Arc::from(sample_kpi_cards().into_boxed_slice()),
        rows: Arc::from(rows.into_boxed_slice()),
        matches: 2,
        truncated: false,
        onboarding: false,
        active_incidents: Arc::from(Vec::<DashboardActiveIncident>::new().into_boxed_slice()),
        status_counts: StatusCounts {
            up: 1,
            degraded: 1,
            ..Default::default()
        },
        type_counts: Arc::from(
            vec![TypeCount {
                label: "All",
                count: 2,
                active: true,
            }]
            .into_boxed_slice(),
        ),
        ribbon: sample_ribbon(),
        regions: Vec::new(),
        selected_region: None,
        status_options: build_range_options(FILTER_ANY, &STATUS_FILTERS),
        selected_status: None,
        selected_kind: None,
        drill: None,
        restored_notice: false,
        joined_notice: None,
        invite_missed_notice: false,
    }
}

#[test]
fn page_renders_chrome_and_kpis() {
    let html = sample_page().render().unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("Dashboard"));
    assert!(html.contains("99.92%"));
    assert!(html.contains("142 ms"));
    assert!(html.contains("17.3k"));
    // Range tabs.
    for k in &RANGE_KEYS {
        assert!(html.contains(&format!(">{k}<")));
    }
    // Row cells.
    assert!(html.contains("api"));
    assert!(html.contains("worker"));
    // htmx swap target for tab clicks.
    assert!(html.contains(r#"id="dashboard-table""#));
    assert!(html.contains(r#"hx-get="/web/partials/dashboard"#));
}

#[test]
fn partial_omits_chrome() {
    let partial = DashboardTablePartial {
        range: "7d",
        range_options: build_range_options("7d", &RANGE_KEYS),
        kpi_cards: Arc::from(sample_kpi_cards().into_boxed_slice()),
        rows: Arc::from(vec![sample_row("api", "up")].into_boxed_slice()),
        matches: 1,
        truncated: false,
        active_incidents: Arc::from(Vec::<DashboardActiveIncident>::new().into_boxed_slice()),
        status_counts: StatusCounts {
            up: 1,
            ..Default::default()
        },
        type_counts: Arc::from(
            vec![TypeCount {
                label: "All",
                count: 1,
                active: true,
            }]
            .into_boxed_slice(),
        ),
        ribbon: sample_ribbon(),
        regions: Vec::new(),
        selected_region: None,
        status_options: build_range_options(FILTER_ANY, &STATUS_FILTERS),
        selected_status: None,
        selected_kind: None,
        drill: None,
    };
    let html = partial.render().unwrap();
    assert!(!html.contains("<!doctype html>"));
    assert!(!html.contains("<nav"));
    assert!(html.contains(r#"id="dashboard-table""#));
    assert!(html.contains("99.92%"));
    // An outerHTML swap inserts every top-level node beside the target.
    assert!(html.starts_with("<div id=\"dashboard-table\""));
    assert!(html.ends_with("</div>"));
    // The resume carries the filter guard too: without it a catch-up poll
    // built from pre-click params can land after the filtered swap.
    assert!(html.contains(
        r#"hx-trigger="every 5s [!document.querySelector('#dashboard-table .htmx-request')], sm:poll-resume[!document.querySelector('#dashboard-table .htmx-request')] from:body""#
    ));
    // Re-armed on every swap, so the pause survives the zone replacing itself.
    assert!(html.contains("data-poll-pause"));
}

#[test]
fn onboarding_state_skips_table() {
    let kpis = DashboardKpis {
        uptime_pct_label: "—".into(),
        avg_response_ms_label: "—".into(),
        checks_label: "0".into(),
        checks_successful_label: "0 successful".into(),
        incidents: 0,
    };
    let page = DashboardPage {
        active_tab: "dashboard",
        range: "24h",
        range_options: build_range_options("24h", &RANGE_KEYS),
        kpi_cards: Arc::from({
            let zero = PriorPeriodSummary::default();
            build_kpi_cards(
                &kpis,
                "24h",
                "0 successful".into(),
                &zero,
                &zero,
                &FleetSparks::default(),
            )
            .into_boxed_slice()
        }),
        rows: Arc::from(Vec::<DashboardRow>::new().into_boxed_slice()),
        matches: 0,
        truncated: false,
        onboarding: true,
        active_incidents: Arc::from(Vec::<DashboardActiveIncident>::new().into_boxed_slice()),
        status_counts: StatusCounts::default(),
        type_counts: Arc::from(Vec::<TypeCount>::new().into_boxed_slice()),
        ribbon: sample_ribbon(),
        regions: Vec::new(),
        selected_region: None,
        status_options: build_range_options(FILTER_ANY, &STATUS_FILTERS),
        selected_status: None,
        selected_kind: None,
        drill: None,
        restored_notice: false,
        joined_notice: None,
        invite_missed_notice: false,
    };
    let html = page.render().unwrap();
    assert!(html.contains("nothing to watch yet."));
    assert!(html.contains("add your first monitor"));
    assert!(!html.contains(r#"id="dashboard-table""#));
}

#[test]
fn format_count_compacts() {
    assert_eq!(format_count(42), "42");
    assert_eq!(format_count(1_500), "1.5k");
    assert_eq!(format_count(17_300), "17.3k");
    assert_eq!(format_count(2_400_000), "2.4M");
}

#[test]
fn pct_label_handles_empty_window() {
    assert_eq!(pct_label(0, 0), "—");
    assert_eq!(pct_label(1_000, 999), "99.90%");
}

fn spark_bucket(
    from: DateTime<Utc>,
    minute: i64,
    avg_ms: f32,
    checks: u64,
    up: u64,
) -> DashboardSparkBucket {
    DashboardSparkBucket {
        target_id: Uuid::nil(),
        bucket_ts: from.timestamp() + minute * 60,
        avg_ms,
        checks,
        up,
    }
}

#[test]
fn fleet_sparks_weight_by_check_count() {
    let from = Utc::now() - Duration::minutes(SPARK_MINUTES);
    let sparks = fleet_sparks(
        &[
            spark_bucket(from, 0, 100.0, 1, 1),
            spark_bucket(from, 0, 200.0, 59, 30),
        ],
        from,
    );
    // A mean of the two means would say 150 ms and 75 % uptime.
    assert_eq!(sparks.avg_ms[0].unwrap().round(), 198.0);
    assert_eq!(sparks.uptime[0].unwrap().round(), 52.0);
    assert_eq!(sparks.checks[0].unwrap(), 60.0);
    assert!(sparks.avg_ms[1].is_none());
}

#[test]
fn each_kpi_card_plots_its_own_metric() {
    let from = Utc::now() - Duration::minutes(SPARK_MINUTES);
    // Latency climbs while checks and uptime fall — three shapes, not one.
    let sparks = fleet_sparks(
        &[
            spark_bucket(from, 0, 100.0, 40, 40),
            spark_bucket(from, 1, 400.0, 10, 5),
        ],
        from,
    );
    let zero = PriorPeriodSummary::default();
    let cards = build_kpi_cards(
        &sample_kpis(),
        "24h",
        "17.0k successful".into(),
        &zero,
        &zero,
        &sparks,
    );
    let paths: Vec<&str> = cards.iter().map(|c| c.spark_path.as_str()).collect();
    assert!(paths.iter().all(|p| !p.is_empty()));
    assert_ne!(paths[0], paths[1]);
    assert_ne!(paths[1], paths[2]);
    assert_ne!(paths[0], paths[2]);
}

#[test]
fn render_spark_path_connects_across_gaps() {
    let (line, fill, _) = render_spark_path(&[Some(1.0), Some(2.0), None, Some(3.0)]);
    assert_eq!(line.matches('M').count(), 1);
    assert_eq!(line.matches('L').count(), 2);
    assert!(!fill.is_empty());
}

#[test]
fn render_spark_path_renders_sparse_interval_monitor() {
    // ≥2-min cadence leaves every filled bucket isolated.
    let mut spark = vec![None; 60];
    for (i, slot) in spark.iter_mut().enumerate() {
        if i % 5 == 0 {
            *slot = Some(100.0 + i as f32);
        }
    }
    let (line, fill, _) = render_spark_path(&spark);
    assert!(line.starts_with('M'));
    assert_eq!(line.matches('M').count(), 1);
    assert!(line.contains('L'));
    assert!(!fill.is_empty());
}

#[test]
fn render_spark_path_partial_window_fills_recent_end_only() {
    // Half-window of data must start past the midpoint, not from x=0.
    let mut spark = vec![None; 60];
    for slot in spark.iter_mut().skip(30) {
        *slot = Some(50.0);
    }
    let (line, _, _) = render_spark_path(&spark);
    let first_x: f32 = line
        .trim_start_matches('M')
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let last_x: f32 = line
        .rsplit('L')
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(first_x > 80.0, "first sample x={first_x}, want recent half");
    assert!(
        (last_x - 160.0).abs() < 0.5,
        "newest sample x={last_x}, want right edge"
    );
}

#[test]
fn render_spark_path_drops_non_finite_samples() {
    // CH avgMerge returns NaN for empty groups; one non-finite must not
    // poison min/max nor emit an "M NaN" path browsers silently drop.
    let (line, fill, _) =
        render_spark_path(&[Some(1.0), Some(f32::NAN), Some(f32::INFINITY), Some(3.0)]);
    assert!(!line.contains("NaN") && !line.contains("inf"));
    assert_eq!(line.matches('L').count(), 1);
    assert!(!fill.is_empty());
}

#[test]
fn render_spark_path_lone_sample_draws_dot() {
    let mut spark = vec![None; 60];
    spark[30] = Some(42.0);
    let (line, fill, _) = render_spark_path(&spark);
    assert_eq!(line.matches('M').count(), 1);
    assert_eq!(line.matches('L').count(), 1);
    assert!(fill.is_empty());
}

#[test]
fn render_spark_path_empty_when_no_data() {
    assert_eq!(render_spark_path(&vec![None; 60]).0, "");
}

#[test]
fn render_spark_path_domain_fixed_scale_independent_of_series_range() {
    let data = [Some(100.0_f32), Some(100.0), Some(50.0)];
    let (fixed, _, _) = render_spark_path_domain(&data, 0.0, 100.0);
    let (auto, _, _) = render_spark_path(&data);
    // Healthy 100 sits at the top (y=0) on both.
    assert!(fixed.starts_with("M0.0 0.0"), "fixed: {fixed}");
    // The dip to 50 lands mid-height on the fixed 0–100 scale, but at the
    // bottom when the line normalises to its own [50,100] range.
    assert!(fixed.trim_end().ends_with("11.0"), "fixed dip: {fixed}");
    assert!(auto.trim_end().ends_with("22.0"), "auto dip: {auto}");
}

fn row_with(enabled: bool, status: &'static str) -> DashboardRow {
    DashboardRow {
        id: String::new(),
        name: String::new(),
        kind: "HTTP",
        address: String::new(),
        enabled,
        last_status: status,
        p50_label: String::new(),
        p95_label: String::new(),
        err_pct_label: String::new(),
        uptime_pct_label: String::new(),
        samples: 0,
        spark: Vec::new(),
        spark_path: String::new(),
        spark_fill: String::new(),
        spark_baseline_y: 0,
    }
}

fn sample_snapshot(rows: Vec<DashboardRow>) -> DashboardSnapshot {
    let n = rows.len();
    DashboardSnapshot {
        rows: Arc::from(rows.into_boxed_slice()),
        kpi_cards: Arc::from(sample_kpi_cards().into_boxed_slice()),
        matches: n,
        truncated: false,
        active_incidents: Arc::from(Vec::<DashboardActiveIncident>::new().into_boxed_slice()),
        status_counts: StatusCounts::default(),
        type_counts: Arc::from(
            vec![
                TypeCount {
                    label: "All",
                    count: n as u32,
                    active: true,
                },
                TypeCount {
                    label: "HTTP",
                    count: n as u32,
                    active: false,
                },
            ]
            .into_boxed_slice(),
        ),
        ribbon: sample_ribbon(),
    }
}

#[test]
fn type_filter_matches_kind_case_insensitively() {
    let snapshot = sample_snapshot(vec![sample_row("api", "up"), sample_row("worker", "up")]);
    let (rows, matches) = filter_rows(&snapshot, None, Some("http"), None);
    assert_eq!(matches, 2);
    assert_eq!(rows.len(), 2);
    let (_, matches) = filter_rows(&snapshot, None, Some("tcp"), None);
    assert_eq!(matches, 0);

    let chips = type_chips(&snapshot, None, Some("http"));
    assert!(!chips[0].active, "All must drop active under a type filter");
    assert!(chips[1].active);
    let chips = type_chips(&snapshot, None, None);
    assert!(chips[0].active);
    assert!(!chips[1].active);
}

#[test]
fn type_chip_counts_follow_status_filter() {
    let snapshot = sample_snapshot(vec![sample_row("api", "up"), sample_row("worker", "down")]);
    let chips = type_chips(&snapshot, Some("down"), None);
    assert_eq!(chips[0].count, 1, "All narrows to status matches");
    assert_eq!(chips[1].count, 1);
    let chips = type_chips(&snapshot, Some("paused"), None);
    assert_eq!(chips[0].count, 0, "chip stays visible at 0, not dropped");
    let chips = type_chips(&snapshot, None, None);
    assert_eq!(chips[0].count, 2, "no status filter keeps snapshot counts");
}

#[test]
fn type_filters_mirror_chip_order() {
    assert_eq!(TYPE_FILTERS[0], FILTER_ANY);
    assert_eq!(TYPE_FILTERS.len(), TYPE_CHIP_ORDER.len() + 1);
    for (key, label) in TYPE_FILTERS[1..].iter().zip(TYPE_CHIP_ORDER) {
        assert_eq!(*key, label.to_ascii_lowercase());
    }
    // One chip per check kind — a new CheckSpec variant must add its chip.
    assert_eq!(
        TYPE_CHIP_ORDER.len(),
        crate::domain::CheckSpec::ALL_KINDS.len()
    );
}

#[test]
fn status_filter_paused_matches_disabled_only() {
    assert!(row_matches_status(&row_with(false, "up"), "paused"));
    assert!(!row_matches_status(&row_with(true, "up"), "paused"));
    assert!(!row_matches_status(&row_with(false, "down"), "down"));
}

#[test]
fn status_filter_down_includes_error() {
    assert!(row_matches_status(&row_with(true, "down"), "down"));
    assert!(row_matches_status(&row_with(true, "error"), "down"));
    assert!(!row_matches_status(&row_with(true, "up"), "down"));
}

#[test]
fn status_filter_up_excludes_never_reported() {
    assert!(row_matches_status(&row_with(true, "up"), "up"));
    assert!(!row_matches_status(&row_with(true, ""), "up"));
}

#[test]
fn tally_status_disabled_wins_over_status() {
    let mut c = StatusCounts::default();
    tally_status(&mut c, &row_with(false, "down"));
    assert_eq!(c.paused, 1);
    assert_eq!(c.down, 0);
}

#[test]
fn tally_status_error_counts_as_down() {
    let mut c = StatusCounts::default();
    tally_status(&mut c, &row_with(true, "error"));
    assert_eq!(c.down, 1);
}

#[test]
fn tally_status_ignores_unknown_status() {
    let mut c = StatusCounts::default();
    tally_status(&mut c, &row_with(true, ""));
    assert_eq!(c.up, 0);
    assert_eq!(c.down, 0);
}

#[test]
fn build_type_counts_empty_returns_empty() {
    assert!(build_type_counts([0; TYPE_CHIP_ORDER.len()]).is_empty());
}

#[test]
fn build_type_counts_emits_all_plus_nonzero_kinds() {
    let idx = |label| TYPE_CHIP_ORDER.iter().position(|k| *k == label).unwrap();
    let mut acc = [0; TYPE_CHIP_ORDER.len()];
    acc[idx("HTTP")] = 3;
    acc[idx("DNS")] = 1;
    let counts = build_type_counts(acc);
    assert_eq!(counts.len(), 3);
    assert_eq!(counts[0].label, "All");
    assert_eq!(counts[0].count, 4);
    assert_eq!(counts[1].label, "HTTP");
    assert_eq!(counts[2].label, "DNS");
}

fn prior(total: u64, up: u64, avg_ms: u32) -> PriorPeriodSummary {
    PriorPeriodSummary {
        checks_total: total,
        checks_up: up,
        avg_ms,
    }
}

#[test]
fn uptime_delta_hides_when_either_side_has_no_data() {
    assert!(uptime_delta(&prior(100, 100, 0), &prior(0, 0, 0)).is_none());
    assert!(uptime_delta(&prior(0, 0, 0), &prior(100, 100, 0)).is_none());
}

#[test]
fn uptime_delta_higher_is_up_class() {
    // 99.5% vs 98.0% → +1.50 pp better → metric-delta--up.
    let d = uptime_delta(&prior(1000, 995, 0), &prior(1000, 980, 0)).expect("delta");
    assert_eq!(d.class, "metric-delta--up");
    assert_eq!(d.arrow, "↑");
    assert_eq!(d.body, "+1.50 pp");
}

#[test]
fn uptime_delta_lower_is_down_class() {
    let d = uptime_delta(&prior(1000, 950, 0), &prior(1000, 999, 0)).expect("delta");
    assert_eq!(d.class, "metric-delta--down");
    assert_eq!(d.arrow, "↓");
    assert_eq!(d.body, "-4.90 pp");
}

#[test]
fn uptime_delta_flat_within_tolerance() {
    // 99.9994% vs 99.9990% → diff ≈ 0.0004 pp → quantizes to 0 → flat.
    let d =
        uptime_delta(&prior(1_000_000, 999_994, 0), &prior(1_000_000, 999_990, 0)).expect("delta");
    assert_eq!(d.class, "metric-delta--flat");
    assert_eq!(d.arrow, "±");
    assert_eq!(d.body, "unchanged");
}

#[test]
fn uptime_delta_quantize_threshold_matches_display() {
    // 0.005 pp rounds *up* to 0.01 → should render the non-flat chip
    // and the displayed value must equal the rounded threshold.
    let d = uptime_delta(&prior(100_000, 99_995, 0), &prior(100_000, 99_990, 0)).expect("delta");
    assert_eq!(d.class, "metric-delta--up");
    assert_eq!(d.body, "+0.01 pp");
}

#[test]
fn avg_delta_faster_is_up_class() {
    // Avg dropped (better) — green.
    let d = avg_delta(&prior(100, 100, 120), &prior(100, 100, 180)).expect("delta");
    assert_eq!(d.class, "metric-delta--up");
    assert_eq!(d.body, "-60 ms");
}

#[test]
fn avg_delta_slower_is_down_class() {
    let d = avg_delta(&prior(100, 100, 200), &prior(100, 100, 150)).expect("delta");
    assert_eq!(d.class, "metric-delta--down");
    assert_eq!(d.body, "+50 ms");
}

#[test]
fn checks_delta_neutral_direction() {
    let d = checks_delta(&prior(2_500, 2_500, 0), &prior(2_000, 2_000, 0)).expect("delta");
    assert_eq!(d.class, "metric-delta--flat");
    assert_eq!(d.arrow, "↑");
    assert!(d.body.contains("+500"), "{}", d.body);
}

#[test]
fn checks_delta_hides_when_either_side_has_no_data() {
    assert!(checks_delta(&prior(100, 100, 0), &prior(0, 0, 0)).is_none());
    assert!(checks_delta(&prior(0, 0, 0), &prior(100, 100, 0)).is_none());
}

#[test]
fn ribbon_class_partitions_by_uptime() {
    assert_eq!(ribbon_class(100.0), "op");
    assert_eq!(ribbon_class(99.9), "op");
    assert_eq!(ribbon_class(99.89), "deg");
    assert_eq!(ribbon_class(95.0), "deg");
    assert_eq!(ribbon_class(94.99), "maj");
    assert_eq!(ribbon_class(0.0), "maj");
}

fn snapped_from() -> DateTime<Utc> {
    snap_to_bucket(
        Utc::now() - Duration::hours(RIBBON_HOURS),
        RIBBON_BUCKET_SECONDS,
    )
}

#[test]
fn build_fleet_ribbon_emits_48_segs_when_empty() {
    let from = snapped_from();
    let r = build_fleet_ribbon(&[], from, &HashMap::new());
    assert_eq!(r.segs.len(), RIBBON_BUCKETS);
    assert!(r.segs.iter().all(|s| s.class == "none"));
    assert_eq!(r.uptime_label, "—");
}

#[test]
fn build_fleet_ribbon_classifies_rows_into_slots() {
    let from = snapped_from();
    let from_ts = from.timestamp();
    let bucket = RIBBON_BUCKET_SECONDS as i64;
    let rows = vec![
        FleetRibbonBucket {
            bucket_ts: from_ts,
            samples: 100,
            up: 100,
            down_targets: vec![],
        },
        FleetRibbonBucket {
            bucket_ts: from_ts + bucket,
            samples: 100,
            up: 97,
            down_targets: vec![],
        },
        FleetRibbonBucket {
            bucket_ts: from_ts + bucket * 2,
            samples: 100,
            up: 50,
            down_targets: vec![],
        },
    ];
    let r = build_fleet_ribbon(&rows, from, &HashMap::new());
    assert_eq!(r.segs[0].class, "op");
    assert_eq!(r.segs[1].class, "deg");
    assert_eq!(r.segs[2].class, "maj");
    assert_eq!(r.segs[3].class, "none");
    assert_eq!(r.segs[0].bucket_ts, from_ts);
    assert_eq!(r.segs[1].bucket_ts, from_ts + bucket);
    assert!(r.uptime_label.starts_with("82.")); // 247/300
}

#[test]
fn build_fleet_ribbon_drops_out_of_window_rows() {
    let from = snapped_from();
    let from_ts = from.timestamp();
    // Storage `WHERE minute >= from AND minute < to` should already
    // filter these, but the view layer drops them defensively so a
    // clock skew can't smear data into edge slots.
    let rows = vec![
        FleetRibbonBucket {
            bucket_ts: from_ts - 10_000,
            samples: 10,
            up: 10,
            down_targets: vec![],
        },
        FleetRibbonBucket {
            bucket_ts: from_ts + (RIBBON_HOURS * 3600),
            samples: 10,
            up: 0,
            down_targets: vec![],
        },
        FleetRibbonBucket {
            bucket_ts: from_ts + (RIBBON_HOURS * 3600) + 10_000,
            samples: 10,
            up: 0,
            down_targets: vec![],
        },
    ];
    let r = build_fleet_ribbon(&rows, from, &HashMap::new());
    assert!(r.segs.iter().all(|s| s.class == "none"));
    assert_eq!(r.uptime_label, "—");
}

#[test]
fn build_fleet_ribbon_handles_all_down_slot() {
    let from = snapped_from();
    let rows = vec![FleetRibbonBucket {
        bucket_ts: from.timestamp(),
        samples: 50,
        up: 0,
        down_targets: vec![],
    }];
    let r = build_fleet_ribbon(&rows, from, &HashMap::new());
    assert_eq!(r.segs[0].class, "maj");
    assert_eq!(r.segs[0].stat, "0.0%");
    assert_eq!(r.uptime_label, "0.00%");
}

#[test]
fn build_fleet_ribbon_sums_multiple_rows_in_same_slot() {
    // Storage emits one row per CH bucket so this shouldn't happen,
    // but the +=-into-fixed-array contract is the whole point of the
    // stack array — pin it.
    let from = snapped_from();
    let rows = vec![
        FleetRibbonBucket {
            bucket_ts: from.timestamp(),
            samples: 40,
            up: 40,
            down_targets: vec![],
        },
        FleetRibbonBucket {
            bucket_ts: from.timestamp(),
            samples: 60,
            up: 56,
            down_targets: vec![],
        },
    ];
    let r = build_fleet_ribbon(&rows, from, &HashMap::new());
    assert_eq!(r.segs[0].class, "deg"); // 96/100 → 96 % → deg
    assert_eq!(r.uptime_label, "96.00%");
}

#[test]
fn build_fleet_ribbon_previews_capped_down_names() {
    let from = snapped_from();
    let ids: Vec<Uuid> = (0..8).map(|_| Uuid::new_v4()).collect();
    let names: HashMap<Uuid, String> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, format!("svc{i}")))
        .collect();
    let rows = vec![FleetRibbonBucket {
        bucket_ts: from.timestamp(),
        samples: 100,
        up: 60,
        down_targets: ids.clone(),
    }];
    let r = build_fleet_ribbon(&rows, from, &names);
    let seg = &r.segs[0];
    assert_eq!(
        &*seg.down_targets,
        ids.as_slice(),
        "drill keeps the full set"
    );
    assert_eq!(seg.down_targets.len(), 8);
    assert_eq!(seg.down_preview.len(), DOWN_PREVIEW, "preview capped");
    assert_eq!(&seg.down_preview[0], "svc0");
    assert_eq!(&seg.down_preview[5], "svc5");
}

fn ribbon_one_down() -> FleetRibbon {
    let from = snapped_from();
    let id = Uuid::new_v4();
    let mut names = HashMap::new();
    names.insert(id, "api".to_string());
    let rows = vec![FleetRibbonBucket {
        bucket_ts: from.timestamp(),
        samples: 10,
        up: 4,
        down_targets: vec![id],
    }];
    build_fleet_ribbon(&rows, from, &names)
}

#[test]
fn ribbon_drill_cell_links_to_filter_when_inactive() {
    let ribbon = ribbon_one_down();
    let bucket = ribbon.segs[0].bucket_ts;
    let mut page = sample_page();
    page.ribbon = ribbon;
    page.drill = None;
    let html = page.render().unwrap();
    assert!(
        html.contains(&format!("down_at={bucket}")),
        "links to set the filter"
    );
    assert!(!html.contains("dashboard-ribbon__seg--active"));
}

#[test]
fn ribbon_drill_cell_toggles_off_when_active() {
    let ribbon = ribbon_one_down();
    let bucket = ribbon.segs[0].bucket_ts;
    let mut page = sample_page();
    page.ribbon = ribbon;
    page.drill = Some(DrillChip {
        down_at: bucket,
        label: "00:00".into(),
    });
    let html = page.render().unwrap();
    assert!(
        html.contains("dashboard-ribbon__seg--active"),
        "active cell is ringed"
    );
    // The cell's own link drops down_at (toggle off)...
    let active_btn = html
        .split("dashboard-ribbon__seg--active")
        .nth(1)
        .unwrap()
        .split("</button>")
        .next()
        .unwrap();
    assert!(
        !active_btn.contains("down_at"),
        "active cell links to clear, not re-set"
    );
    // ...while the poll URL keeps it so the filter survives the 5s refresh.
    assert!(
        html.contains(&format!(
            "/web/partials/dashboard?range=24h&down_at={bucket}"
        )),
        "poll persists the active filter"
    );
}

#[test]
fn snap_to_bucket_floors_to_grid() {
    let bucket = RIBBON_BUCKET_SECONDS as i64;
    let t = DateTime::<Utc>::from_timestamp(bucket * 100 + 137, 0).unwrap();
    let s = snap_to_bucket(t, RIBBON_BUCKET_SECONDS);
    assert_eq!(s.timestamp() % bucket, 0);
    assert_eq!(s.timestamp(), bucket * 100);
}

#[test]
fn dashboard_active_incident_falls_back_to_target_name_then_default() {
    let now = Utc::now();
    let make = |public_title, target_name: &str| IncidentBrief {
        id: Uuid::nil(),
        target_id: Uuid::nil(),
        target_name: target_name.into(),
        severity: IncidentSeverity::Major,
        started_at: now - Duration::minutes(5),
        ended_at: None,
        public_title,
        latest_update: None,
    };
    assert_eq!(
        DashboardActiveIncident::build(make(Some("Outage".into()), "api"), now).title,
        "Outage"
    );
    assert_eq!(
        DashboardActiveIncident::build(make(None, "api"), now).title,
        "api"
    );
    assert_eq!(
        DashboardActiveIncident::build(make(Some("  ".into()), ""), now).title,
        "Active incident"
    );
}
