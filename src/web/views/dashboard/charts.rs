//! Derived numbers for the dashboard: KPI cards and their deltas, the fleet
//! ribbon, and the sparkline paths.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::domain::metrics::{DashboardSparkBucket, FleetRibbonBucket, PriorPeriodSummary};

use super::*;

pub(super) const ACTIVE_INCIDENTS_LIMIT: usize = 5;
pub(super) const TYPE_CHIP_ORDER: &[&str] = &[
    "HTTP",
    "TCP",
    "PING",
    "HEARTBEAT",
    "DNS",
    "TLS",
    "DOMAIN",
    "FLOW",
];

pub(super) fn tally_status(counts: &mut StatusCounts, row: &DashboardRow) {
    if !row.enabled {
        counts.paused += 1;
        return;
    }
    match row.last_status {
        "up" => counts.up += 1,
        "degraded" => counts.degraded += 1,
        "down" | "error" => counts.down += 1,
        _ => {}
    }
}

pub(super) fn build_type_counts(acc: [u32; TYPE_CHIP_ORDER.len()]) -> Vec<TypeCount> {
    let total: u32 = acc.iter().sum();
    if total == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(1 + TYPE_CHIP_ORDER.len());
    out.push(TypeCount {
        label: "All",
        count: total,
        active: true,
    });
    for (idx, &label) in TYPE_CHIP_ORDER.iter().enumerate() {
        if acc[idx] > 0 {
            out.push(TypeCount {
                label,
                count: acc[idx],
                active: false,
            });
        }
    }
    out
}

/// Fleet metrics over the sparkline window — one series per KPI card, so each
/// card plots the metric it names.
#[derive(Default)]
pub(super) struct FleetSparks {
    pub(super) uptime: Vec<Option<f32>>,
    pub(super) avg_ms: Vec<Option<f32>>,
    pub(super) checks: Vec<Option<f32>>,
}

#[derive(Clone, Copy, Default)]
struct FleetSlot {
    latency_ms: f64,
    checks: u64,
    up: u64,
}

/// Every monitor's rollup, not just the rows the table shows — the KPI values
/// beside these lines are org-wide too.
pub(super) fn fleet_sparks(rows: &[DashboardSparkBucket], from: DateTime<Utc>) -> FleetSparks {
    let from_ts = from.timestamp();
    let mut agg = [FleetSlot::default(); SPARK_BUCKETS];
    for r in rows {
        if !r.avg_ms.is_finite() {
            continue;
        }
        let slot = ((r.bucket_ts - from_ts) / 60).clamp(0, SPARK_BUCKETS as i64 - 1) as usize;
        // Weighted by check count: a monitor probed once in a minute must not
        // outvote one probed sixty times.
        agg[slot].latency_ms += f64::from(r.avg_ms) * r.checks as f64;
        agg[slot].checks += r.checks;
        agg[slot].up += r.up;
    }
    let series = |f: fn(&FleetSlot) -> f32| -> Vec<Option<f32>> {
        agg.iter().map(|s| (s.checks > 0).then(|| f(s))).collect()
    };
    FleetSparks {
        uptime: series(|s| (s.up as f64 / s.checks as f64 * 100.0) as f32),
        avg_ms: series(|s| (s.latency_ms / s.checks as f64) as f32),
        checks: series(|s| s.checks as f32),
    }
}

pub(super) fn build_kpi_cards(
    kpis: &DashboardKpis,
    range: &'static str,
    checks_successful_label: String,
    current: &PriorPeriodSummary,
    prior: &PriorPeriodSummary,
    sparks: &FleetSparks,
) -> Vec<KpiCardSpec> {
    let incidents_html = format!(
        r#"Incidents · {range}: <span class="{cls}">{n}</span>"#,
        cls = if kpis.incidents > 0 {
            "metric-alert"
        } else {
            "metric-quiet"
        },
        n = kpis.incidents,
    );
    let spark = |series: &[Option<f32>]| {
        let (path, fill, _) = render_spark_path(series);
        (path, fill)
    };
    // Percentages get the full 0..100 scale — autoscaled, a 99.99 → 100 wiggle
    // would draw the same cliff as an outage.
    let (uptime_path, uptime_fill, _) = render_spark_path_domain(&sparks.uptime, 0.0, 100.0);
    let (avg_path, avg_fill) = spark(&sparks.avg_ms);
    let (checks_path, checks_fill) = spark(&sparks.checks);
    vec![
        KpiCardSpec {
            label: format!("Uptime · {range}"),
            value: kpis.uptime_pct_label.clone(),
            hint_html: incidents_html,
            delta: uptime_delta(current, prior),
            spark_tint: "ok",
            spark_path: uptime_path,
            spark_fill: uptime_fill,
        },
        KpiCardSpec {
            label: format!("Avg response · {range}"),
            value: kpis.avg_response_ms_label.clone(),
            hint_html: "across all monitors".into(),
            delta: avg_delta(current, prior),
            spark_tint: "ink",
            spark_path: avg_path,
            spark_fill: avg_fill,
        },
        KpiCardSpec {
            label: format!("Checks · {range}"),
            value: kpis.checks_label.clone(),
            hint_html: checks_successful_label,
            delta: checks_delta(current, prior),
            spark_tint: "info",
            spark_path: checks_path,
            spark_fill: checks_fill,
        },
    ]
}

