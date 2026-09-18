//! Free standalone developer tools served from the apex host. Each is a
//! self-contained, no-DB page that ranks for an adjacent query and routes
//! the visitor into the product. Same cached-render contract as the rest
//! of marketing: one render at boot, ETag + Cache-Control on every hit.

pub mod domain_expiry;
pub mod http_headers;
mod probe;
pub mod security;
pub mod ssl;

use std::sync::Arc;
use std::sync::OnceLock;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::response::Response;

use crate::marketing::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_faqpage, json_ld_item_list_links,
    json_ld_web_application, json_ld_webpage,
};
use crate::web::filters;

use super::config::{BRAND, MarketingCfg};
use super::pages::{CachedRender, cached_render, serve_cached};

const TOOL_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=300, stale-while-revalidate=86400");

pub const UPTIME_SLA_PATH: &str = "/tools/uptime-sla-calculator";
const UPTIME_SLA_CREATED: &str = "2026-07-09";
const UPTIME_SLA_LASTMOD: &str = "2026-09-04";
pub const UPTIME_SLA_TITLE: &str = "Uptime SLA & Downtime Calculator";
const UPTIME_SLA_LABEL: &str = "uptime SLA calculator";
pub const UPTIME_SLA_DESCRIPTION: &str = "Turn an uptime percentage into allowed downtime per day, week, month and year. A free SLA and SLO calculator with the full nines reference table.";

const DAY: f64 = 86_400.0;
const WEEK: f64 = 604_800.0;
const MONTH: f64 = 2_592_000.0; // 30 days
const YEAR: f64 = 31_536_000.0; // 365 days
const CHECK_INTERVAL: f64 = 60.0;

/// Uptime targets shown in the server-rendered reference table.
const REFERENCE_NINES: &[f64] = &[90.0, 95.0, 98.0, 99.0, 99.5, 99.9, 99.95, 99.99, 99.999];
/// One-click targets on the interactive widget.
const PRESETS: &[f64] = &[99.0, 99.5, 99.9, 99.95, 99.99, 99.999];
/// The state the page renders with before any JS runs.
const DEFAULT_UPTIME: f64 = 99.9;

const UPTIME_SLA_FAQS: &[(&str, &str)] = &[
    (
        "How do you calculate allowed downtime from an uptime percentage?",
        "Multiply the length of the period by one minus the uptime fraction. \
         At 99.9%, a 30-day month allows 30 days times 0.001, which is 43 minutes \
         and 12 seconds of downtime.",
    ),
    (
        "How do I calculate an uptime percentage?",
        "Subtract the downtime share from 100%. If a service was down 43 minutes \
         in a 30-day month, uptime is (2,592,000 seconds minus 2,580) divided by \
         2,592,000, which is about 99.9%. The reverse calculator above turns a \
         downtime figure straight into the percentage.",
    ),
    (
        "What is the difference between an SLA and an SLO?",
        "An SLO is the reliability target you set internally, such as 99.9% uptime. \
         An SLA is the contract you sign with a customer, usually a looser number \
         with a refund if you miss it. Both use the same downtime math.",
    ),
    (
        "How much downtime does 99.9% allow?",
        "Three nines allows about 8 hours 45 minutes a year, 43 minutes a month, \
         or 1 minute 26 seconds a day. 99.99% cuts that to roughly 52 minutes a year.",
    ),
    (
        "What uptime should I aim for?",
        "Most SaaS products commit to 99.9% and run internally toward 99.95%. Chasing \
         a fifth nine is expensive and rarely worth it unless a payment or safety flow \
         depends on it. Pick a number you can actually measure and alert on.",
    ),
    (
        "How is the month and year length defined here?",
        "A month is 30 days and a year is 365 days, the convention most SLA tables use. \
         Day and week are exact. Change the percentage and every column recomputes.",
    ),
];

/// Numbers come from the same math as the table so the copy never drifts.
const SLA_LEVELS: &[(f64, &str, Option<&str>)] = &[
    (
        98.0,
        "Fine for an internal tool or a staging box. In a contract it reads as \
         more than fourteen hours of monthly downtime, which is a hard sell.",
        Some("/blog/is-98-uptime-good"),
    ),
    (
        99.0,
        "Two nines. Almost a full working day of downtime every month, and \
         most of it tends to arrive in business hours.",
        None,
    ),
    (
        99.5,
        "Where single-server setups without failover usually land. A weekly \
         maintenance window still fits, barely.",
        None,
    ),
    (
        99.9,
        "Three nines, the most common SaaS commitment. One bad deploy a month \
         can eat the whole budget.",
        Some("/blog/how-much-downtime-is-99-9-uptime"),
    ),
    (
        99.95,
        "The usual internal target behind a 99.9% contract. It leaves room for \
         one short incident a month, not two.",
        Some("/blog/how-much-downtime-is-99-95-uptime"),
    ),
    (
        99.99,
        "Four nines. No human pages fast enough for this; recovery has to be \
         automatic before anyone reads the alert.",
        Some("/blog/how-much-downtime-is-99-99-uptime"),
    ),
    (
        99.999,
        "Five nines is telecom territory. Five minutes a year needs redundant \
         everything, and few web products earn back what that costs.",
        None,
    ),
];

pub struct SlaLevelSection {
    pub anchor: String,
    pub heading: String,
    pub lede: String,
    pub note: &'static str,
    pub guide: Option<GuideLink>,
}

pub struct GuideLink {
    pub href: &'static str,
    pub label: String,
}

/// One reference-table row: an uptime target and its allowed downtime per period.
pub struct SlaRow {
    pub label: String,
    pub daily: String,
    pub weekly: String,
    pub monthly: String,
    pub yearly: String,
}

/// The interactive widget's default state, rendered server-side so the page
/// shows real numbers with JS disabled.
pub struct SlaResult {
    pub daily: String,
    pub weekly: String,
    pub monthly: String,
    pub yearly: String,
    pub checks_monthly: String,
}

/// Reverse widget default: a downtime figure resolved back to an uptime %.
const REVERSE_VALUE: &str = "1";
const REVERSE_UNIT_SECS: f64 = 3_600.0; // hour
const REVERSE_PERIOD_SECS: f64 = MONTH;

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_uptime_sla.html")]
struct UptimeSlaPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    rows: Vec<SlaRow>,
    levels: Vec<SlaLevelSection>,
    default: SlaResult,
    default_uptime: String,
    presets: &'static [f64],
    reverse_value: &'static str,
    reverse_uptime: String,
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

