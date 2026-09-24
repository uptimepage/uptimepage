//! Competitor fact tables. Every claim here is dated and verified against
//! the named project's own repo, docs or plan pages; refresh them when they
//! drift.

use super::model::{Matrix, MatrixRow};

/// Decision matrix for `/open-source-uptime-monitoring`, verified July 2026
/// against each project's repo and docs. Uptime Kuma is the search default,
/// OpenStatus the nearest AGPL alternative, and Prometheus + Blackbox the
/// build-it-yourself route. Tones reflect reality, not favour: Kuma runs in
/// one container, which we do not.
static OPEN_SOURCE_MONITOR_MATRIX: Matrix = Matrix {
    heading: "Open-source uptime monitors compared",
    columns: &[
        "Uptimepage",
        "Uptime Kuma",
        "OpenStatus",
        "Prometheus + Blackbox",
    ],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[
                ("AGPL", "yes"),
                ("MIT", "yes"),
                ("AGPL", "yes"),
                ("Apache 2.0", "yes"),
            ],
        },
        MatrixRow {
            label: "built with",
            cells: &[
                ("Rust", "yes"),
                ("JavaScript / Node", ""),
                ("TypeScript / Node", ""),
                ("Go", ""),
            ],
        },
        MatrixRow {
            label: "what you run",
            cells: &[
                ("binary + Postgres + ClickHouse", "part"),
                ("one container", "yes"),
                ("~6 Docker services", "part"),
                ("Prometheus + Alertmanager", "no"),
            ],
        },
        MatrixRow {
            label: "check kinds",
            cells: &[
                (
                    "HTTP, TCP, DNS, TLS, domain expiry, ping, heartbeat, browser flow",
                    "yes",
                ),
                ("HTTP, TCP, DNS, ping, push, browser, and more", "yes"),
                ("HTTP, TCP, DNS, ICMP", "part"),
                ("HTTP, TCP, DNS, ICMP, gRPC, TLS", "part"),
            ],
        },
        MatrixRow {
            label: "watches a cron job",
            cells: &[
                ("heartbeat, period + grace", "yes"),
                ("push monitor", "yes"),
                ("no", "no"),
                ("no, outside-in only", "no"),
            ],
        },
        MatrixRow {
            label: "customer status page",
            cells: &[
                ("branded, subscribers", "yes"),
                ("basic, no subscribers", "part"),
                ("yes", "yes"),
                ("build it yourself", "no"),
            ],
        },
        MatrixRow {
            label: "monitoring as code",
            cells: &[
                ("Terraform · REST · MCP", "yes"),
                ("no REST API for monitors", "no"),
                ("Terraform · REST · MCP", "yes"),
                ("config files", "part"),
            ],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[
                ("5 hosted regions", "yes"),
                ("via Globalping add-on", "part"),
                ("28 regions on hosted", "part"),
                ("federate it yourself", "part"),
            ],
        },
        MatrixRow {
            label: "teams and roles",
            cells: &[
                ("orgs + roles", "yes"),
                ("single shared login", "no"),
                ("yes", "yes"),
                ("external SSO / proxy", "part"),
            ],
        },
        MatrixRow {
            label: "auto incidents from checks",
            cells: &[
                ("yes", "yes"),
                ("basic", "part"),
                ("yes", "yes"),
                ("via Alertmanager", "part"),
            ],
        },
    ],
    notes: &[
        "Uptime Kuma is MIT; OpenStatus and Uptimepage are AGPL-3.0; Prometheus and Blackbox exporter are Apache 2.0.",
        "Check kinds read from source on 2026-08-27: Uptime Kuma's push monitor in server/model/monitor.js, the OpenStatus checker's handlers (dns, icmp, ping, tcp), and Blackbox exporter's prober modules. Only Uptimepage and Uptime Kuma accept an inbound ping; the other two probe from outside and cannot see a job that never ran.",
        "OpenStatus's 28-region checking is on its hosted tier; self-hosting runs several Docker services.",
        "Verified July 2026 against each project's repository and docs.",
    ],
};