/// Polarity of a metric — whether a higher value is good, bad, or
/// just informational. Drives the `metric-delta--{up,down,flat}` class
/// (a green ↑ on uptime is the same class as a green ↓ on latency).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Polarity {
    /// Bigger is better — uptime.
    HigherIsBetter,
    /// Smaller is better — response time.
    LowerIsBetter,
    /// Direction is informational only — checks count.
    Neutral,
}

/// Standard chip for a non-zero Δ. `signed` carries the sign for the
/// arrow; polarity decides which colour class wins.
fn signed_delta(polarity: Polarity, signed: f64, body: String) -> KpiDelta {
    let arrow = if signed > 0.0 { "↑" } else { "↓" };
    let class = match polarity {
        Polarity::HigherIsBetter if signed > 0.0 => "metric-delta--up",
        Polarity::HigherIsBetter => "metric-delta--down",
        Polarity::LowerIsBetter if signed < 0.0 => "metric-delta--up",
        Polarity::LowerIsBetter => "metric-delta--down",
        Polarity::Neutral => "metric-delta--flat",
    };
    KpiDelta { class, arrow, body }
}

fn flat_delta() -> KpiDelta {
    KpiDelta {
        class: "metric-delta--flat",
        arrow: "±",
        body: "unchanged".into(),
    }
}

/// Δ chip for a percentage-point change; collapses to "unchanged" below
/// display precision so the chip never contradicts the shown value.
pub(crate) fn uptime_pp_delta(cur_pct: f64, prior_pct: f64) -> KpiDelta {
    let diff = ((cur_pct - prior_pct) * 100.0).round() / 100.0;
    if diff.abs() < 0.01 {
        return flat_delta();
    }
    signed_delta(Polarity::HigherIsBetter, diff, format!("{diff:+.2} pp"))
}

/// Δ chip for a raw count change (up / down / errors).
pub(crate) fn count_delta(cur: u64, prior: u64, polarity: Polarity) -> KpiDelta {
    let diff = cur as i64 - prior as i64;
    if diff == 0 {
        return flat_delta();
    }
    signed_delta(polarity, diff as f64, format!("{diff:+}"))
}

fn uptime_pct(s: &PriorPeriodSummary) -> Option<f64> {
    if s.checks_total == 0 {
        None
    } else {
        Some((s.checks_up as f64 / s.checks_total as f64) * 100.0)
    }
}

pub(super) fn uptime_delta(
    cur: &PriorPeriodSummary,
    prior: &PriorPeriodSummary,
) -> Option<KpiDelta> {
    Some(uptime_pp_delta(uptime_pct(cur)?, uptime_pct(prior)?))
}

pub(super) fn avg_delta(cur: &PriorPeriodSummary, prior: &PriorPeriodSummary) -> Option<KpiDelta> {
    if cur.checks_total == 0 || prior.checks_total == 0 {
        return None;
    }
    let diff = cur.avg_ms as i64 - prior.avg_ms as i64;
    if diff == 0 {
        return Some(flat_delta());
    }
    Some(signed_delta(
        Polarity::LowerIsBetter,
        diff as f64,
        format!("{diff:+} ms"),
    ))
}