/// Allowed downtime for an uptime percentage over a period, in seconds.
fn downtime(uptime_pct: f64, period_secs: f64) -> f64 {
    period_secs * (1.0 - uptime_pct / 100.0)
}

/// Human-readable duration. Sub-second falls to `ms`, under ten seconds keeps
/// one decimal, larger values break into `d/h/m/s`. Kept in lockstep with the
/// JavaScript port so the live widget never disagrees with the server render.
fn human_duration(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "0s".to_string();
    }
    if secs < 1.0 {
        return format!("{}ms", (secs * 1000.0).round());
    }
    if secs < 10.0 {
        let v = (secs * 10.0).round() / 10.0;
        if v.fract() == 0.0 {
            return format!("{v}s");
        }
        return format!("{v:.1}s");
    }
    let mut total = secs.round() as u64;
    let d = total / 86_400;
    total %= 86_400;
    let h = total / 3_600;
    total %= 3_600;
    let m = total / 60;
    let s = total % 60;
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d}d"));
    }
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

fn sla_result(uptime: f64) -> SlaResult {
    SlaResult {
        daily: human_duration(downtime(uptime, DAY)),
        weekly: human_duration(downtime(uptime, WEEK)),
        monthly: human_duration(downtime(uptime, MONTH)),
        yearly: human_duration(downtime(uptime, YEAR)),
        checks_monthly: format!(
            "{}",
            (downtime(uptime, MONTH) / CHECK_INTERVAL).round() as u64
        ),
    }
}

/// Uptime percentage for a downtime figure over a period. Inverse of
/// [`downtime`]; clamped at zero so an over-budget entry reads 0%.
fn uptime_for(downtime_secs: f64, period_secs: f64) -> f64 {
    (1.0 - downtime_secs / period_secs).max(0.0) * 100.0
}

/// Format a percentage with up to four decimals, trailing zeros trimmed.
fn format_pct(pct: f64) -> String {
    let s = format!("{pct:.4}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}%")
}

fn reference_rows() -> Vec<SlaRow> {
    REFERENCE_NINES
        .iter()
        .map(|&pct| SlaRow {
            label: format!("{pct}%"),
            daily: human_duration(downtime(pct, DAY)),
            weekly: human_duration(downtime(pct, WEEK)),
            monthly: human_duration(downtime(pct, MONTH)),
            yearly: human_duration(downtime(pct, YEAR)),
        })
        .collect()
}

fn sla_level_sections() -> Vec<SlaLevelSection> {
    SLA_LEVELS
        .iter()
        .map(|&(pct, note, guide)| {
            let d = sla_result(pct);
            SlaLevelSection {
                anchor: format!("sla-{pct}").replace('.', "-"),
                heading: format!("Downtime allowed at {pct}%"),
                lede: format!(
                    "A {pct}% SLA allows {} of downtime per day, {} per week, \
                     {} per month and {} per year.",
                    d.daily, d.weekly, d.monthly, d.yearly,
                ),
                note,
                guide: guide.map(|href| GuideLink {
                    href,
                    label: format!("The full guide to {pct}% uptime"),
                }),
            }
        })
        .collect()
}