/// Head-to-head facts for `/vs/self-hosted-status-pages`, verified in July
/// 2026 against each project's repository and, for Cachet, its live v3 source
/// (`cachethq/core`, whose docs lag the code). Cells are `(text, tone)`; the
/// first column is always Uptimepage. Refresh when a project releases a new version.
static SELF_HOSTED_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &["Uptimepage", "Upptime", "Cachet", "Statping"],
    rows: &[
        MatrixRow {
            label: "uptime checks built in",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("basic HTTP", "part"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
                ("HTTP · TCP · ICMP", ""),
                ("HTTP GET", ""),
                ("HTTP · TCP · UDP · ICMP · gRPC", ""),
            ],
        },
        MatrixRow {
            label: "fastest check interval",
            cells: &[
                ("60s", "yes"),
                ("5 min", "part"),
                ("your cron", "part"),
                ("~30s", "yes"),
            ],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[
                ("5 hosted regions", "yes"),
                ("bolt-on", "part"),
                ("no", "no"),
                ("no", "no"),
            ],
        },
        MatrixRow {
            label: "status page",
            cells: &[
                ("branded", "yes"),
                ("static", ""),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "visitor subscribers",
            cells: &[
                ("email + webhook", "yes"),
                ("no", "no"),
                ("email + webhook", "yes"),
                ("no", "no"),
            ],
        },
        MatrixRow {
            label: "auto incidents from checks",
            cells: &[
                ("yes", "yes"),
                ("GitHub issues", ""),
                ("status only", "part"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[
                ("Terraform · API · MCP", "yes"),
                ("YAML", ""),
                ("API", ""),
                ("YAML · API", ""),
            ],
        },
        MatrixRow {
            label: "official Terraform provider",
            cells: &[
                ("yes", "yes"),
                ("no", "no"),
                ("community", "part"),
                ("no", "no"),
            ],
        },
        MatrixRow {
            label: "tech stack",
            cells: &[
                ("Rust", ""),
                ("JavaScript", ""),
                ("PHP / Laravel", ""),
                ("Go", ""),
            ],
        },
        MatrixRow {
            label: "how you run it",
            cells: &[
                ("hosted or self-host", "yes"),
                ("Actions + Pages", ""),
                ("PHP/Laravel + DB", ""),
                ("Go binary", ""),
            ],
        },
        MatrixRow {
            label: "license",
            cells: &[
                ("AGPL-3.0", ""),
                ("MIT", ""),
                ("source-available", "part"),
                ("GPL-3.0", ""),
            ],
        },
        MatrixRow {
            label: "maintained in 2026",
            cells: &[
                ("active", "yes"),
                ("low velocity", "part"),
                ("active, v3 dev", "part"),
                ("~yearly", "part"),
            ],
        },
    ],
    notes: &[
        "Statping here is the maintained community fork, statping-ng; the original Statping has not shipped a release since 2020.",
        "Upptime runs checks as GitHub Actions cron jobs, which cannot fire more than once every five minutes and can drift later under load. Reaching other regions needs the third-party Globalping add-on.",
        "Cachet's actively developed v3 (the cachethq/core source) added basic HTTP component checks in mid-2026 and now sends confirmed email to subscribers, but it is still 3.x-dev with no stable release: checks are HTTP GET only on a cron you add yourself, a failed check colours a component rather than opening an incident, subscriptions are global rather than per-component, and the code ships under a custom source-available license.",
        "Competitor facts were verified against each project's repository and docs in July 2026. Open-source projects move quickly, so check their current docs before you decide.",
    ],
};

/// Head-to-head facts for `/vs/self-hosted-monitoring`, verified in July 2026
/// against each project's local source: Uptime Kuma 2.4.0, OpenStatus (HEAD
/// 2026-05), OneUptime 11.0.12, Gatus 5.36.0, Kener 4.1.1. Cells are
/// `(text, tone)`; the first column is always Uptimepage.
static MONITORING_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &[
        "Uptimepage",
        "Uptime Kuma",
        "OpenStatus",
        "OneUptime",
        "Gatus",
        "Kener",
    ],
    rows: &[
        MatrixRow {
            label: "fastest check interval",
            cells: &[
                ("10s", "yes"),
                ("1s", "yes"),
                ("30s", ""),
                ("60s", ""),
                ("seconds", "yes"),
                ("60s", ""),
            ],
        },
        MatrixRow {
            label: "check breadth",
            cells: &[
                ("HTTP·TCP·DNS·TLS·domain·ping·heartbeat·flow", ""),
                ("31 types", "yes"),
                ("HTTP·TCP·DNS", ""),
                ("25+ types", "yes"),
                ("11 protocols", "yes"),
                ("9 types", ""),
            ],
        },
        MatrixRow {
            label: "ping / ICMP",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("no", "no"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "TLS + domain expiry",
            cells: &[
                ("yes", "yes"),
                ("yes", ""),
                ("stub", "no"),
                ("yes", ""),
                ("yes", ""),
                ("SSL only", "part"),
            ],
        },
        MatrixRow {
            label: "push / heartbeat monitor",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("no", "no"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "branded status page",
            cells: &[
                ("yes", "yes"),
                ("basic", "part"),
                ("yes", ""),
                ("yes", ""),
                ("dashboard", "no"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "status-page subscribers",
            cells: &[
                ("email + webhook", "yes"),
                ("RSS only", "part"),
                ("email + webhook", ""),
                ("email·SMS·Slack", "yes"),
                ("no", "no"),
                ("email + RSS", ""),
            ],
        },
        MatrixRow {
            label: "alert channels",
            cells: &[
                ("14", ""),
                ("94", "yes"),
                ("13", ""),
                ("9", ""),
                ("41", "yes"),
                ("4", "no"),
            ],
        },
        MatrixRow {
            label: "auto-incidents from checks",
            cells: &[
                ("yes", "yes"),
                ("no", "no"),
                ("yes", ""),
                ("yes", ""),
                ("no", "no"),
                ("yes", ""),
            ],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[
                ("5 hosted regions", "yes"),
                ("add-on", "part"),
                ("28 regions", "yes"),
                ("yes", ""),
                ("no", "no"),
                ("no", "no"),
            ],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[
                ("Terraform · REST · MCP", "yes"),
                ("socket API", "no"),
                ("Terraform · REST · MCP", "yes"),
                ("Terraform · CLI", ""),
                ("YAML", "part"),
                ("REST API", "part"),
            ],
        },
        MatrixRow {
            label: "MCP server",
            cells: &[
                ("yes", "yes"),
                ("no", "no"),
                ("yes", ""),
                ("yes", ""),
                ("no", "no"),
                ("no", "no"),
            ],
        },
        MatrixRow {
            label: "teams / RBAC",
            cells: &[
                ("orgs + roles", "yes"),
                ("single user", "no"),
                ("workspaces", ""),
                ("projects + SSO", "yes"),
                ("basic auth", "no"),
                ("roles", ""),
            ],
        },
        MatrixRow {
            label: "tech stack",
            cells: &[
                ("Rust", ""),
                ("Node.js", ""),
                ("TypeScript + Go", ""),
                ("TypeScript", ""),
                ("Go", ""),
                ("SvelteKit", ""),
            ],
        },
        MatrixRow {
            label: "deploy footprint",
            cells: &[
                ("1 binary + 2 DBs", ""),
                ("1 container", "yes"),
                ("multi-service", "part"),
                ("6-14 services", "no"),
                ("1 tiny binary", "yes"),
                ("Node + Redis", ""),
            ],
        },
        MatrixRow {
            label: "license",
            cells: &[
                ("AGPL-3.0", ""),
                ("MIT", ""),
                ("AGPL-3.0", ""),
                ("Apache-2.0", ""),
                ("Apache-2.0", ""),
                ("MIT", ""),
            ],
        },
    ],
    notes: &[
        "Fastest interval each tool can reach; hosted free tiers are usually slower. Uptimepage's self-hosted floor is 10s, and hosted plans run at 60s on the free founding plan (with 50 monitors) or 30s on Team.",
        "OpenStatus lists ICMP, UDP and SSL-certificate monitors in its config, but its open-source Go checker implements only HTTP, TCP and DNS.",
        "Uptime Kuma has 31 monitor types and 94 alert integrations, but it is single-user, is configured over a socket API rather than REST or Terraform, and its status pages offer RSS, not email or webhook subscribers.",
        "Gatus is a health dashboard with badges rather than a subscriber status page, and its multi-region support is an experimental status-federation feature, not distributed probes.",
        "Alert-channel counts mix first-class and niche providers: Uptime Kuma's total includes the Apprise meta-provider and dozens of SMS gateways, and Gatus's includes automation bridges like Zapier, IFTTT and n8n. Uptimepage's fourteen are native integrations.",
        "Facts verified against each project's source in July 2026 (Uptime Kuma 2.4.0, OpenStatus, OneUptime 11.0.12, Gatus 5.36.0, Kener 4.1.1). Open-source projects move quickly, so check their current source before you decide.",
    ],
};

/// Head-to-head facts for `/vs/uptime-kuma`, verified in July 2026 against
/// Uptime Kuma 2.4.0 source. Cells are `(text, tone)`; column one is Uptimepage.
static UPTIME_KUMA_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &["Uptimepage", "Uptime Kuma"],
    rows: &[
        MatrixRow {
            label: "fastest check interval",
            cells: &[("60s hosted · 10s self", ""), ("1s", "yes")],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
                ("31 types", "yes"),
            ],
        },
        MatrixRow {
            label: "ping / ICMP",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "push / heartbeat monitor",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "TLS + domain expiry",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "branded status page",
            cells: &[("yes", "yes"), ("basic", "part")],
        },
        MatrixRow {
            label: "status-page subscribers",
            cells: &[("email + webhook", "yes"), ("RSS only", "part")],
        },
        MatrixRow {
            label: "auto incidents from checks",
            cells: &[("yes", "yes"), ("manual", "no")],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[("5 hosted regions", "yes"), ("add-on", "part")],
        },
        MatrixRow {
            label: "alert channels",
            cells: &[("14 native", ""), ("94", "yes")],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[("Terraform · REST · MCP", "yes"), ("socket API", "no")],
        },
        MatrixRow {
            label: "teams / RBAC",
            cells: &[("orgs + roles", "yes"), ("single user", "no")],
        },
        MatrixRow {
            label: "how you run it",
            cells: &[("hosted or self-host", "yes"), ("1 container", "yes")],
        },
        MatrixRow {
            label: "license",
            cells: &[("AGPL-3.0", ""), ("MIT", "")],
        },
    ],
    notes: &[
        "Uptime Kuma is single-user with one shared login and is driven over an internal socket API rather than a REST or Terraform surface; its status pages offer an RSS feed, not email or webhook subscribers.",
        "Its 94 integrations include the Apprise meta-provider and many SMS gateways; Uptimepage's 14 are native. Kuma's 2.x line checks as often as every second, faster than Uptimepage's floor.",
        "Verified against Uptime Kuma 2.4.0 source in July 2026. Open-source projects move quickly, so check the current source before you decide.",
    ],
};

/// Head-to-head facts for `/vs/oneuptime`, verified in July 2026 against
/// OneUptime 11.0.12 source. Cells are `(text, tone)`; column one is Uptimepage.
static ONEUPTIME_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &["Uptimepage", "OneUptime"],
    rows: &[
        MatrixRow {
            label: "fastest check interval",
            cells: &[("60s hosted · 10s self", "yes"), ("60s", "")],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
                ("25+ types", "yes"),
            ],
        },
        MatrixRow {
            label: "ping / ICMP",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "push / heartbeat monitor",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "TLS + domain expiry",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "branded status page",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "status-page subscribers",
            cells: &[("email + webhook", "yes"), ("email · SMS · Slack", "yes")],
        },
        MatrixRow {
            label: "auto incidents from checks",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "on-call & escalation",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[("5 hosted regions", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[("Terraform · REST · MCP", "yes"), ("Terraform · CLI", "")],
        },
        MatrixRow {
            label: "tech stack",
            cells: &[("Rust", ""), ("TypeScript", "")],
        },
        MatrixRow {
            label: "deploy footprint",
            cells: &[("1 binary + 2 DBs", "yes"), ("6-14 services", "no")],
        },
        MatrixRow {
            label: "license",
            cells: &[("AGPL-3.0", ""), ("Apache-2.0", "")],
        },
    ],
    notes: &[
        "OneUptime is a broad incident platform (monitoring, status pages, on-call, logs and tracing) that runs as 6-14 services with Postgres, ClickHouse and Redis; Uptimepage is a single binary with Postgres and ClickHouse.",
        "OneUptime covers more check types than Uptimepage does; Uptimepage runs a tighter footprint and a faster self-hosted interval.",
        "Verified against OneUptime 11.0.12 source in July 2026. Fast-moving project, so check the current source before you decide.",
    ],
};

/// Head-to-head facts for `/vs/uptimerobot`, verified against
/// uptimerobot.com/pricing in July 2026. Column one is Uptimepage.
static UPTIMEROBOT_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &["Uptimepage", "UptimeRobot"],
    rows: &[
        MatrixRow {
            label: "fastest check interval",
            cells: &[
                ("60s hosted · 10s self", "yes"),
                ("5 min free · 60s paid", "part"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
                ("HTTP · TCP · ping · keyword", ""),
            ],
        },
        MatrixRow {
            label: "ping / ICMP",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "push / heartbeat monitor",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "TLS + domain expiry",
            cells: &[("yes", "yes"), ("paid only", "part")],
        },
        MatrixRow {
            label: "branded status page",
            cells: &[("yes", "yes"), ("basic free · full paid", "part")],
        },
        MatrixRow {
            label: "status-page subscribers",
            cells: &[("email + webhook", "yes"), ("email", "part")],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[("5 hosted regions", "yes"), ("1 free · 4 paid", "part")],
        },
        MatrixRow {
            label: "alert channels",
            cells: &[
                ("Slack · Telegram · PagerDuty · SMS · webhook", "yes"),
                ("email/SMS free · webhook + PagerDuty on Team", "part"),
            ],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[
                ("Terraform · REST · MCP", "yes"),
                ("REST · Terraform · MCP", "part"),
            ],
        },
        MatrixRow {
            label: "self-host (open source)",
            cells: &[("yes · AGPL", "yes"), ("no", "no")],
        },
        MatrixRow {
            label: "free tier",
            cells: &[("free, no card", "yes"), ("50 monitors", "yes")],
        },
        MatrixRow {
            label: "price to start",
            cells: &[("$0", "yes"), ("$0", "yes")],
        },
    ],
    notes: &[
        "UptimeRobot's free plan covers 50 monitors from a single region at 5-minute checks, with email and SMS alerts and basic integrations. Multi-region probes, 60-second checks, DNS, UDP and API checks, SSL and domain-expiry monitoring, Slack, webhook and PagerDuty alerts, full-featured status pages and team seats are all paid-tier features.",
        "UptimeRobot is a hosted service, not open-source or self-hostable. Its Terraform provider ships from its own GitHub organization though it carries the registry's community badge, and it added a hosted MCP server.",
        "Verified against uptimerobot.com/pricing in July 2026. SaaS plans change, so check their current pricing before you decide.",
    ],
};

/// Head-to-head facts for `/vs/better-stack`, verified against
/// betterstack.com/pricing in July 2026. Column one is Uptimepage.
static BETTER_STACK_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &["Uptimepage", "Better Stack"],
    rows: &[
        MatrixRow {
            label: "fastest check interval",
            cells: &[
                ("60s hosted · 10s self", "yes"),
                ("3 min free · 30s paid", "part"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
                ("HTTP · TCP · UDP · DNS · mail · ping", "yes"),
            ],
        },
        MatrixRow {
            label: "ping / ICMP",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "push / heartbeat monitor",
            cells: &[("yes", "yes"), ("1s heartbeat", "yes")],
        },
        MatrixRow {
            label: "TLS + domain expiry",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "branded status page",
            cells: &[("yes", "yes"), ("white-label", "yes")],
        },
        MatrixRow {
            label: "status-page subscribers",
            cells: &[("email + webhook", "yes"), ("1,000 included", "yes")],
        },
        MatrixRow {
            label: "on-call & escalation",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[("5 hosted regions", "yes"), ("4 regions", "yes")],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[
                ("Terraform · REST · MCP", "yes"),
                ("Terraform · REST", "yes"),
            ],
        },
        MatrixRow {
            label: "teams / RBAC + SSO",
            cells: &[("orgs + roles", "yes"), ("RBAC + SSO", "yes")],
        },
        MatrixRow {
            label: "self-host (open source)",
            cells: &[("yes · AGPL", "yes"), ("no", "no")],
        },
        MatrixRow {
            label: "price to start",
            cells: &[("free, no card", "yes"), ("free 10 · paid $29/mo", "")],
        },
    ],
    notes: &[
        "Better Stack's free plan covers 10 monitors at 3-minute checks with 1 status page; 30-second checks and other paid features start around $29/month. It is a hosted service, not open-source or self-hostable.",
        "Better Stack takes heartbeats as often as every second and covers more check types than Uptimepage does. Uptimepage is AGPL and self-hostable, adds an MCP server, and starts free with no card.",
        "Verified against betterstack.com/pricing in July 2026. SaaS plans change, so check their current pricing before you decide.",
    ],
};

/// Head-to-head facts for `/vs/pingdom`, verified against pingdom.com and its
/// docs in July 2026. Column one is Uptimepage.
static PINGDOM_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &["Uptimepage", "Pingdom"],
    rows: &[
        MatrixRow {
            label: "fastest check interval",
            cells: &[("60s hosted · 10s self", "yes"), ("60s", "")],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
                ("HTTP · TCP · UDP · DNS · ping · mail", "yes"),
            ],
        },
        MatrixRow {
            label: "ping / ICMP",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "transaction / real-user monitoring",
            cells: &[("browser flows, no RUM", "part"), ("yes", "yes")],
        },
        MatrixRow {
            label: "TLS + domain expiry",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "branded status page",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "auto incidents from checks",
            cells: &[("yes", "yes"), ("alerts", "part")],
        },
        MatrixRow {
            label: "check locations",
            cells: &[("yes", "yes"), ("100+ locations", "yes")],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[("Terraform · REST · MCP", "yes"), ("REST API", "part")],
        },
        MatrixRow {
            label: "self-host (open source)",
            cells: &[("yes · AGPL", "yes"), ("no", "no")],
        },
        MatrixRow {
            label: "free tier",
            cells: &[("free, no card", "yes"), ("trial only", "no")],
        },
    ],
    notes: &[
        "Pingdom (by SolarWinds) has no free tier, only a 30-day trial; Uptimepage starts free with no card. Pingdom is a hosted service, not open-source or self-hostable.",
        "Pingdom checks from 100+ locations at a 1-minute minimum interval and adds transaction and real-user monitoring that Uptimepage does not have. Uptimepage pairs uptime with a status page in one binary, is AGPL, and adds a Terraform provider and MCP server.",
        "Verified against pingdom.com and its docs in July 2026. SaaS plans change, so check their current pricing before you decide.",
    ],
};

/// Head-to-head facts for `/vs/statuspage`, verified against
/// atlassian.com/software/statuspage/pricing in July 2026. Column one is
/// Uptimepage. Statuspage does not monitor — it is a status page only.
static STATUSPAGE_MATRIX: Matrix = Matrix {
    heading: "How they compare",
    columns: &["Uptimepage", "Statuspage"],
    rows: &[
        MatrixRow {
            label: "built-in uptime monitoring",
            cells: &[("yes", "yes"), ("no — needs 3rd-party", "no")],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
                ("none native", "no"),
            ],
        },
        MatrixRow {
            label: "auto incidents from checks",
            cells: &[("yes", "yes"), ("manual / integration", "part")],
        },
        MatrixRow {
            label: "branded status page",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "custom domain",
            cells: &[("yes", "yes"), ("paid tiers", "")],
        },
        MatrixRow {
            label: "status-page subscribers",
            cells: &[
                ("email + webhook", "yes"),
                ("email · SMS · Slack · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "subscribers on free",
            cells: &[("included", "yes"), ("100", "part")],
        },
        MatrixRow {
            label: "scheduled maintenance",
            cells: &[("yes", "yes"), ("yes", "")],
        },
        MatrixRow {
            label: "config-as-code",
            cells: &[("Terraform · REST · MCP", "yes"), ("REST API", "part")],
        },
        MatrixRow {
            label: "teams / RBAC",
            cells: &[("orgs + roles", "yes"), ("RBAC on Business+", "")],
        },
        MatrixRow {
            label: "self-host (open source)",
            cells: &[("yes · AGPL", "yes"), ("no", "no")],
        },
        MatrixRow {
            label: "price to start",
            cells: &[("free, no card", "yes"), ("free · paid $29/mo", "")],
        },
    ],
    notes: &[
        "Atlassian Statuspage does not run its own checks — it is a status page only, fed by external monitors (Pingdom, Datadog, New Relic, Opsgenie). Uptimepage does the monitoring and the page in one.",
        "Statuspage's free plan includes 100 subscribers; the cheapest paid plan (Hobby) is $29/month with 250 subscribers and 5 team members. It is a hosted Atlassian product, not open-source or self-hostable.",
        "Verified against atlassian.com/software/statuspage/pricing in July 2026. Plans change, so check their current pricing before you decide.",
    ],
};

/// Third-party face-off for `/compare/openstatus-vs-uptime-kuma`, verified
/// July 2026 against each project's repository, docs and plan pages. The
/// Uptimepage column is last on purpose: the rivals are the subject.
static OPENSTATUS_KUMA_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["OpenStatus", "Uptime Kuma", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("AGPL-3.0", ""), ("MIT", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "run it yourself",
            cells: &[
                ("multi-service TS stack", "part"),
                ("one container", "yes"),
                ("one binary + compose", "yes"),
            ],
        },
        MatrixRow {
            label: "hosted option",
            cells: &[
                ("yes, free tier", "yes"),
                ("no", "no"),
                ("yes, free tier", "yes"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("HTTP · TCP · DNS, more in schema", ""),
                ("31 incl. DBs · MQTT · browser", ""),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "fastest interval",
            cells: &[
                ("30s on hosted paid tiers", ""),
                ("1s", ""),
                ("60s free · 30s Pro · 10s self-hosted", ""),
            ],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[
                ("28 hosted regions", "yes"),
                ("via Globalping add-on", "part"),
                ("5 hosted regions", "yes"),
            ],
        },
        MatrixRow {
            label: "status page subscribers",
            cells: &[
                ("email · webhook · Slack", "yes"),
                ("RSS only", "part"),
                ("email · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "config as code",
            cells: &[
                ("Terraform · REST · CLI · MCP", "yes"),
                ("UI only, no management API", "no"),
                ("Terraform · REST · MCP", "yes"),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("members on paid tiers", "yes"),
                ("single login", "no"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~8.9k", ""), ("~89k", ""), ("young", "")],
        },
    ],
    notes: &[
        "OpenStatus's open-source checker implements HTTP, TCP and DNS; ICMP, UDP and TLS-certificate monitor types exist in its API schema.",
        "Uptime Kuma's 2.x line added a Globalping monitor type, so checks can run from other locations without a second instance; it is still not a probe fleet you control.",
        "Star counts rounded from GitHub, July 2026.",
        "Verified July 2026 against each project's repository, documentation and plan pages. Refresh when a project releases a new version.",
    ],
};

/// Third-party face-off for `/compare/uptime-kuma-vs-zabbix`, verified July
/// 2026 against Zabbix's own docs and lifecycle page. No star-count row here:
/// Zabbix develops on git.zabbix.com, so its GitHub mirror understates it.
static KUMA_ZABBIX_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Uptime Kuma", "Zabbix", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("MIT", ""), ("AGPL-3.0 since 7.0", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "what it watches",
            cells: &[
                ("services, from outside", ""),
                ("hosts, from inside", ""),
                ("services, from outside", ""),
            ],
        },
        MatrixRow {
            label: "agents required",
            cells: &[
                ("none", "yes"),
                ("one per host for the useful parts", "part"),
                ("none, optional private probe", "yes"),
            ],
        },
        MatrixRow {
            label: "what you run",
            cells: &[
                ("one container", "yes"),
                ("server + database + PHP frontend", "part"),
                ("one binary + compose", "yes"),
            ],
        },
        MatrixRow {
            label: "endpoint checks",
            cells: &[
                ("31 types incl. DBs · MQTT · browser", ""),
                ("web scenarios + simple checks", ""),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "infrastructure metrics",
            cells: &[
                ("none", "no"),
                ("CPU · memory · disk · logs · DBs", "yes"),
                ("none", "no"),
            ],
        },
        MatrixRow {
            label: "checks from elsewhere",
            cells: &[
                ("via Globalping add-on", "part"),
                ("proxies you host", "part"),
                ("5 hosted regions", "yes"),
            ],
        },
        MatrixRow {
            label: "customer status page",
            cells: &[
                ("yes, RSS only", "part"),
                ("none, dashboards instead", "no"),
                ("branded, email + webhook subs", "yes"),
            ],
        },
        MatrixRow {
            label: "incidents",
            cells: &[
                ("posted by hand", "part"),
                ("triggers + events, internal", "part"),
                ("auto-opened from checks", "yes"),
            ],
        },
        MatrixRow {
            label: "config as code",
            cells: &[
                ("UI only, no management API", "no"),
                ("JSON-RPC API, community Terraform", "part"),
                ("Terraform · REST · MCP, official", "yes"),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("single login", "no"),
                ("users · groups · roles", "yes"),
                ("orgs + roles", "yes"),
            ],
        },
    ],
    notes: &[
        "Zabbix requires MySQL 8.0.30+, MariaDB 10.5+, or PostgreSQL 13-18 (optionally with TimescaleDB), plus PHP 8.0-8.5 on Apache 2.4 or Nginx 1.20.",
        "Zabbix web scenarios assert on status codes, required strings and response time. The server must be built with cURL support, and redirects are capped at ten.",
        "No official Zabbix Terraform provider exists. The registry carries at least five community providers, maintained independently of the project.",
        "Verified July 2026 against Zabbix documentation, its license page and its lifecycle policy, and against the Uptime Kuma repository. Zabbix 7.4 reaches end of life on 30 September 2026, so recheck this table around the 8.0 release.",
    ],
};

/// Third-party face-off for `/compare/uptime-kuma-vs-gatus`, verified July
/// 2026 against both repositories. Uptimepage column last: rivals first.
static KUMA_GATUS_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Uptime Kuma", "Gatus", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("MIT", ""), ("Apache-2.0", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "configuration",
            cells: &[
                ("UI only", ""),
                ("YAML only, read-only UI", ""),
                ("UI + Terraform + REST + MCP", ""),
            ],
        },
        MatrixRow {
            label: "run it yourself",
            cells: &[
                ("one container (Node)", "yes"),
                ("tiny static binary", "yes"),
                ("one binary + compose", "yes"),
            ],
        },
        MatrixRow {
            label: "hosted option",
            cells: &[
                ("no", "no"),
                ("gatus.io, paid", "part"),
                ("yes, free tier", "yes"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("31 incl. DBs · MQTT · browser", ""),
                ("11 protocols incl. gRPC · SSH · WebSocket", ""),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "fastest interval",
            cells: &[
                ("1s", ""),
                ("no documented floor, default 60s", ""),
                ("60s free · 30s Pro · 10s self-hosted", ""),
            ],
        },
        MatrixRow {
            label: "status page",
            cells: &[
                ("yes, custom domains", "yes"),
                ("dashboard doubles as page", "part"),
                ("branded, custom domain on Pro", "yes"),
            ],
        },
        MatrixRow {
            label: "page subscribers",
            cells: &[
                ("RSS only", "part"),
                ("none", "no"),
                ("email · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "incidents",
            cells: &[
                ("posted by hand", "part"),
                ("none", "no"),
                ("auto-opened from checks", "yes"),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("single login", "no"),
                ("one basic-auth or OIDC gate", "no"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "alert channels",
            cells: &[
                ("94 services", ""),
                ("41 providers", ""),
                ("Slack · Telegram · PagerDuty · SMS + more", ""),
            ],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~89k", ""), ("~11.4k", ""), ("young", "")],
        },
    ],
    notes: &[
        "Kuma counts include the Apprise meta-provider and types implemented outside its monitor-types module; Gatus provider count from its README.",
        "Star counts rounded from GitHub, July 2026.",
        "Verified July 2026 against both repositories. Refresh when a project releases a new version.",
    ],
};

/// Third-party face-off for `/compare/uptime-kuma-vs-upptime`, verified July
/// 2026 against both repositories and Upptime's configuration docs.
static KUMA_UPPTIME_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Uptime Kuma", "Upptime", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("MIT", ""), ("MIT code · ODbL data", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "configuration",
            cells: &[
                ("UI only", ""),
                ("one YAML file in git", ""),
                ("UI + Terraform + REST + MCP", ""),
            ],
        },
        MatrixRow {
            label: "what you run",
            cells: &[
                ("one container (Node)", "yes"),
                ("no server, GitHub Actions", "yes"),
                ("one binary + compose", "yes"),
            ],
        },
        MatrixRow {
            label: "hosted option",
            cells: &[
                ("no", "no"),
                ("GitHub Pages, free", "part"),
                ("yes, free tier", "yes"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("31 incl. DBs · MQTT · browser", ""),
                ("HTTP · tcp-ping", ""),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "fastest interval",
            cells: &[
                ("1s", ""),
                ("5 min, the Actions schedule", "no"),
                ("60s free · 30s Pro · 10s self-hosted", ""),
            ],
        },
        MatrixRow {
            label: "probe locations",
            cells: &[
                ("via Globalping add-on", "part"),
                ("GitHub runners, or Globalping", "part"),
                ("5 hosted regions", "yes"),
            ],
        },
        MatrixRow {
            label: "status page",
            cells: &[
                ("yes, custom domains", "yes"),
                ("GitHub Pages + custom domain", "yes"),
                ("branded, custom domain on Pro", "yes"),
            ],
        },
        MatrixRow {
            label: "page subscribers",
            cells: &[
                ("RSS only", "part"),
                ("GitHub Issues + Slack", "part"),
                ("email · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "incidents",
            cells: &[
                ("posted by hand", "part"),
                ("auto-opened as Issues", "yes"),
                ("auto-opened from checks", "yes"),
            ],
        },
        MatrixRow {
            label: "history lives in",
            cells: &[
                ("its database", ""),
                ("the git repo", ""),
                ("Postgres + ClickHouse", ""),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("single login", "no"),
                ("GitHub repo permissions", "part"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~89k", ""), ("~17k", ""), ("young", "")],
        },
    ],
    notes: &[
        "Upptime's five-minute limit is what a GitHub Actions schedule allows. It is not a setting you can change.",
        "Upptime checks are HTTP unless `check: tcp-ping` is set, which also covers Globalping locations.",
        "Star counts rounded from GitHub, July 2026.",
        "Verified July 2026 against both repositories. Refresh when a project releases a new version.",
    ],
};

/// Third-party face-off for `/compare/uptime-kuma-vs-oneuptime`, verified July
/// 2026 against both repositories. OneUptime rows track `ONEUPTIME_MATRIX`.
static KUMA_ONEUPTIME_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Uptime Kuma", "OneUptime", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("MIT", ""), ("Apache-2.0", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "scope",
            cells: &[
                ("uptime only", ""),
                ("uptime + status + on-call + APM + logs", ""),
                ("uptime + status + incidents", ""),
            ],
        },
        MatrixRow {
            label: "configuration",
            cells: &[
                ("UI only", ""),
                ("Terraform · CLI", ""),
                ("UI + Terraform + REST + MCP", ""),
            ],
        },
        MatrixRow {
            label: "deploy footprint",
            cells: &[
                ("one container (Node)", "yes"),
                ("6-14 services", "no"),
                ("1 binary + 2 DBs", "yes"),
            ],
        },
        MatrixRow {
            label: "hosted option",
            cells: &[("no", "no"), ("yes", "yes"), ("yes, free tier", "yes")],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("31 incl. DBs · MQTT · browser", ""),
                ("25+ types", ""),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "fastest interval",
            cells: &[
                ("1s", ""),
                ("60s", ""),
                ("60s free · 30s Pro · 10s self-hosted", ""),
            ],
        },
        MatrixRow {
            label: "page subscribers",
            cells: &[
                ("RSS only", "part"),
                ("email · SMS · Slack", "yes"),
                ("email · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "on-call & escalation",
            cells: &[("none", "no"), ("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("single login", "no"),
                ("yes", "yes"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[
                ("via Globalping add-on", "part"),
                ("yes", "yes"),
                ("5 hosted regions", "yes"),
            ],
        },
        MatrixRow {
            label: "tech stack",
            cells: &[("JavaScript", ""), ("TypeScript", ""), ("Rust", "")],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~89k", ""), ("~7.3k", ""), ("young", "")],
        },
    ],
    notes: &[
        "OneUptime rows match our fuller OneUptime comparison, checked against its repository.",
        "Star counts rounded from GitHub, July 2026.",
        "Verified July 2026 against both repositories. Refresh when a project releases a new version.",
    ],
};

/// Third-party face-off for `/compare/uptime-kuma-vs-kener`, verified July 2026
/// against both repositories. Kener's check interval and whether its pages take
/// end-user subscribers are not documented, so no row claims either.
static KUMA_KENER_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Uptime Kuma", "Kener", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("MIT", ""), ("MIT", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "built for",
            cells: &[
                ("a monitoring dashboard", ""),
                ("the status page first", ""),
                ("monitoring + status page", ""),
            ],
        },
        MatrixRow {
            label: "configuration",
            cells: &[
                ("UI only", ""),
                ("UI + REST API", ""),
                ("UI + Terraform + REST + MCP", ""),
            ],
        },
        MatrixRow {
            label: "deploy footprint",
            cells: &[
                ("one container (Node)", "yes"),
                ("app + Redis", "part"),
                ("1 binary + 2 DBs", "yes"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("31 incl. DBs · MQTT · browser", ""),
                ("8 incl. SQL · heartbeat · GameDig", ""),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "alert channels",
            cells: &[
                ("94 services", ""),
                ("email · webhook · Slack · Discord", ""),
                ("Slack · Telegram · PagerDuty · SMS + more", ""),
            ],
        },
        MatrixRow {
            label: "page branding",
            cells: &[
                ("yes, custom domains", "yes"),
                ("logo · colors · CSS · themes · i18n", "yes"),
                ("branded, custom domain on Pro", "yes"),
            ],
        },
        MatrixRow {
            label: "pages per instance",
            cells: &[("many", ""), ("many", ""), ("many", "")],
        },
        MatrixRow {
            label: "REST API for monitors",
            cells: &[("none official", "no"), ("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("single login", "no"),
                ("role-based collaboration", "yes"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~89k", ""), ("~5.1k", ""), ("young", "")],
        },
    ],
    notes: &[
        "Kener's check and alert lists come from its README. Its check interval and page subscriptions are not documented, so no row claims either.",
        "Star counts rounded from GitHub, July 2026.",
        "Verified July 2026 against both repositories. Refresh when a project releases a new version.",
    ],
};

/// Third-party face-off for `/compare/pingdom-vs-statuscake`, verified July
/// 2026 against both vendors' pricing/feature pages and Pingdom's API spec.
/// Prices are geo-localized by both vendors, so rows describe shape only.
static PINGDOM_STATUSCAKE_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Pingdom", "StatusCake", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "free tier",
            cells: &[
                ("no, 30-day trial", "no"),
                ("yes, 10 monitors @ 5 min", "yes"),
                ("yes, no card", "yes"),
            ],
        },
        MatrixRow {
            label: "pricing model",
            cells: &[
                ("usage ladders per product", ""),
                ("three tiers + add-ons", ""),
                ("free · founding · Pro", ""),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("HTTP · TCP · ping · DNS · UDP · mail", ""),
                ("HTTP · TCP · DNS · SSH · SMTP · ping · push", ""),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "fastest uptime interval",
            cells: &[
                ("1 min", ""),
                ("30s on top tier", ""),
                ("60s free · 30s Pro · 10s self-hosted", ""),
            ],
        },
        MatrixRow {
            label: "browser transactions + RUM",
            cells: &[
                ("yes, core products", "yes"),
                ("page speed only, no RUM", "part"),
                ("no", "no"),
            ],
        },
        MatrixRow {
            label: "probe locations",
            cells: &[
                ("~100 locations", ""),
                ("30+ countries", ""),
                ("EU · US · Asia-Pacific", ""),
            ],
        },
        MatrixRow {
            label: "status page",
            cells: &[
                ("included", "yes"),
                ("separate paid add-on", "part"),
                ("included, branded", "yes"),
            ],
        },
        MatrixRow {
            label: "page subscribers",
            cells: &[
                ("not published", ""),
                ("email · SMS, capped per add-on tier", "part"),
                ("email · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "open source / self-host",
            cells: &[("no", "no"), ("no", "no"), ("AGPL", "yes")],
        },
        MatrixRow {
            label: "team members",
            cells: &[
                ("unlimited on all plans", "yes"),
                ("capped per tier", "part"),
                ("orgs + roles", "yes"),
            ],
        },
    ],
    notes: &[
        "Pingdom check types from its public API spec; StatusCake types from its features pages.",
        "Both vendors geo-localize prices, so tiers are described by shape rather than numbers.",
        "Verified July 2026 against both vendors' public pages. Refresh when either changes plans.",
    ],
};

/// Third-party face-off for `/compare/uptime-kuma-vs-healthchecks`, verified
/// July 2026 against Uptime Kuma 2.4.0 and Healthchecks v4.2 source.
static KUMA_HEALTHCHECKS_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Uptime Kuma", "Healthchecks", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("MIT", ""), ("BSD-3-Clause", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "how a check works",
            cells: &[
                ("it calls your service", ""),
                ("your job calls it", ""),
                ("either direction", ""),
            ],
        },
        MatrixRow {
            label: "watches a URL",
            cells: &[("yes", "yes"), ("never", "no"), ("yes", "yes")],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("31 incl. DBs · MQTT · browser", ""),
                ("inbound pings only", "no"),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "cron & scheduled jobs",
            cells: &[
                ("push monitor, interval only", "part"),
                ("cron + systemd OnCalendar, timezones", "yes"),
                ("heartbeat with period + grace, no cron schedules", "part"),
            ],
        },
        MatrixRow {
            label: "job duration, exit code, output",
            cells: &[("no", "no"), ("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "fastest granularity",
            cells: &[
                ("1s", "yes"),
                ("60s ping period", ""),
                ("60s free · 30s Pro · 10s self-hosted", ""),
            ],
        },
        MatrixRow {
            label: "status page",
            cells: &[
                ("basic", "part"),
                ("badges only", "no"),
                ("branded, custom domain on Pro", "yes"),
            ],
        },
        MatrixRow {
            label: "page subscribers",
            cells: &[
                ("RSS only", "part"),
                ("none", "no"),
                ("email · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "alert integrations",
            cells: &[
                ("94 services", "yes"),
                ("~28 incl. Signal · Apprise", ""),
                ("14 native", ""),
            ],
        },
        MatrixRow {
            label: "config as code",
            cells: &[
                ("socket API, no REST", "no"),
                ("REST API, community Terraform", "part"),
                ("Terraform · REST · MCP", "yes"),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("single login", "no"),
                ("projects + 4 roles", "yes"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "run it yourself",
            cells: &[
                ("one container (Node)", "yes"),
                ("one container (Django, SQLite)", "yes"),
                ("one binary + compose", "yes"),
            ],
        },
        MatrixRow {
            label: "hosted option",
            cells: &[
                ("no", "no"),
                ("yes, 20 checks free", "yes"),
                ("yes, free tier", "yes"),
            ],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~89k", ""), ("~10k", ""), ("young", "")],
        },
    ],
    notes: &[
        "Healthchecks never makes a request to your service; your service must make a request to it. It cannot tell you a website is down, by design.",
        "Uptime Kuma's push monitor covers the simple dead-man's-switch case. Healthchecks is alone here on cron and systemd OnCalendar schedules with timezones; a heartbeat that only knows a period cannot tell that a nightly job ran at the wrong hour.",
        "Healthchecks' Terraform provider is community-maintained, not official.",
        "Verified July 2026 against Uptime Kuma 2.4.0 and Healthchecks v4.2. Both projects move quickly, so check their current source before you decide.",
    ],
};

/// Third-party face-off for `/compare/uptime-kuma-vs-cachet`, verified July
/// 2026 against Uptime Kuma 2.4.0 and Cachet's live v3 source (`cachethq/core`,
/// whose docs lag the code).
static KUMA_CACHET_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Uptime Kuma", "Cachet", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[
                ("MIT", ""),
                ("v2 BSD-3 · v3 source-available", "part"),
                ("AGPL-3.0", ""),
            ],
        },
        MatrixRow {
            label: "newest tagged release",
            cells: &[
                ("2.4.0, May 2026", "yes"),
                ("v2.4.1, Nov 2023 · v3 untagged", "part"),
                (env!("CARGO_PKG_VERSION"), ""),
            ],
        },
        MatrixRow {
            label: "runs its own checks",
            cells: &[
                ("yes, 31 types", "yes"),
                ("basic HTTP GET", "part"),
                ("yes, 8 types", "yes"),
            ],
        },
        MatrixRow {
            label: "who schedules the check",
            cells: &[
                ("built in, down to 1s", "yes"),
                ("a cron entry you add", "no"),
                ("built in, from 60s", "yes"),
            ],
        },
        MatrixRow {
            label: "failed check opens an incident",
            cells: &[
                ("no, posted by hand", "no"),
                ("no, colours a component", "no"),
                ("yes, automatic", "yes"),
            ],
        },
        MatrixRow {
            label: "status page depth",
            cells: &[
                ("basic", "part"),
                ("components · incidents · maintenance · metrics", "yes"),
                ("branded, custom domain on Pro", "yes"),
            ],
        },
        MatrixRow {
            label: "page subscribers",
            cells: &[
                ("RSS only", "part"),
                ("email + webhook, global scope", "yes"),
                ("email + webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("single login", "no"),
                ("admin + user, no granularity", "part"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "config as code",
            cells: &[
                ("socket API, no REST", "no"),
                ("REST, scoped tokens, OpenAPI", "part"),
                ("Terraform · REST · MCP", "yes"),
            ],
        },
        MatrixRow {
            label: "run it yourself",
            cells: &[
                ("one container (Node)", "yes"),
                ("PHP + DB + queue + cron, no v3 image", "no"),
                ("one binary + compose", "yes"),
            ],
        },
        MatrixRow {
            label: "hosted option",
            cells: &[("no", "no"), ("no", "no"), ("yes, free tier", "yes")],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~89k", ""), ("~15k", ""), ("young", "")],
        },
    ],
    notes: &[
        "Cachet's newest tagged release is v2.4.1 from November 2023. The v3 rewrite ships from the dev branch, has never been tagged, and its own README says it is not yet completely ready for production use.",
        "Cachet v3 added an HTTP GET component check in mid-2026, but nothing schedules it out of the box, it is absent from the components guide, it runs from one location, and a failure colours a component rather than opening an incident or notifying a subscriber.",
        "Cachet's official Docker image repository covers v2 only and last saw a commit in 2021, so self-hosting v3 means a hand-rolled PHP and Laravel deployment with a database, a queue worker and cron.",
        "Cachet 2.x was BSD-3-Clause; the v3 branch carries a custom source-available license and declares itself proprietary in composer.json, while its README still says MIT. Read the license before you build on it.",
        "Verified July 2026 against Uptime Kuma 2.4.0 and the cachethq/core source. Both projects move quickly, so check their current source before you decide.",
    ],
};

/// Third-party face-off for `/compare/openstatus-vs-gatus`, verified July 2026
/// against Gatus 5.36.0 and the OpenStatus source (which ships untagged).
static OPENSTATUS_GATUS_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["OpenStatus", "Gatus", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("AGPL-3.0", ""), ("Apache-2.0", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "configuration",
            cells: &[
                ("YAML · CLI · Terraform · REST", "yes"),
                ("YAML only, read-only UI", ""),
                ("UI + Terraform + REST + MCP", "yes"),
            ],
        },
        MatrixRow {
            label: "run it yourself",
            cells: &[
                ("~11-app TypeScript stack", "no"),
                ("tiny static binary, no DB needed", "yes"),
                ("one binary + compose", "yes"),
            ],
        },
        MatrixRow {
            label: "check types",
            cells: &[
                ("HTTP · TCP · DNS in the OSS checker", "part"),
                ("11 protocols + domain expiry", "yes"),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "assertions",
            cells: &[
                ("status · body · headers", ""),
                (
                    "status · body JSONPath · latency · cert + domain expiry",
                    "yes",
                ),
                ("status · body · cert + domain expiry", ""),
            ],
        },
        MatrixRow {
            label: "fastest interval",
            cells: &[
                ("30s on paid tiers", ""),
                ("no documented floor, default 60s", ""),
                ("60s free · 30s Pro · 10s self-hosted", ""),
            ],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[
                ("28 hosted regions + private locations", "yes"),
                ("experimental federation only", "no"),
                ("5 hosted regions", "yes"),
            ],
        },
        MatrixRow {
            label: "status page",
            cells: &[
                ("yes, custom domains", "yes"),
                ("dashboard doubles as page", "part"),
                ("branded, custom domain on Pro", "yes"),
            ],
        },
        MatrixRow {
            label: "page subscribers",
            cells: &[
                ("email · webhook · Slack · RSS", "yes"),
                ("none", "no"),
                ("email · webhook", "yes"),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("orgs, members on paid tiers", "yes"),
                ("one basic-auth or OIDC gate", "no"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "hosted free tier",
            cells: &[
                ("1 monitor, 10-min interval", "part"),
                ("paid only, at gatus.io", "part"),
                ("free, no card", "yes"),
            ],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~8.9k", ""), ("~11.5k", ""), ("young", "")],
        },
    ],
    notes: &[
        "OpenStatus ships continuously with no tagged releases, so there is no version to pin.",
        "OpenStatus's open-source checker implements HTTP, TCP and DNS; ICMP, UDP and TLS-certificate monitor types appear in its API schema.",
        "Gatus's multi-step suites are labelled alpha and its remote-instance federation is labelled experimental by the project. Its maintainer has said in release notes that Gatus is a side project and that reviews and merges have slowed.",
        "Verified July 2026 against Gatus 5.36.0 and the OpenStatus source. Both projects move quickly, so check their current source before you decide.",
    ],
};

/// Third-party face-off for `/compare/blackbox-exporter-vs-uptime-kuma`,
/// verified July 2026 against Blackbox exporter v0.28.0 and Uptime Kuma 2.4.0.
static BLACKBOX_KUMA_MATRIX: Matrix = Matrix {
    heading: "The facts, side by side",
    columns: &["Blackbox exporter", "Uptime Kuma", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "license",
            cells: &[("Apache-2.0", ""), ("MIT", ""), ("AGPL-3.0", "")],
        },
        MatrixRow {
            label: "works on its own",
            cells: &[
                ("no, it is a component", "no"),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "what you operate",
            cells: &[
                ("exporter + Prometheus + Alertmanager + Grafana", "no"),
                ("one container", "yes"),
                ("hosted, or one binary", "yes"),
            ],
        },
        MatrixRow {
            label: "probe types",
            cells: &[
                ("http · tcp · dns · icmp · grpc · unix", "yes"),
                ("31 types", "yes"),
                (
                    "HTTP · TCP · DNS · TLS · domain · ping · heartbeat · flow",
                    "",
                ),
            ],
        },
        MatrixRow {
            label: "who schedules the check",
            cells: &[
                ("Prometheus, default 60s", "part"),
                ("built in, down to 1s", "yes"),
                ("built in, from 60s", "yes"),
            ],
        },
        MatrixRow {
            label: "alerting",
            cells: &[
                ("PromQL rules you write + Alertmanager", "no"),
                ("94 integrations", "yes"),
                ("14 native integrations", "yes"),
            ],
        },
        MatrixRow {
            label: "certificate expiry",
            cells: &[
                ("a metric, alert it yourself", "part"),
                ("a check with an alert", "yes"),
                ("a check with an alert", "yes"),
            ],
        },
        MatrixRow {
            label: "dashboard",
            cells: &[
                ("debug page only", "no"),
                ("built in", "yes"),
                ("built in", "yes"),
            ],
        },
        MatrixRow {
            label: "status page",
            cells: &[
                ("none", "no"),
                ("basic, RSS only", "part"),
                ("branded, with subscribers", "yes"),
            ],
        },
        MatrixRow {
            label: "reaches private targets",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes, with your own agent", "yes"),
            ],
        },
        MatrixRow {
            label: "multi-region probes",
            cells: &[
                ("deploy N exporters yourself", "no"),
                ("via Globalping add-on", "part"),
                ("5 hosted regions", "yes"),
            ],
        },
        MatrixRow {
            label: "teams & roles",
            cells: &[
                ("one basic-auth password", "no"),
                ("single login", "no"),
                ("orgs + roles", "yes"),
            ],
        },
        MatrixRow {
            label: "community (GitHub stars)",
            cells: &[("~5.8k", ""), ("~89k", ""), ("young", "")],
        },
    ],
    notes: &[
        "The Blackbox exporter has no scheduler. A probe runs only when Prometheus requests it, so check frequency is Prometheus's scrape interval, which defaults to one minute.",
        "Certificate expiry is exposed as the probe_ssl_earliest_cert_expiry metric rather than asserted by the probe; turning it into an alert is a PromQL rule you write.",
        "The exporter serves a small in-memory debug page listing recent probes, not a status page. Its history is lost on restart.",
        "Probers listed are those in the current release, v0.28.0. A websocket prober exists on master but is not in any released version.",
        "Verified July 2026 against Blackbox exporter v0.28.0 and Uptime Kuma 2.4.0. Both projects move quickly, so check their current source before you decide.",
    ],
};

/// Terraform Registry landscape for `/compare/terraform-providers`, verified
/// 14 July 2026 against the registry API and each provider's repository.
/// Registry `community` tier does not mean third-party: UptimeRobot and
/// OneUptime publish from their own verified orgs under a community badge.
static TERRAFORM_PROVIDER_MATRIX: Matrix = Matrix {
    heading: "The registry, not the marketing page",
    columns: &[
        "Uptimepage",
        "Pingdom",
        "StatusCake",
        "Statuspage",
        "UptimeRobot",
        "OneUptime",
    ],
    rows: &[
        MatrixRow {
            label: "provider from the vendor",
            cells: &[
                ("yes", "yes"),
                ("none", "no"),
                ("yes", "yes"),
                ("none", "no"),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "who maintains it",
            cells: &[
                ("Uptimepage", ""),
                ("community forks", "no"),
                ("StatusCake", ""),
                ("individuals", "no"),
                ("UptimeRobot", ""),
                ("OneUptime", ""),
            ],
        },
        MatrixRow {
            label: "registry tier",
            cells: &[
                ("community", ""),
                ("community", ""),
                ("partner", "yes"),
                ("community", ""),
                ("community", ""),
                ("community", ""),
            ],
        },
        MatrixRow {
            label: "newest release",
            cells: &[
                ("Jul 2026", "yes"),
                ("2020, archived", "no"),
                ("Oct 2023", "part"),
                ("2022 · 2024", "no"),
                ("Jul 2026", "yes"),
                ("Jul 2026", "yes"),
            ],
        },
        MatrixRow {
            label: "manages monitors",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("no product", "no"),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "manages status pages",
            cells: &[
                ("yes", "yes"),
                ("no", "no"),
                ("no", "no"),
                ("objects only", "part"),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
        MatrixRow {
            label: "manages alert channels",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("no", "no"),
                ("yes", "yes"),
                ("yes", "yes"),
            ],
        },
    ],
    notes: &[
        "Registry tiers are official (HashiCorp-built), partner (vendor-verified) and community (everything else). Community does not mean third-party: UptimeRobot and OneUptime both publish from their own verified GitHub organizations under a community badge, and so does Uptimepage. Ours carries the same community badge as theirs, which is exactly why we are telling you to read the repository rather than the badge.",
        "No provider exists in a SolarWinds- or Pingdom-owned namespace on the Terraform Registry. The most-downloaded community provider, russellcardullo/pingdom, is archived and self-describes as no longer maintained; last release 2020, last commit 2023. Living forks are maintained by an unrelated media company and by individuals, and none manages status pages.",
        "StatusCake's provider is partner tier and its repository is active, but it has shipped no new release since v2.2.2 in October 2023 and it carries no status-page resource, though StatusCake sells status pages.",
        "Atlassian publishes no Statuspage provider. The two community options manage components and incidents on a page you created by hand; neither creates the page itself.",
        "Better Stack, Checkly, Uptime.com, UptimeRobot and OneUptime all manage monitors and status pages in Terraform, as Uptimepage does. Grafana and Datadog manage synthetic checks and no status page, because neither sells one.",
        "Verified against the Terraform Registry and each provider's repository on 14 July 2026. Providers ship often, so check the registry before you decide.",
    ],
};

/// Uptime Kuma's Terraform story for `/compare/terraform-uptime-kuma`, verified
/// 11 August 2026 against the registry and each provider's repository. The
/// community column describes breml/uptimekuma, the most complete of the seven.
static TERRAFORM_KUMA_MATRIX: Matrix = Matrix {
    heading: "What seven community forks add up to",
    columns: &["Uptime Kuma", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "provider from the vendor",
            cells: &[("none", "no"), ("yes", "yes")],
        },
        MatrixRow {
            label: "providers on the registry",
            cells: &[("7, all community", "part"), ("one, ours", "yes")],
        },
        MatrixRow {
            label: "best community option",
            cells: &[("breml/uptimekuma v0.4.0", ""), ("not needed", "")],
        },
        MatrixRow {
            label: "monitors as code",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "status page as code",
            cells: &[("yes, community", "part"), ("yes", "yes")],
        },
        MatrixRow {
            label: "management API",
            cells: &[("none documented", "no"), ("REST, documented", "yes")],
        },
        MatrixRow {
            label: "how Terraform authenticates",
            cells: &[("admin user and password", "no"), ("scoped token", "yes")],
        },
    ],
    notes: &[
        "The Uptime Kuma project publishes no provider. The registry returns seven community ones: breml/uptimekuma, kenlee20/kuma, kenlee20/upkuapi, ehealth-co-id/uptimekuma, zahornyak/uptime-kuma-wapi, kurtmc/uptimekuma and TheodoreHerzfeld's.",
        "breml/uptimekuma is the most complete: 63 stars, v0.4.0 released 25 July 2026, commits this month, and resources for monitors, notifications, proxies, maintenance, tags, status pages and status-page incidents.",
        "Uptime Kuma documents no management API, so every provider here depends on a reverse-engineered client. breml's README states its capabilities are limited to what go-uptime-kuma-client supports.",
        "Uptime Kuma's own API keys expose metrics only, so the provider takes an account username and password instead of a scoped token.",
        "Verified 11 August 2026 against the Terraform Registry and each provider's repository. Providers ship often, so check before you decide.",
    ],
};

/// UptimeRobot's Terraform coverage for `/compare/terraform-uptimerobot`,
/// verified 11 August 2026 against the vendor's repository. It is one of the
/// better vendor providers, so the page says so before naming the gap.
static TERRAFORM_UPTIMEROBOT_MATRIX: Matrix = Matrix {
    heading: "Where the vendor provider stops",
    columns: &["UptimeRobot", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "provider from the vendor",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "latest release",
            cells: &[("v1.10.0, 22 Jul 2026", ""), ("shipping", "")],
        },
        MatrixRow {
            label: "monitors as code",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "alerting as code",
            cells: &[("alert contacts", "yes"), ("channels", "yes")],
        },
        MatrixRow {
            label: "status page as code",
            cells: &[("page and announcements", "part"), ("yes", "yes")],
        },
        MatrixRow {
            label: "components as code",
            cells: &[("no resource", "no"), ("yes", "yes")],
        },
        MatrixRow {
            label: "incidents as code",
            cells: &[("no resource", "no"), ("open from checks", "yes")],
        },
        MatrixRow {
            label: "self-host it",
            cells: &[("no", "no"), ("yes, AGPL", "yes")],
        },
    ],
    notes: &[
        "The provider lives at uptimerobot/terraform-provider-uptimerobot in UptimeRobot's own GitHub organization and publishes as uptimerobot/uptimerobot. Latest release v1.10.0 on 22 July 2026, commits this month.",
        "Its resources are monitor, monitor_group, alert_contact, integration, maintenance_window, psp and psp_announcement. There is no component, incident or subscriber resource.",
        "The registry badge reads community, which means the publisher is unverified rather than third-party. Uptimepage carries the same badge.",
        "Verified 11 August 2026 against the provider's repository. Providers ship often, so check before you decide.",
    ],
};

/// Atlassian Statuspage's Terraform options for `/compare/terraform-statuspage`,
/// verified 11 August 2026. The popular fork and the maintained fork are
/// different repositories, which is the whole point of the page.
static TERRAFORM_STATUSPAGE_MATRIX: Matrix = Matrix {
    heading: "Two forks, neither from Atlassian",
    columns: &["Statuspage", "Uptimepage"],
    rows: &[
        MatrixRow {
            label: "provider from the vendor",
            cells: &[("none", "no"), ("yes", "yes")],
        },
        MatrixRow {
            label: "who maintains it",
            cells: &[("two community forks", "no"), ("Uptimepage", "yes")],
        },
        MatrixRow {
            label: "most-starred fork",
            cells: &[("yannh, last release 2022", "no"), ("not needed", "")],
        },
        MatrixRow {
            label: "maintained fork",
            cells: &[("sbecker59 v1.1.0, Aug 2026", "part"), ("ours", "yes")],
        },
        MatrixRow {
            label: "create the page in code",
            cells: &[("no resource", "no"), ("yes", "yes")],
        },
        MatrixRow {
            label: "components and incidents",
            cells: &[("yes", "yes"), ("yes", "yes")],
        },
        MatrixRow {
            label: "monitoring included",
            cells: &[("none, bring your own", "no"), ("built in", "yes")],
        },
    ],
    notes: &[
        "Atlassian publishes atlassian/atlassian-operations at v2.0.5 for Jira Service Management operations, and no provider for Statuspage under any namespace it owns.",
        "yannh/statuspage has 52 stars, last release v0.1.12 in May 2022 and last commit in January 2025. sbecker59/statuspage has 8 stars and released v1.1.0 on 1 August 2026.",
        "The maintained fork offers component, component_group, incident, metric, metric_provider, page_access_group, page_access_user and subscriber. Neither fork has a resource that creates the page.",
        "Statuspage publishes status and does not run checks, so the monitoring behind it is a separate tool and a separate provider.",
        "Verified 11 August 2026 against the Terraform Registry and each provider's repository. Providers ship often, so check before you decide.",
    ],
};

/// MCP landscape for `/compare/mcp-servers`, verified 14 July 2026 against each
/// vendor's documentation. Hosted OAuth servers are common here now; the page
/// says so rather than implying ours is rare.
static MCP_SERVER_MATRIX: Matrix = Matrix {
    heading: "Who actually ships one",
    columns: &[
        "Uptimepage",
        "Better Stack",
        "UptimeRobot",
        "Checkly",
        "OpenStatus",
        "Pingdom",
    ],
    rows: &[
        MatrixRow {
            label: "official MCP server",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("none", "no"),
            ],
        },
        MatrixRow {
            label: "hosted endpoint",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("none", "no"),
            ],
        },
        MatrixRow {
            label: "OAuth sign-in",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("API key only", "part"),
                ("none", "no"),
            ],
        },
        MatrixRow {
            label: "writes",
            cells: &[
                ("fenced, you approve", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("scoped by key", "part"),
                ("none", "no"),
            ],
        },
        MatrixRow {
            label: "covers status pages",
            cells: &[
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("yes", "yes"),
                ("none", "no"),
            ],
        },
    ],
    notes: &[
        "Hosted, OAuth-authenticated MCP servers are common in this category as of July 2026, not a differentiator. Better Stack, UptimeRobot and Checkly all ship one, as do Datadog, Grafana Cloud, Sentry and PagerDuty in the wider observability space.",
        "StatusCake and Pingdom ship no MCP server. Atlassian's official server covers Jira, Confluence, Bitbucket and Compass, and explicitly does not cover Statuspage.",
        "OneUptime ships a hosted server with roughly 155 tools, authenticated with a project-scoped API key rather than OAuth.",
        "Uptime Kuma has no official MCP server. A dozen or so community wrappers exist, all local and pointed at an instance you run.",
        "Verified against each vendor's documentation on 14 July 2026.",
    ],
};

/// The comparison matrices, looked up by path so they stay off every other
/// [`Landing`](super::Landing).
pub(in crate::marketing) fn page_matrix(path: &str) -> Option<&'static Matrix> {
    match path {
        "/compare/openstatus-vs-uptime-kuma" => Some(&OPENSTATUS_KUMA_MATRIX),
        "/compare/uptime-kuma-vs-gatus" => Some(&KUMA_GATUS_MATRIX),
        "/compare/uptime-kuma-vs-upptime" => Some(&KUMA_UPPTIME_MATRIX),
        "/compare/uptime-kuma-vs-oneuptime" => Some(&KUMA_ONEUPTIME_MATRIX),
        "/compare/uptime-kuma-vs-kener" => Some(&KUMA_KENER_MATRIX),
        "/compare/pingdom-vs-statuscake" => Some(&PINGDOM_STATUSCAKE_MATRIX),
        "/compare/uptime-kuma-vs-healthchecks" => Some(&KUMA_HEALTHCHECKS_MATRIX),
        "/compare/terraform-providers" => Some(&TERRAFORM_PROVIDER_MATRIX),
        "/compare/terraform-uptime-kuma" => Some(&TERRAFORM_KUMA_MATRIX),
        "/compare/terraform-uptimerobot" => Some(&TERRAFORM_UPTIMEROBOT_MATRIX),
        "/compare/terraform-statuspage" => Some(&TERRAFORM_STATUSPAGE_MATRIX),
        "/compare/mcp-servers" => Some(&MCP_SERVER_MATRIX),
        "/compare/uptime-kuma-vs-cachet" => Some(&KUMA_CACHET_MATRIX),
        "/compare/openstatus-vs-gatus" => Some(&OPENSTATUS_GATUS_MATRIX),
        "/compare/blackbox-exporter-vs-uptime-kuma" => Some(&BLACKBOX_KUMA_MATRIX),
        "/compare/uptime-kuma-vs-zabbix" => Some(&KUMA_ZABBIX_MATRIX),
        "/open-source-uptime-monitoring" => Some(&OPEN_SOURCE_MONITOR_MATRIX),
        "/vs/self-hosted-status-pages" => Some(&SELF_HOSTED_MATRIX),
        "/vs/self-hosted-monitoring" => Some(&MONITORING_MATRIX),
        "/vs/uptime-kuma" => Some(&UPTIME_KUMA_MATRIX),
        "/vs/oneuptime" => Some(&ONEUPTIME_MATRIX),
        "/vs/uptimerobot" => Some(&UPTIMEROBOT_MATRIX),
        "/vs/better-stack" => Some(&BETTER_STACK_MATRIX),
        "/vs/pingdom" => Some(&PINGDOM_MATRIX),
        "/vs/statuspage" => Some(&STATUSPAGE_MATRIX),
        _ => None,
    }
}