pub(super) fn checks_delta(
    cur: &PriorPeriodSummary,
    prior: &PriorPeriodSummary,
) -> Option<KpiDelta> {
    if cur.checks_total == 0 || prior.checks_total == 0 {
        return None;
    }
    let diff = cur.checks_total as i64 - prior.checks_total as i64;
    if diff == 0 {
        return Some(flat_delta());
    }
    let body = if diff >= 0 {
        format!("+{}", format_count(diff as u64))
    } else {
        format!("-{}", format_count(diff.unsigned_abs()))
    };
    Some(signed_delta(Polarity::Neutral, diff as f64, body))
}

/// Map CH ribbon rows → fixed-length 48-seg view. Buckets the storage
/// layer omitted (no samples) become `none`. Aggregate uptime label is
/// computed from the same sample totals so the displayed % matches the
/// segs the operator sees. `from` must already be bucket-aligned (see
/// `snap_to_bucket`) so labels line up with the CH `toStartOfInterval`
/// grid.
pub(super) fn build_fleet_ribbon(
    rows: &[FleetRibbonBucket],
    from: DateTime<Utc>,
    names: &HashMap<Uuid, String>,
) -> FleetRibbon {
    let from_ts = from.timestamp();
    let bucket = RIBBON_BUCKET_SECONDS as i64;
    let mut filled: [(u64, u64); RIBBON_BUCKETS] = [(0, 0); RIBBON_BUCKETS];
    let mut down_by_slot: [Vec<Uuid>; RIBBON_BUCKETS] = std::array::from_fn(|_| Vec::new());
    let mut total_samples: u64 = 0;
    let mut total_up: u64 = 0;
    for r in rows {
        let offset = r.bucket_ts - from_ts;
        if offset < 0 {
            continue;
        }
        let slot = offset / bucket;
        if slot >= RIBBON_BUCKETS as i64 {
            continue;
        }
        let slot = slot as usize;
        filled[slot].0 += r.samples;
        filled[slot].1 += r.up;
        down_by_slot[slot].extend_from_slice(&r.down_targets);
        total_samples += r.samples;
        total_up += r.up;
    }
    let mut segs: Vec<FleetRibbonSeg> = Vec::with_capacity(RIBBON_BUCKETS);
    for (i, (samples, up)) in filled.iter().enumerate() {
        let slot_start = from + Duration::seconds(i as i64 * bucket);
        let down = std::mem::take(&mut down_by_slot[i]);
        let (class, stat) = if *samples == 0 {
            ("none", "no data".to_string())
        } else {
            let pct = (*up as f64 / *samples as f64) * 100.0;
            (ribbon_class(pct), format!("{pct:.1}%"))
        };
        let down_preview: Vec<String> = down
            .iter()
            .take(DOWN_PREVIEW)
            .map(|id| names.get(id).cloned().unwrap_or_else(|| "unknown".into()))
            .collect();
        segs.push(FleetRibbonSeg {
            class,
            time: slot_start.format("%H:%M").to_string(),
            stat,
            down_preview: Arc::from(down_preview.into_boxed_slice()),
            bucket_ts: from_ts + i as i64 * bucket,
            down_targets: Arc::from(down.into_boxed_slice()),
        });
    }
    FleetRibbon {
        segs: Arc::from(segs.into_boxed_slice()),
        uptime_label: pct_label(total_samples, total_up),
    }
}

/// Round `t` down to the nearest `bucket_seconds` boundary so the
/// returned `from` matches a CH `toStartOfInterval(_, INTERVAL bucket
/// SECOND)` grid line.
pub(super) fn snap_to_bucket(t: DateTime<Utc>, bucket_seconds: u32) -> DateTime<Utc> {
    let b = bucket_seconds.max(1) as i64;
    let snapped = (t.timestamp().div_euclid(b)) * b;
    DateTime::<Utc>::from_timestamp(snapped, 0).unwrap_or(t)
}

pub(crate) fn ribbon_class(pct: f64) -> &'static str {
    if pct >= 99.9 {
        "op"
    } else if pct >= 95.0 {
        "deg"
    } else {
        "maj"
    }
}

pub(super) fn range_span(key: &'static str) -> Duration {
    match key {
        "7d" => Duration::days(7),
        "30d" => Duration::days(30),
        "90d" => Duration::days(90),
        _ => Duration::hours(24),
    }
}

pub(crate) fn pct_label(total: u64, up: u64) -> String {
    match uptime_pct(&PriorPeriodSummary {
        checks_total: total,
        checks_up: up,
        avg_ms: 0,
    }) {
        Some(p) => format!("{p:.2}%"),
        None => "—".into(),
    }
}

