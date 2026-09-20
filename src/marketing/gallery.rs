//! The "Inside the app" screenshot set.
//!
//! Read by three surfaces that must not drift: the landing template, the image
//! sitemap, and `SoftwareApplication.screenshot`.

/// `caption` renders under the card; `alt` must not repeat it verbatim, or a
/// screen reader hears the same sentence twice. `width`/`height` must match the
/// file on disk. `file` is a Google Images ranking signal, so it names the
/// query the shot answers.
pub struct Shot {
    pub id: &'static str,
    pub file: &'static str,
    pub alt: &'static str,
    pub caption: &'static str,
    pub width: u32,
    pub height: u32,
}

pub const SHOTS: &[Shot] = &[
    Shot {
        id: "shot-dashboard",
        file: "marketing/uptime-monitoring-dashboard.webp",
        alt: "The Uptimepage dashboard: 24-hour uptime, average response time, \
              and total checks, above a list of monitors with live status and \
              latency sparklines.",
        caption: "Dashboard — uptime, response time, and every monitor at a glance",
        width: 2014,
        height: 1611,
    },
    Shot {
        id: "shot-monitors",
        file: "marketing/uptime-monitors-list.webp",
        alt: "The monitors list: checks grouped by environment, each with live \
              status, type, tags, last check time, and 30-day uptime.",
        caption: "Monitors — grouped by environment, filterable by type, tag, or owner",
        width: 2014,
        height: 1611,
    },
    Shot {
        id: "shot-monitor-detail",
        file: "marketing/monitor-latency-by-region.webp",
        alt: "One monitor in detail: uptime and check counts for the last 24 \
              hours, plus median latency charted per region.",
        caption: "Monitor detail — latency broken down by region",
        width: 2014,
        height: 1611,
    },
    Shot {
        id: "shot-channels",
        file: "marketing/notification-channels-slack-pagerduty-sms.webp",
        alt: "The notification channels list: Slack, PagerDuty, SMS, email, \
              webhook, Discord, Teams, Pushover, ntfy, and Telegram, each \
              enabled or disabled.",
        caption: "Alerting — Slack, PagerDuty, SMS, webhook, and seven more",
        width: 2014,
        height: 1611,
    },
    Shot {
        id: "shot-create-channel",
        file: "marketing/add-notification-channel.webp",
        alt: "The new notification-channel form: choose a channel type such as \
              Slack, Discord, email, Telegram, PagerDuty, SMS, or webhook.",
        caption: "Adding a channel — pick a transport, paste one credential",
        width: 2014,
        height: 1611,
    },
    Shot {
        id: "shot-status",
        file: "marketing/public-status-page.webp",
        alt: "A public status page reading All Systems Operational, with 90-day \
              uptime history per component.",
        caption: "Public status page — 90 days of history per component",
        width: 2014,
        height: 1611,
    },
];

/// Fingerprinted to match what the page renders: a bare path would index as a
/// second, separate image.
pub fn absolute_url(canonical_origin: &str, shot: &Shot) -> String {
    format!(
        "{canonical_origin}{}",
        crate::templates::assets::url(shot.file)
    )
}