static UPTIME_SLA_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_uptime_sla(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, UPTIME_SLA_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{UPTIME_SLA_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = UPTIME_SLA_DESCRIPTION.to_string();
    let page = UptimeSlaPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            UPTIME_SLA_TITLE,
            UPTIME_SLA_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            UPTIME_SLA_TITLE,
            UPTIME_SLA_PATH,
            UPTIME_SLA_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            UPTIME_SLA_PATH,
            UPTIME_SLA_TITLE,
            UPTIME_SLA_CREATED,
            UPTIME_SLA_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(UPTIME_SLA_FAQS),
        faqs: UPTIME_SLA_FAQS,
        rows: reference_rows(),
        levels: sla_level_sections(),
        default: sla_result(DEFAULT_UPTIME),
        default_uptime: format!("{DEFAULT_UPTIME}"),
        presets: PRESETS,
        reverse_value: REVERSE_VALUE,
        reverse_uptime: format_pct(uptime_for(REVERSE_UNIT_SECS, REVERSE_PERIOD_SECS)),
        canonical_url,
        og,
        tools: TOOLS,
        self_path: UPTIME_SLA_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- uptime-sla render failed: {e} -->"));
    cached_render(body)
}

async fn uptime_sla(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = UPTIME_SLA_CACHED.get_or_init(|| render_uptime_sla(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

// ── Cron expression generator ─────────────────────────────────────────
// Client-side only (cron.js). The server default's authored description
// must stay identical to the JS output, or it flickers on hydration.

pub const CRON_PATH: &str = "/tools/cron-expression-generator";
const CRON_CREATED: &str = "2026-07-09";
const CRON_LASTMOD: &str = "2026-09-04";
pub const CRON_TITLE: &str = "Cron Expression Generator & Parser";
const CRON_LABEL: &str = "cron expression generator";
pub const CRON_DESCRIPTION: &str = "Build and read cron expressions in plain English, with the next run times and a reference table of the most common schedules. Free, no sign-up.";
const CRON_DEFAULT_EXPR: &str = "*/15 9-17 * * 1-5";
const CRON_DEFAULT_DESC: &str =
    "At every 15th minute past every hour from 9 through 17, on Monday through Friday.";

/// Common schedules for the reference table (expression, plain-English).
const CRON_EXAMPLES: &[(&str, &str)] = &[
    ("* * * * *", "Every minute"),
    ("*/5 * * * *", "Every 5 minutes"),
    ("0 * * * *", "Every hour, on the hour"),
    (
        "*/15 9-17 * * 1-5",
        "Every 15 minutes, 9am to 5pm, on weekdays",
    ),
    ("0 0 * * *", "Every day at midnight"),
    ("30 2 * * *", "Every day at 02:30"),
    ("0 9 * * 1-5", "At 09:00, Monday to Friday"),
    ("0 0 * * 0", "Every Sunday at midnight"),
    ("0 0 1 * *", "Midnight on the 1st of each month"),
    ("0 0 1 1 *", "Midnight on the 1st of January"),
];

/// One-click schedules on the interactive widget (expression, label).
const CRON_PRESETS: &[(&str, &str)] = &[
    ("*/5 * * * *", "every 5 min"),
    ("0 * * * *", "hourly"),
    ("0 0 * * *", "daily"),
    ("0 9 * * 1-5", "weekdays 9am"),
    ("0 0 * * 0", "weekly"),
    ("0 0 1 * *", "monthly"),
];

const CRON_FAQS: &[(&str, &str)] = &[
    (
        "What is a cron expression?",
        "Five fields that tell a scheduler when to run a job: minute, hour, \
         day of the month, month, and day of the week. <code class=\"mk-chip\" \
         translate=\"no\">0 9 * * 1-5</code> means 9am on weekdays.",
    ),
    (
        "What does the slash mean, like */15?",
        "A step. <code class=\"mk-chip\" translate=\"no\">*/15</code> in the minute \
         field means every 15th minute (0, 15, 30, 45). <code class=\"mk-chip\" \
         translate=\"no\">*/2</code> in the hour field means every other hour.",
    ),
    (
        "Are the next run times UTC or local?",
        "The times shown here are in your browser's timezone. On a server, cron \
         uses that machine's timezone, which is usually UTC. Check the box's \
         timezone before you trust a schedule in production.",
    ),
    (
        "What happens when both day fields are set?",
        "If you restrict both the day of the month and the day of the week, cron \
         runs when EITHER matches, not both. So <code class=\"mk-chip\" \
         translate=\"no\">0 0 1 * 1</code> fires on the 1st and every Monday.",
    ),
    (
        "Do shortcuts like @daily work?",
        "Yes. The parser accepts @hourly, @daily, @weekly, @monthly and @yearly \
         and expands them to the equivalent five-field expression.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_cron.html")]
struct CronPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    examples: &'static [(&'static str, &'static str)],
    presets: &'static [(&'static str, &'static str)],
    default_expr: &'static str,
    default_desc: &'static str,
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

static CRON_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_cron(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, CRON_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{CRON_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = CRON_DESCRIPTION.to_string();
    let page = CronPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(&cfg.canonical_origin, CRON_TITLE, CRON_PATH),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            CRON_TITLE,
            CRON_PATH,
            CRON_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            CRON_PATH,
            CRON_TITLE,
            CRON_CREATED,
            CRON_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(CRON_FAQS),
        faqs: CRON_FAQS,
        examples: CRON_EXAMPLES,
        presets: CRON_PRESETS,
        default_expr: CRON_DEFAULT_EXPR,
        default_desc: CRON_DEFAULT_DESC,
        canonical_url,
        og,
        tools: TOOLS,
        self_path: CRON_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- cron render failed: {e} -->"));
    cached_render(body)
}

// ── Error budget & burn rate calculator ───────────────────────────────
// Client-side only (error_budget.js). The server default must equal the JS
// output, or the numbers flicker on hydration.

const QUARTER: f64 = 7_776_000.0; // 90 days

pub const ERROR_BUDGET_PATH: &str = "/tools/error-budget-calculator";
const ERROR_BUDGET_CREATED: &str = "2026-07-13";
const ERROR_BUDGET_LASTMOD: &str = "2026-09-04";
pub const ERROR_BUDGET_TITLE: &str = "Error Budget & Burn Rate Calculator";
const ERROR_BUDGET_LABEL: &str = "error budget calculator";
pub const ERROR_BUDGET_DESCRIPTION: &str = "Turn an SLO target and your measured availability into error budget spent, budget left and burn rate, with a burn-rate reference table. Free, no sign-up.";

/// One-click SLO targets on the interactive widget.
const ERROR_BUDGET_SLOS: &[f64] = &[99.0, 99.5, 99.9, 99.95, 99.99];
/// Window options (label, seconds). Month is 30 days, quarter 90, year 365.
const ERROR_BUDGET_WINDOWS: &[(&str, f64)] = &[
    ("week", WEEK),
    ("month", MONTH),
    ("quarter", QUARTER),
    ("year", YEAR),
];
/// Burn-rate rows for the reference table. Each maps a sustained rate to the
/// share of a 30-day budget spent per hour and how long the budget lasts.
const BURN_RATES: &[f64] = &[1.0, 2.0, 3.0, 6.0, 10.0, 14.4];

const DEFAULT_SLO: f64 = 99.9;
const DEFAULT_MEASURED: f64 = 99.95;
const DEFAULT_WINDOW: f64 = MONTH;
const DEFAULT_WINDOW_LABEL: &str = "month";

const ERROR_BUDGET_FAQS: &[(&str, &str)] = &[
    (
        "What is an error budget?",
        "The amount of unreliability a service is allowed before it misses its \
         SLO. It is the length of the window times one minus the SLO. A 99.9% SLO \
         over a 30-day month gives 43 minutes 12 seconds of budget.",
    ),
    (
        "What is a burn rate?",
        "How fast you are spending the budget, relative to the pace that would \
         use it up exactly over the window. A burn rate of 1x is on budget, 2x \
         spends it in half the window, and anything under 1x means you finish \
         with budget to spare.",
    ),
    (
        "How do you calculate burn rate?",
        "Divide your actual failure rate by the allowed one: (100% minus measured \
         availability) divided by (100% minus the SLO). At a 99.9% SLO, measured \
         99.8% availability is a 2x burn rate.",
    ),
    (
        "What is the difference between an SLO, an SLA and an SLI?",
        "An SLI is the measurement, such as the share of successful requests. An \
         SLO is the internal target for that measurement, like 99.9%. An SLA is the \
         contract with a customer, usually looser, with a penalty if you miss it. \
         The error budget is built from the SLO.",
    ),
    (
        "What window should I measure over?",
        "A rolling 30-day window is the common default for alerting. Quarterly and \
         yearly windows suit planning and reliability reviews. Change the window \
         above and the budget recomputes.",
    ),
    (
        "What burn rate should trigger an alert?",
        "Fast-burn alerts commonly fire around 14.4x, which spends 2% of a 30-day \
         budget in an hour. Slower multi-window alerts catch a sustained 3x to 6x \
         that would quietly exhaust the budget over days.",
    ),
];

/// Interactive-widget result, rendered server-side so the page shows real
/// numbers before any JavaScript runs.
pub struct BudgetResult {
    pub total: String,
    pub used: String,
    pub remaining: String,
    pub burn: String,
    pub verdict: String,
    pub verdict_state: &'static str,
}

/// One burn-rate reference row: a sustained rate, the share of a 30-day budget
/// it spends per hour, and how long that budget lasts at the rate.
pub struct BurnRow {
    pub rate: String,
    pub per_hour: String,
    pub lasts: String,
}

/// Burn rate: actual failure rate over allowed failure rate. A perfect SLO
/// (100%) has no budget, so any downtime burns infinitely fast; report that as
/// zero rather than dividing by zero.
fn burn_rate(measured_pct: f64, slo_pct: f64) -> f64 {
    let allowed = 1.0 - slo_pct / 100.0;
    if allowed <= 0.0 {
        return if measured_pct >= 100.0 {
            0.0
        } else {
            f64::INFINITY
        };
    }
    ((1.0 - measured_pct / 100.0) / allowed).max(0.0)
}

/// Format a burn-rate multiple with up to two decimals, trailing zeros trimmed.
fn format_multiple(rate: f64) -> String {
    if !rate.is_finite() {
        return "∞×".to_string();
    }
    let s = format!("{rate:.2}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed}×")
}

fn budget_result(slo: f64, measured: f64, window_secs: f64) -> BudgetResult {
    let total = downtime(slo, window_secs);
    let used = downtime(measured, window_secs);
    let remaining = (total - used).max(0.0);
    let rate = burn_rate(measured, slo);
    let (verdict, verdict_state) = if measured >= 100.0 {
        (
            "on track, no downtime and full budget intact".to_string(),
            "ok",
        )
    } else if !rate.is_finite() {
        (
            "over budget, a 100% SLO leaves no room for any downtime".to_string(),
            "warn",
        )
    } else if rate < 1.0 {
        (
            format!(
                "under budget, spending at {} the allowed rate",
                format_multiple(rate)
            ),
            "ok",
        )
    } else if (rate - 1.0).abs() < 0.001 {
        (
            "on budget, spends the window's allowance exactly".to_string(),
            "on",
        )
    } else {
        (
            format!(
                "over budget, the allowance runs out in {}",
                human_duration(window_secs / rate)
            ),
            "warn",
        )
    };
    BudgetResult {
        total: human_duration(total),
        used: human_duration(used),
        remaining: human_duration(remaining),
        burn: format_multiple(rate),
        verdict,
        verdict_state,
    }
}

fn burn_rows() -> Vec<BurnRow> {
    BURN_RATES
        .iter()
        .map(|&rate| BurnRow {
            rate: format_multiple(rate),
            per_hour: format_pct(rate * 3_600.0 / MONTH * 100.0),
            lasts: human_duration(MONTH / rate),
        })
        .collect()
}

// Burn-down chart geometry. The SVG viewBox and plot rectangle are shared,
// byte-for-byte, with error_budget.js so the server render and the first
// client redraw land on the same pixels.
const CHART_X0: f64 = 40.0;
const CHART_X1: f64 = 568.0;
const CHART_Y0: f64 = 16.0;
const CHART_Y1: f64 = 190.0;

fn chart_x(t: f64) -> f64 {
    CHART_X0 + t * (CHART_X1 - CHART_X0)
}

fn chart_y(frac: f64) -> f64 {
    CHART_Y0 + (1.0 - frac) * (CHART_Y1 - CHART_Y0)
}

/// The burn-down line, its end marker and its one direct label, all as ready
/// SVG coordinate strings. Budget remaining falls from 100% at a slope equal
/// to the burn rate, so a 1x line lands on zero exactly at the window's end
/// and anything steeper crosses zero early.
pub struct BurnChart {
    pub line_points: String,
    pub dot_x: String,
    pub dot_y: String,
    pub label_x: String,
    pub label_y: String,
    pub label_anchor: &'static str,
    pub label_text: String,
    pub state: &'static str,
}

fn point(x: f64, y: f64) -> String {
    format!("{x:.1},{y:.1}")
}

fn burn_chart(slo: f64, measured: f64, window_secs: f64) -> BurnChart {
    let rate = burn_rate(measured, slo);
    let over = !rate.is_finite() || rate > 1.0;
    if over {
        let t_star = if rate.is_finite() { 1.0 / rate } else { 0.0 };
        let cross_x = chart_x(t_star);
        let label_text = if rate.is_finite() {
            format!("gone in {}", human_duration(window_secs / rate))
        } else {
            "no budget".to_string()
        };
        BurnChart {
            line_points: format!(
                "{} {} {}",
                point(chart_x(0.0), chart_y(1.0)),
                point(cross_x, chart_y(0.0)),
                point(chart_x(1.0), chart_y(0.0)),
            ),
            dot_x: format!("{cross_x:.1}"),
            dot_y: format!("{:.1}", chart_y(0.0)),
            label_x: format!("{cross_x:.1}"),
            label_y: format!("{:.1}", chart_y(0.0) - 8.0),
            label_anchor: "middle",
            label_text,
            state: "warn",
        }
    } else {
        let end_frac = 1.0 - rate;
        let end_y = chart_y(end_frac);
        BurnChart {
            line_points: format!(
                "{} {}",
                point(chart_x(0.0), chart_y(1.0)),
                point(chart_x(1.0), end_y),
            ),
            dot_x: format!("{CHART_X1:.1}"),
            dot_y: format!("{end_y:.1}"),
            label_x: format!("{:.1}", CHART_X1 + 6.0),
            label_y: format!("{end_y:.1}"),
            label_anchor: "start",
            label_text: format!("{}% left", (end_frac * 100.0).round() as i64),
            state: "ok",
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_error_budget.html")]
struct ErrorBudgetPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    rows: Vec<BurnRow>,
    default: BudgetResult,
    chart: BurnChart,
    default_slo: String,
    default_measured: String,
    default_window: &'static str,
    slo_values: String,
    windows: &'static [(&'static str, f64)],
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

static ERROR_BUDGET_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_error_budget(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, ERROR_BUDGET_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{ERROR_BUDGET_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = ERROR_BUDGET_DESCRIPTION.to_string();
    og.image = format!(
        "{}/static/marketing/og-error-budget.png",
        cfg.canonical_origin
    );
    let page = ErrorBudgetPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            ERROR_BUDGET_TITLE,
            ERROR_BUDGET_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            ERROR_BUDGET_TITLE,
            ERROR_BUDGET_PATH,
            ERROR_BUDGET_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            ERROR_BUDGET_PATH,
            ERROR_BUDGET_TITLE,
            ERROR_BUDGET_CREATED,
            ERROR_BUDGET_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(ERROR_BUDGET_FAQS),
        faqs: ERROR_BUDGET_FAQS,
        rows: burn_rows(),
        default: budget_result(DEFAULT_SLO, DEFAULT_MEASURED, DEFAULT_WINDOW),
        chart: burn_chart(DEFAULT_SLO, DEFAULT_MEASURED, DEFAULT_WINDOW),
        default_slo: format!("{DEFAULT_SLO}"),
        default_measured: format!("{DEFAULT_MEASURED}"),
        default_window: DEFAULT_WINDOW_LABEL,
        slo_values: ERROR_BUDGET_SLOS
            .iter()
            .map(|v| format!("{v}"))
            .collect::<Vec<_>>()
            .join(","),
        windows: ERROR_BUDGET_WINDOWS,
        canonical_url,
        og,
        tools: TOOLS,
        self_path: ERROR_BUDGET_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- error-budget render failed: {e} -->"));
    cached_render(body)
}

async fn error_budget(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = ERROR_BUDGET_CACHED.get_or_init(|| render_error_budget(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

async fn cron(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = CRON_CACHED.get_or_init(|| render_cron(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

// ── Incident update generator ───────────────────────────────────────
// Deterministic and browser-only: no incident text is submitted or stored.

pub const INCIDENT_UPDATE_PATH: &str = "/tools/incident-update-generator";
const INCIDENT_UPDATE_CREATED: &str = "2026-07-20";
const INCIDENT_UPDATE_LASTMOD: &str = "2026-09-04";
pub const INCIDENT_UPDATE_TITLE: &str = "Incident Update Message Generator";
const INCIDENT_UPDATE_LABEL: &str = "incident message generator";
pub const INCIDENT_UPDATE_DESCRIPTION: &str = "Write clear investigating, identified, monitoring, resolved and maintenance messages for your status page. Free, private and generated in your browser.";

const INCIDENT_UPDATE_FAQS: &[(&str, &str)] = &[
    (
        "What should an incident update include?",
        "Lead with the customer impact, say what the team is doing, and give a specific time for the next update. Share confirmed facts and label anything still under investigation.",
    ),
    (
        "How often should a status page be updated?",
        "Set the next update time in the first message and keep that promise, even when there is no major change. During a serious outage, updates every 30 to 60 minutes are a useful starting point.",
    ),
    (
        "What is the difference between investigating and identified?",
        "Investigating means the cause is not confirmed. Identified means the team has confirmed the cause and is working on a fix or mitigation. Do not move to identified based only on a suspicion.",
    ),
    (
        "Should an incident update include an estimated resolution time?",
        "Only include an estimate when the team has enough evidence to trust it. An accurate next-update time is more useful than a speculative resolution time that may slip.",
    ),
    (
        "Does this generator send or store incident details?",
        "No. The messages are generated locally in your browser. Uptimepage does not receive or store anything you type into this tool.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_incident_update.html")]
struct IncidentUpdatePage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

static INCIDENT_UPDATE_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_incident_update(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, INCIDENT_UPDATE_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{INCIDENT_UPDATE_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = INCIDENT_UPDATE_DESCRIPTION.to_string();
    let page = IncidentUpdatePage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            INCIDENT_UPDATE_TITLE,
            INCIDENT_UPDATE_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            INCIDENT_UPDATE_TITLE,
            INCIDENT_UPDATE_PATH,
            INCIDENT_UPDATE_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            INCIDENT_UPDATE_PATH,
            INCIDENT_UPDATE_TITLE,
            INCIDENT_UPDATE_CREATED,
            INCIDENT_UPDATE_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(INCIDENT_UPDATE_FAQS),
        faqs: INCIDENT_UPDATE_FAQS,
        canonical_url,
        og,
        tools: TOOLS,
        self_path: INCIDENT_UPDATE_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- incident-update render failed: {e} -->"));
    cached_render(body)
}

async fn incident_update(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = INCIDENT_UPDATE_CACHED.get_or_init(|| render_incident_update(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

// ── DNS lookup ────────────────────────────────────────────────────────

pub const DNS_LOOKUP_PATH: &str = "/tools/dns-lookup";
const DNS_LOOKUP_CREATED: &str = "2026-08-05";
pub const DNS_LOOKUP_LASTMOD: &str = "2026-08-05";
pub const DNS_LOOKUP_TITLE: &str = "DNS Lookup: Compare Two Resolvers Side by Side";
const DNS_LOOKUP_LABEL: &str = "DNS lookup";
pub const DNS_LOOKUP_DESCRIPTION: &str = "Check A, MX, NS, TXT and CAA records against Cloudflare and Google at once and see where they disagree. Free, no sign-up, runs in your browser.";

/// Offered record types. The number is the DNS RR type the resolvers take, kept
/// beside the label so the template and the script cannot drift.
const DNS_RECORD_TYPES: &[(&str, u16, &str)] = &[
    ("A", 1, "IPv4 address the name points at"),
    ("AAAA", 28, "IPv6 address the name points at"),
    ("CNAME", 5, "Alias pointing this name at another"),
    ("MX", 15, "Mail servers, in priority order"),
    ("NS", 2, "Nameservers the zone is delegated to"),
    ("TXT", 16, "Free-form text: SPF, DMARC, domain verification"),
    ("SOA", 6, "Zone's start of authority and serial"),
    ("CAA", 257, "Which authorities may issue certificates"),
];

const DNS_LOOKUP_FAQS: &[(&str, &str)] = &[
    (
        "Why do two resolvers return different answers?",
        "Usually you are seeing a change mid-flight: a record was edited and one resolver still holds the old answer until its TTL runs out. It can also mean a filtering or captive resolver is rewriting the answer, which is what makes a site look down from one network and fine from every other.",
    ),
    (
        "What is a TTL?",
        "Time to live, in seconds: how long a resolver may cache the answer before asking again. A record with a TTL of 3600 can keep serving the old value for an hour after you change it, so lower the TTL before a planned migration, not after.",
    ),
    (
        "My site loads but mail stopped. What should I check?",
        "Look at MX first, then TXT for the SPF record. A domain that expired and got parked keeps answering on port 80, so an uptime check stays green while MX records quietly disappear and mail dies. That failure is invisible to anything that only watches the homepage.",
    ),
    (
        "Does this send my domain anywhere?",
        "The lookup runs in your browser and goes straight to the public resolvers at Cloudflare and Google. It does not pass through our servers, and nothing is stored.",
    ),
    (
        "Can I be told when these records change?",
        "Yes. A DNS check watches a name and record type on a schedule and alerts when the answer changes or disappears. The button beside each result opens a monitor prefilled with what you just looked up.",
    ),
];

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tool_dns_lookup.html")]
struct DnsLookupPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    web_application_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    faq_json_ld: JsonLd,
    faqs: &'static [(&'static str, &'static str)],
    record_types: &'static [(&'static str, u16, &'static str)],
    tools: &'static [ToolMeta],
    self_path: &'static str,
    version: &'static str,
}

static DNS_LOOKUP_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_dns_lookup(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, DNS_LOOKUP_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{DNS_LOOKUP_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = DNS_LOOKUP_DESCRIPTION.to_string();
    let page = DnsLookupPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            DNS_LOOKUP_TITLE,
            DNS_LOOKUP_PATH,
        ),
        web_application_json_ld: json_ld_web_application(
            &cfg.canonical_origin,
            DNS_LOOKUP_TITLE,
            DNS_LOOKUP_PATH,
            DNS_LOOKUP_DESCRIPTION,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            DNS_LOOKUP_PATH,
            DNS_LOOKUP_TITLE,
            DNS_LOOKUP_CREATED,
            DNS_LOOKUP_LASTMOD,
            true,
        ),
        faq_json_ld: json_ld_faqpage(DNS_LOOKUP_FAQS),
        faqs: DNS_LOOKUP_FAQS,
        record_types: DNS_RECORD_TYPES,
        canonical_url,
        og,
        tools: TOOLS,
        self_path: DNS_LOOKUP_PATH,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- dns-lookup render failed: {e} -->"));
    cached_render(body)
}

async fn dns_lookup(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = DNS_LOOKUP_CACHED.get_or_init(|| render_dns_lookup(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

/// Path + copy for each tool, so the sitemap and llms index iterate one list.
pub struct ToolMeta {
    pub path: &'static str,
    pub title: &'static str,
    /// Lower-case name for the sibling strip in each tool's footer, where the
    /// full SEO title reads as shouting.
    pub label: &'static str,
    pub description: &'static str,
    pub lastmod: &'static str,
}

pub const TOOLS: &[ToolMeta] = &[
    ToolMeta {
        path: UPTIME_SLA_PATH,
        title: UPTIME_SLA_TITLE,
        label: UPTIME_SLA_LABEL,
        description: UPTIME_SLA_DESCRIPTION,
        lastmod: UPTIME_SLA_LASTMOD,
    },
    ToolMeta {
        path: CRON_PATH,
        title: CRON_TITLE,
        label: CRON_LABEL,
        description: CRON_DESCRIPTION,
        lastmod: CRON_LASTMOD,
    },
    ToolMeta {
        path: ERROR_BUDGET_PATH,
        title: ERROR_BUDGET_TITLE,
        label: ERROR_BUDGET_LABEL,
        description: ERROR_BUDGET_DESCRIPTION,
        lastmod: ERROR_BUDGET_LASTMOD,
    },
    ToolMeta {
        path: INCIDENT_UPDATE_PATH,
        title: INCIDENT_UPDATE_TITLE,
        label: INCIDENT_UPDATE_LABEL,
        description: INCIDENT_UPDATE_DESCRIPTION,
        lastmod: INCIDENT_UPDATE_LASTMOD,
    },
    ToolMeta {
        path: DNS_LOOKUP_PATH,
        title: DNS_LOOKUP_TITLE,
        label: DNS_LOOKUP_LABEL,
        description: DNS_LOOKUP_DESCRIPTION,
        lastmod: DNS_LOOKUP_LASTMOD,
    },
    ToolMeta {
        path: http_headers::HEADER_CHECKER_PATH,
        title: http_headers::HEADER_CHECKER_TITLE,
        label: http_headers::HEADER_CHECKER_LABEL,
        description: http_headers::HEADER_CHECKER_DESCRIPTION,
        lastmod: http_headers::HEADER_CHECKER_LASTMOD,
    },
    ToolMeta {
        path: ssl::SSL_CHECKER_PATH,
        title: ssl::SSL_CHECKER_TITLE,
        label: ssl::SSL_CHECKER_LABEL,
        description: ssl::SSL_CHECKER_DESCRIPTION,
        lastmod: ssl::SSL_CHECKER_LASTMOD,
    },
    ToolMeta {
        path: domain_expiry::DOMAIN_CHECKER_PATH,
        title: domain_expiry::DOMAIN_CHECKER_TITLE,
        label: domain_expiry::DOMAIN_CHECKER_LABEL,
        description: domain_expiry::DOMAIN_CHECKER_DESCRIPTION,
        lastmod: domain_expiry::DOMAIN_CHECKER_LASTMOD,
    },
    ToolMeta {
        path: security::SECURITY_CHECKER_PATH,
        title: security::SECURITY_CHECKER_TITLE,
        label: security::SECURITY_CHECKER_LABEL,
        description: security::SECURITY_CHECKER_DESCRIPTION,
        lastmod: security::SECURITY_CHECKER_LASTMOD,
    },
];

// ── Tools index ───────────────────────────────────────────────────────

pub const TOOLS_INDEX_PATH: &str = "/tools";
const TOOLS_INDEX_CREATED: &str = "2026-07-09";
pub const TOOLS_INDEX_LASTMOD: &str = "2026-09-18";
const TOOLS_INDEX_TITLE: &str = "Free Tools for Developers & SREs";
const TOOLS_INDEX_DESCRIPTION: &str = "Free, no sign-up calculators and generators for uptime, reliability and scheduling. Built for our own work and kept open for yours.";

#[derive(Template, WebTemplate)]
#[template(path = "marketing/tools_index.html")]
struct ToolsIndexPage {
    app_url: String,
    canonical_url: String,
    og: OpenGraph,
    breadcrumb_json_ld: JsonLd,
    webpage_json_ld: JsonLd,
    item_list_json_ld: JsonLd,
    tools: &'static [ToolMeta],
    version: &'static str,
}

static TOOLS_INDEX_CACHED: OnceLock<CachedRender> = OnceLock::new();

fn render_tools_index(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{}", cfg.canonical_origin, TOOLS_INDEX_PATH);
    let mut og = OpenGraph::default_for(
        &format!("{TOOLS_INDEX_TITLE} | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = TOOLS_INDEX_DESCRIPTION.to_string();
    let items: Vec<(&str, &str)> = TOOLS.iter().map(|t| (t.title, t.path)).collect();
    let page = ToolsIndexPage {
        app_url: cfg.app_url.clone(),
        breadcrumb_json_ld: json_ld_breadcrumb(
            &cfg.canonical_origin,
            TOOLS_INDEX_TITLE,
            TOOLS_INDEX_PATH,
        ),
        webpage_json_ld: json_ld_webpage(
            &cfg.canonical_origin,
            TOOLS_INDEX_PATH,
            TOOLS_INDEX_TITLE,
            TOOLS_INDEX_CREATED,
            TOOLS_INDEX_LASTMOD,
            true,
        ),
        item_list_json_ld: json_ld_item_list_links(
            &cfg.canonical_origin,
            TOOLS_INDEX_PATH,
            TOOLS_INDEX_TITLE,
            &items,
        ),
        tools: TOOLS,
        canonical_url,
        og,
        version: env!("CARGO_PKG_VERSION"),
    };
    let body = page
        .render()
        .unwrap_or_else(|e| format!("<!-- tools-index render failed: {e} -->"));
    cached_render(body)
}

async fn tools_index(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = TOOLS_INDEX_CACHED.get_or_init(|| render_tools_index(&cfg));
    serve_cached(&headers, cached, &TOOL_CACHE_CONTROL)
}

/// Mount every tool route.
pub fn mount(router: axum::Router<Arc<MarketingCfg>>) -> axum::Router<Arc<MarketingCfg>> {
    router
        .route(TOOLS_INDEX_PATH, axum::routing::get(tools_index))
        .route(UPTIME_SLA_PATH, axum::routing::get(uptime_sla))
        .route(CRON_PATH, axum::routing::get(cron))
        .route(ERROR_BUDGET_PATH, axum::routing::get(error_budget))
        .route(INCIDENT_UPDATE_PATH, axum::routing::get(incident_update))
        .route(DNS_LOOKUP_PATH, axum::routing::get(dns_lookup))
        .route(ssl::SSL_CHECKER_PATH, axum::routing::get(ssl::page))
        .route(
            security::SECURITY_CHECKER_PATH,
            axum::routing::get(security::page),
        )
        .route(ssl::SSL_PROBE_PATH, axum::routing::get(ssl::probe))
        .route(
            domain_expiry::DOMAIN_CHECKER_PATH,
            axum::routing::get(domain_expiry::page),
        )
        .route(
            domain_expiry::DOMAIN_PROBE_PATH,
            axum::routing::get(domain_expiry::probe),
        )
        .route(
            http_headers::HEADER_CHECKER_PATH,
            axum::routing::get(http_headers::page),
        )
        .route(
            http_headers::HEADER_PROBE_PATH,
            axum::routing::get(http_headers::probe),
        )
}

pub(crate) fn warm(cfg: &MarketingCfg) {
    TOOLS_INDEX_CACHED.get_or_init(|| render_tools_index(cfg));
    UPTIME_SLA_CACHED.get_or_init(|| render_uptime_sla(cfg));
    CRON_CACHED.get_or_init(|| render_cron(cfg));
    ERROR_BUDGET_CACHED.get_or_init(|| render_error_budget(cfg));
    INCIDENT_UPDATE_CACHED.get_or_init(|| render_incident_update(cfg));
    DNS_LOOKUP_CACHED.get_or_init(|| render_dns_lookup(cfg));
    ssl::warm(cfg);
    security::warm(cfg);
    domain_expiry::warm(cfg);
    http_headers::warm(cfg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downtime_matches_known_slas() {
        // 99.9% over a 30-day month is 43m 12s.
        assert_eq!(human_duration(downtime(99.9, MONTH)), "43m 12s");
        // 99.99% over a year is 52m 34s.
        assert_eq!(human_duration(downtime(99.99, YEAR)), "52m 34s");
        // Five nines over a day is sub-second.
        assert_eq!(human_duration(downtime(99.999, DAY)), "864ms");
    }

    #[test]
    fn reference_table_is_complete() {
        let rows = reference_rows();
        assert_eq!(rows.len(), REFERENCE_NINES.len());
        assert_eq!(rows[0].label, "90%");
        assert!(rows.iter().all(|r| !r.yearly.is_empty()));
    }

    #[test]
    fn level_targets_stay_subsets_of_the_table_and_presets() {
        for (pct, _, _) in SLA_LEVELS {
            assert!(
                REFERENCE_NINES.contains(pct),
                "{pct}% has a prose section but no table row"
            );
        }
        for pct in PRESETS {
            let covered = SLA_LEVELS.iter().any(|(p, _, _)| p == pct);
            assert!(covered, "preset {pct}% has no prose section");
        }
    }

    #[test]
    fn level_sections_carry_anchors_and_numbers() {
        let levels = sla_level_sections();
        assert_eq!(levels.len(), SLA_LEVELS.len());
        let mut anchors: Vec<_> = levels.iter().map(|l| l.anchor.as_str()).collect();
        anchors.sort_unstable();
        anchors.dedup();
        assert_eq!(anchors.len(), levels.len(), "anchor collision");
        let three_nines = levels.iter().find(|l| l.anchor == "sla-99-9").unwrap();
        // Restating a guide's title here makes the two pages bid on one query.
        assert_eq!(three_nines.heading, "Downtime allowed at 99.9%");
        assert!(three_nines.lede.contains("43m 12s"));
        let half = levels.iter().find(|l| l.anchor == "sla-99-5").unwrap();
        assert!(half.lede.contains("3h 36m"));
    }

    #[test]
    fn reverse_resolves_downtime_to_uptime() {
        // One hour of downtime in a 30-day month is ~99.8611%.
        assert_eq!(format_pct(uptime_for(3_600.0, MONTH)), "99.8611%");
        // A clean 0.1% budget round-trips to 99.9%, no trailing zeros.
        assert_eq!(format_pct(uptime_for(downtime(99.9, YEAR), YEAR)), "99.9%");
        // Over-budget clamps at zero, never negative.
        assert_eq!(format_pct(uptime_for(MONTH * 2.0, MONTH)), "0%");
    }

    #[test]
    fn default_result_ties_to_check_interval() {
        let r = sla_result(99.9);
        assert_eq!(r.monthly, "43m 12s");
        // 2592s of monthly downtime is ~43 missed 60s checks.
        assert_eq!(r.checks_monthly, "43");
    }

    fn test_cfg() -> MarketingCfg {
        MarketingCfg {
            app_url: "https://app.uptimepage.dev".into(),
            canonical_origin: "https://uptimepage.dev".into(),
            blog_enabled: false,
            mcp_url: None,
            checkout_open: false,
            trusted_proxies: Vec::new(),
        }
    }

    /// The sibling strip used to be hand-typed per template and had already
    /// drifted: several pages listed four of their five siblings.
    #[test]
    fn every_tool_footer_links_every_sibling() {
        let cfg = test_cfg();
        let pages: Vec<(&str, CachedRender)> = vec![
            (UPTIME_SLA_PATH, render_uptime_sla(&cfg)),
            (CRON_PATH, render_cron(&cfg)),
            (ERROR_BUDGET_PATH, render_error_budget(&cfg)),
            (INCIDENT_UPDATE_PATH, render_incident_update(&cfg)),
            (DNS_LOOKUP_PATH, render_dns_lookup(&cfg)),
            (ssl::SSL_CHECKER_PATH, ssl::render(&cfg)),
            (security::SECURITY_CHECKER_PATH, security::render(&cfg)),
            (
                domain_expiry::DOMAIN_CHECKER_PATH,
                domain_expiry::render(&cfg),
            ),
            (
                http_headers::HEADER_CHECKER_PATH,
                http_headers::render(&cfg),
            ),
        ];
        assert_eq!(
            pages.len(),
            TOOLS.len(),
            "a tool ships without a render here"
        );
        for (path, page) in pages {
            let html = std::str::from_utf8(&page.body).expect("tool renders UTF-8");
            assert!(!html.contains("render failed"), "{path} failed to render");
            for sibling in TOOLS.iter().filter(|t| t.path != path) {
                assert!(
                    html.contains(&format!(r#"href="{}""#, sibling.path)),
                    "{path} does not link {}",
                    sibling.path
                );
                assert!(
                    html.contains(sibling.label),
                    "{path} omits {}",
                    sibling.label
                );
            }
            assert!(
                html.contains(r#"href="/tools""#),
                "{path} does not link the hub"
            );
        }
    }

    /// A label is the short form the strip prints mid-sentence. Reusing the SEO
    /// title there reads as shouting and repeats the brand suffix logic.
    #[test]
    fn tool_labels_are_short_and_distinct() {
        let mut seen: Vec<&str> = Vec::new();
        for t in TOOLS {
            assert!(!t.label.is_empty(), "{} has no label", t.path);
            assert!(
                t.label.len() < t.title.len(),
                "{} label is not shorter than its title",
                t.path
            );
            assert!(!seen.contains(&t.label), "duplicate label {}", t.label);
            seen.push(t.label);
        }
    }

    #[test]
    fn tools_registry_is_seo_clean() {
        let paths: Vec<_> = TOOLS.iter().map(|t| t.path).collect();
        assert!(paths.contains(&UPTIME_SLA_PATH));
        assert!(paths.contains(&CRON_PATH));
        assert!(paths.contains(&ERROR_BUDGET_PATH));
        assert!(paths.contains(&INCIDENT_UPDATE_PATH));
        for t in TOOLS {
            assert!(
                t.path.starts_with("/tools/"),
                "{} not under /tools/",
                t.path
            );
            let rendered_title = t.title.len() + " | ".len() + BRAND.len();
            assert!(
                rendered_title <= 60,
                "{} title {rendered_title} > 60",
                t.path
            );
            assert!(
                t.description.len() <= 160,
                "{} description {} > 160",
                t.path,
                t.description.len()
            );
        }
    }

    #[test]
    fn burn_rate_compares_actual_to_allowed() {
        // Measured equal to the SLO spends the budget exactly: 1x.
        assert!((burn_rate(99.9, 99.9) - 1.0).abs() < 1e-9);
        // Twice the allowed failure rate is 2x.
        assert!((burn_rate(99.8, 99.9) - 2.0).abs() < 1e-9);
        // Perfect availability burns nothing.
        assert_eq!(burn_rate(100.0, 99.9), 0.0);
        // Better than target stays under 1x.
        assert!(burn_rate(99.95, 99.9) < 1.0);
    }

    #[test]
    fn burn_rate_handles_perfect_slo_without_dividing_by_zero() {
        // A 100% SLO leaves no budget: any downtime burns infinitely fast.
        assert!(burn_rate(99.9, 100.0).is_infinite());
        // No downtime against a 100% SLO is still zero, not NaN.
        assert_eq!(burn_rate(100.0, 100.0), 0.0);
    }

    #[test]
    fn budget_result_splits_used_and_remaining() {
        // 99.95% measured against a 99.9% SLO over a month: half the 43m12s
        // budget spent, half left, 0.5x burn, on track.
        let r = budget_result(99.9, 99.95, MONTH);
        assert_eq!(r.total, "43m 12s");
        assert_eq!(r.used, "21m 36s");
        assert_eq!(r.remaining, "21m 36s");
        assert_eq!(r.burn, "0.5×");
        assert_eq!(r.verdict_state, "ok");
    }

    #[test]
    fn budget_result_over_budget_clamps_remaining_and_warns() {
        // Worse than the SLO: no budget left, over-budget verdict.
        let r = budget_result(99.9, 99.0, MONTH);
        assert_eq!(r.remaining, "0s");
        assert_eq!(r.burn, "10×");
        assert_eq!(r.verdict_state, "warn");
    }

    #[test]
    fn burn_table_maps_rate_to_lifetime() {
        let rows = burn_rows();
        assert_eq!(rows.len(), BURN_RATES.len());
        // 1x lasts the whole 30-day window.
        assert_eq!(rows[0].rate, "1×");
        assert_eq!(rows[0].lasts, "30d");
        // 14.4x spends 2% of a 30-day budget per hour and lasts ~2 days.
        let fast = rows.last().unwrap();
        assert_eq!(fast.rate, "14.4×");
        assert_eq!(fast.per_hour, "2%");
        assert_eq!(fast.lasts, "2d 2h");
    }

    #[test]
    fn burn_chart_plots_slope_and_crossing() {
        // Default 0.5x: line from full budget to the half mark, green.
        let c = burn_chart(99.9, 99.95, MONTH);
        assert_eq!(c.line_points, "40.0,16.0 568.0,103.0");
        assert_eq!(c.state, "ok");
        assert_eq!(c.label_text, "50% left");
        // 10x crosses zero a tenth of the way in; dot sits on the zero axis.
        let o = burn_chart(99.9, 99.0, MONTH);
        assert_eq!(o.state, "warn");
        assert_eq!(o.dot_y, format!("{CHART_Y1:.1}"));
        assert_eq!(o.label_text, "gone in 3d");
    }
}