pub(super) fn avg_response_label(avg_ms: u32, samples: u64) -> String {
    if samples == 0 {
        return "—".into();
    }
    format!("{avg_ms} ms")
}

/// Compact count: "17.3k" / "1.2M" / "42". Matches the V3 KPI tile so
/// the strip stays single-line at any traffic level.
pub(super) fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Bin sparkline rows into a fixed-length `Vec<Option<f32>>` per target,
/// oldest → newest. Missing minutes stay `None` so the SVG can break
/// the polyline (no fake "drop to zero" between gaps).
pub(super) fn group_sparks(
    rows: &[DashboardSparkBucket],
    from: DateTime<Utc>,
) -> HashMap<Uuid, Vec<Option<f32>>> {
    let from_ts = from.timestamp();
    let mut out: HashMap<Uuid, Vec<Option<f32>>> = HashMap::new();
    for r in rows {
        let slot = ((r.bucket_ts - from_ts) / 60).clamp(0, SPARK_BUCKETS as i64 - 1) as usize;
        let buckets = out
            .entry(r.target_id)
            .or_insert_with(|| vec![None; SPARK_BUCKETS]);
        buckets[slot] = Some(r.avg_ms);
    }
    out
}

/// Per-row sparkline into a `160×22` viewport. Returns `(line, fill, baseline_y)`.
///
/// Line connects across `None` gaps so a monitor sampled slower than the 1-min
/// bucket still renders. x spans the full window — data shorter than the window
/// fills the recent end instead of being stretched across it; y auto-scales to
/// the row's own min/max. A lone sample is a zero-length segment — a dot once
/// the path's round linecap (CSS) renders it. Fill is the line closed to the
/// baseline, empty below two samples. `baseline_y` carries the dashed no-data rule.
pub(crate) fn render_spark_path(spark: &[Option<f32>]) -> (String, String, u32) {
    // Treat NaN/Inf as missing — CH `avgMerge` returns NaN for empty
    // groups and a single non-finite poisons min/max, producing
    // `"M NaN NaN"` paths that browsers silently drop. The empty case (min/max
    // stay sentinels, unused) is handled inside the domain renderer.
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for v in spark.iter().filter_map(|o| o.filter(|v| v.is_finite())) {
        min = min.min(v);
        max = max.max(v);
    }
    render_spark_path_domain(spark, min, max)
}

/// Like [`render_spark_path`] but maps values onto a fixed `[min, max]`
/// domain instead of the series' own range — an availability line then sits
/// flat at the top when healthy and only notches down on real dips.
pub(crate) fn render_spark_path_domain(
    spark: &[Option<f32>],
    min: f32,
    max: f32,
) -> (String, String, u32) {
    const W: f32 = 160.0;
    const H: f32 = 22.0;
    let baseline_y = (H / 2.0).round() as u32;
    let finite = |o: &Option<f32>| o.filter(|v| v.is_finite());
    let present: Vec<f32> = spark.iter().filter_map(finite).collect();
    if present.is_empty() {
        return (String::new(), String::new(), baseline_y);
    }
    let span = (max - min).max(1.0);

    let step = W / (spark.len().saturating_sub(1).max(1)) as f32;

    let points: Vec<(f32, f32)> = spark
        .iter()
        .enumerate()
        .filter_map(|(i, slot)| finite(slot).map(|v| (i, v)))
        .map(|(i, v)| {
            let x = step * i as f32;
            let y = (1.0 - (v - min) / span) * H;
            (x, y)
        })
        .collect();

    // Build the body once; fill derives from it so the two cannot drift apart.
    let mut line = String::with_capacity(points.len() * 14);
    let (fx, fy) = points[0];
    write!(line, "M{fx:.1} {fy:.1}").unwrap();
    for &(x, y) in &points[1..] {
        write!(line, " L{x:.1} {y:.1}").unwrap();
    }

    let fill = if points.len() >= 2 {
        let (lx, _) = *points.last().unwrap();
        let mut fill = String::with_capacity(line.len() + 32);
        fill.push_str(&line);
        write!(fill, " L{lx:.1} {H:.1} L{fx:.1} {H:.1} Z").unwrap();
        fill
    } else {
        // zero-length segment → dot under the round linecap
        write!(line, " L{fx:.1} {fy:.1}").unwrap();
        String::new()
    };

    (line, fill, baseline_y)
}
