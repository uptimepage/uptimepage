use super::model::{ConfigPane, Figure, MockRow, PickCard, Section};

const FLOW_FIGURES: &[Figure] = &[
    Figure {
        mount: "mk-embed-flow-gap",
        script: "js/marketing/flow_gap.js",
        heading: "A 200 is not a login",
        caption: "The same minute, two checks against the same page. The HTTP check times DNS, the TCP connect, the TLS handshake and the first byte, finds all four healthy, and stops at the status line. The flow keeps going: it fills the form, submits, and looks for the page that only exists once you are signed in. Everything here runs against the-internet.herokuapp.com, a public practice site, so you can build the same monitor and watch it work.",
    },
    Figure {
        mount: "mk-embed-flow-record",
        script: "js/marketing/flow_record.js",
        heading: "Record it once, keep the steps",
        caption: "Chrome's own Recorder exports what you did as JSON. Dropping that file in turns it into steps. The focus click before typing collapses into the fill, the submit button's icon becomes the button, and the password never survives the trip.",
    },
    Figure {
        mount: "mk-embed-flow-evidence",
        script: "js/marketing/flow_evidence.js",
        heading: "When it breaks, it says where",
        caption: "A failed step names itself, and the run hands back the page it was looking at. Usually the URL alone settles it: still on the login page means the credentials never took.",
    },
];

pub(super) fn page_figures(path: &str) -> &'static [Figure] {
    match path {
        "/browser-login-monitoring" => FLOW_FIGURES,
        _ => &[],
    }
}

/// The closing "where we fit" pitch every `/vs/` and `/compare/` page ends
/// on, printed beside the status-page mock. Ours to make, so it states no
/// competitor fact; the heading is fixed in the template.
pub(super) static FITS: &[(&str, &str)] = &[
    (
        "/vs/uptimerobot",
        "Everything on this page is one product and one account: HTTP, TCP, DNS, TLS, domain, ping, heartbeat and browser-flow checks every 60 seconds, a branded status page on your own subdomain, and incidents that open themselves when a check fails. Teammates get their own logins with roles, customers subscribe by confirmed email or signed webhook, and the whole configuration can live in Git through a Terraform provider, a REST API and an MCP server. Hosted free with no card, or self-hosted under AGPL when you would rather hold the data yourself.",
    ),
    (
        "/vs/statuspage",
        "The page is not a separate purchase here. A monitor you flip public becomes a component on a branded page at your own subdomain, a failing check opens the incident, and confirmed email and webhook subscribers hear about it without anyone writing an update by hand. Pages, components and channels are all Terraform resources, so what customers read is reviewed in the same pull request as the checks behind it. Hosted free with no card, or self-hosted under AGPL.",
    ),
    (
        "/vs/better-stack",
        "Uptimepage is deliberately smaller: uptime monitoring and a status page, with no log platform or incident suite beside them. It is one Rust binary with Postgres and ClickHouse, so self-hosting is a compose file rather than a project, and the licence is AGPL, so leaving the hosted tier is a migration instead of a rewrite. Checks run from several regions, and you can run your own probe agent for targets that never leave your network.",
    ),
    (
        "/vs/oneuptime",
        "Two jobs done properly, with no platform built around them. Checks over HTTP, TCP, DNS, TLS, domain expiry, ping, heartbeat and browser flows, organizations with roles, a Terraform provider, a REST API and an MCP server, and a branded status page where incidents open on their own and customers subscribe by confirmed email or signed webhook. One binary and two databases, up with a single command, or hosted free with no card.",
    ),
    (
        "/vs/uptime-kuma",
        "The parts a homelab never needs and a team always does: an account per teammate with roles, a status page customers can subscribe to, probes in several regions plus any you run yourself, and every monitor declarable through a Terraform provider, a REST API and an MCP server. Checks cover HTTP, TCP, DNS, TLS, domain expiry, ping, heartbeat and browser flows, and a failing one opens its own incident. Still one binary, hosted free with no card or self-hosted under AGPL.",
    ),
    (
        "/vs/pingdom",
        "The checks and the public page are one product at one price. HTTP, TCP, DNS, TLS, domain, ping, heartbeat and browser-flow checks every 60 seconds on the free tier, timings that point at the cause, incidents opened automatically, and a branded status page with confirmed email and webhook subscribers. Configuration lives in Git when you want it there, through a Terraform provider, a REST API and an MCP server. Open source under AGPL, so self-hosting is always the fallback.",
    ),
    (
        "/vs/self-hosted-status-pages",
        "Uptimepage is one binary doing both halves: checks over HTTP, TCP, DNS, TLS, domain expiry, ping, heartbeat and browser flows, and the branded page those checks feed. Incidents open automatically, subscribers get them by confirmed email or signed webhook, teammates get roles rather than a shared login, and the whole configuration is reachable as Terraform, REST and MCP. Hosted free with no card, or self-hosted under AGPL with docker compose.",
    ),
    (
        "/vs/self-hosted-monitoring",
        "Uptimepage is not the very fastest interval or the widest protocol list here, and it is honest about that. What it does is put the two halves together: real HTTP, TCP, DNS, TLS-certificate, domain-expiry, ping, cron-heartbeat and browser-flow monitoring, and a branded public status page with confirmed email and webhook subscribers, auto-opened incidents and scheduled maintenance. All of it is driven from code with a Terraform provider, a full REST API and an MCP server, isolated per organization with roles, and checked from probes you can run in any region. It runs as one binary with Postgres and ClickHouse, hosted for free or self-hosted under AGPL.",
    ),
    (
        "/compare/openstatus-vs-uptime-kuma",
        "Uptimepage sits deliberately between them: one Rust binary built for teams the way Kuma isn't, with the as-code approach OpenStatus is known for. You get HTTP, TCP, DNS, TLS, domain, ping, heartbeat and browser-flow checks, organizations with roles, a Terraform provider, a REST API and an MCP server, plus a branded status page with confirmed email and webhook subscribers and auto-opened incidents. Probes are multi-region and you can run your own. Hosted free with no card, or self-host under AGPL with docker compose.",
    ),
    (
        "/compare/uptime-kuma-vs-gatus",
        "If the YAML-versus-clicks debate ends with 'actually we need customers to see a status page and teammates to have accounts', that is the gap Uptimepage fills. Checks over HTTP, TCP, DNS, TLS, domain, ping, heartbeat and flow, configured in the UI or declared with the Terraform provider and REST API, organizations with roles, multi-region probes you can run yourself, and a branded status page with email and webhook subscribers where incidents open automatically. One binary, hosted free or AGPL self-hosted.",
    ),
    (
        "/compare/pingdom-vs-statuscake",
        "Uptimepage does not do browser transactions or RUM, and says so plainly. What it does is pair the monitoring with the status page in one product and one price: HTTP, TCP, DNS, TLS, domain, ping, heartbeat and browser-flow checks every 60 seconds on the free tier, a branded status page with confirmed email and webhook subscribers included, incidents that open automatically, and a Terraform provider, REST API and MCP server for teams who keep config in code. It is also open source under AGPL, so you can always self-host instead of being locked in.",
    ),
    (
        "/compare/uptime-kuma-vs-healthchecks",
        "Uptimepage does both directions. It probes over HTTP, TCP, DNS, TLS, domain expiry, ping and browser flows, and it takes heartbeats from jobs that have nothing to probe, with a period, a grace, a max run time, and the exit code and output of the run that failed. What it does not read is a cron expression, so if the question is whether last night's job ran at the right hour in the right timezone, Healthchecks is better at that than we are. What Uptimepage adds over both is the part neither one covers, which is the customers. A branded status page on your own subdomain, confirmed email and webhook subscribers, incidents opened automatically from failing checks, organizations with roles instead of one shared login, and a Terraform provider, REST API and MCP server. Hosted free with no card, or self-host under AGPL.",
    ),
    (
        "/compare/uptime-kuma-vs-cachet",
        "Uptimepage is that pairing collapsed into one binary. Checks over HTTP, TCP, DNS, TLS, domain expiry, ping, heartbeat and browser flows run every 60 seconds from multiple regions, a failing check opens an incident by itself, and the incident lands on a branded status page where visitors have subscribed with confirmed email or a signed webhook. No glue code, one deployment, one set of roles. Hosted free with no card, or self-host under AGPL with docker compose.",
    ),
    (
        "/compare/openstatus-vs-gatus",
        "Uptimepage takes OpenStatus's shape (teams, subscribers, Terraform, multi-region) and Gatus's operational weight (one binary you can actually run). Checks over HTTP, TCP, DNS, TLS, domain expiry, ping, heartbeat and browser flows, configured in the UI or declared with the Terraform provider, REST API and MCP server. Probes are multi-region and you can run your own. Incidents open themselves and land on a branded status page with confirmed email and webhook subscribers. Hosted free with no card, or self-host under AGPL with docker compose and no external services to rent.",
    ),
    (
        "/compare/blackbox-exporter-vs-uptime-kuma",
        "Uptimepage is a finished product like Kuma, but it checks from outside your infrastructure by default, from multiple regions, and you can still run your own probe agent inside the network for the private targets that only the exporter could reach before. On top of the checks: a branded status page with confirmed email and webhook subscribers, incidents opened automatically, organizations with roles, and a Terraform provider, REST API and MCP server, so the config stays in Git the way a Prometheus setup does. Hosted free with no card, or self-host under AGPL.",
    ),
    (
        "/compare/uptime-kuma-vs-zabbix",
        "Uptimepage does the outside-in job, from several regions by default, and you can run your own probe agent inside the network for the private targets an external checker cannot see. It checks over HTTP, TCP, DNS, TLS, domain expiry, ping, heartbeat and browser flows at 60 seconds on the free tier and 10 seconds self-hosted. The config lives in Git through a Terraform provider we publish and maintain, a REST API and an MCP server, so monitoring is reviewed like the rest of your infrastructure. On top: a branded status page with confirmed email and webhook subscribers, incidents opened automatically from failing checks, and organizations with roles instead of one shared password. It is one Rust binary. It is also AGPL-3.0, the same license Zabbix uses, so self-hosting is a real exit rather than a trial. It does not replace Zabbix for CPU graphs and capacity planning, and it is not trying to. Plenty of teams run Zabbix inside and Uptimepage outside.",
    ),
    (
        "/compare/uptime-kuma-vs-upptime",
        "You may like Upptime because its settings live in version control, but five minutes is too slow for you. Or you may like Uptime Kuma's checks, but one login is not enough. Uptimepage sits between the two. It checks every 60 seconds over HTTP, TCP, DNS, TLS, domain, ping, heartbeat and flow. You can set it up in the UI, or declare it with the Terraform provider and REST API. It has organizations with user roles, and probes in several regions that you can also run yourself. Its status page is branded, and customers can subscribe by email or webhook. Incidents open on their own. It is one Rust binary. Host it free with no card, or self-host it under AGPL.",
    ),
    (
        "/compare/uptime-kuma-vs-oneuptime",
        "Most teams that grow past Kuma do not want a full observability platform. They want the two or three things Kuma lacks: an account for each teammate, a status page customers can subscribe to, and monitoring settings kept in version control. Uptimepage adds those things and little else, on purpose. It checks every 60 seconds over HTTP, TCP, DNS, TLS, domain, ping, heartbeat and flow. It has organizations with user roles, a Terraform provider, a REST API and an MCP server. Its probes run in several regions, and you can run your own. Its status page is branded, and customers can subscribe by email or webhook. It is one binary. Host it free, or self-host it under AGPL.",
    ),
    (
        "/compare/uptime-kuma-vs-kener",
        "Kener already handles branding well, so Uptimepage's advantages here are narrow. Uptimepage is one binary, with no second service to run. Its probes check from several regions, not one. You can declare monitors with a Terraform provider, a REST API and an MCP server. And status page subscribers can follow updates by signed webhook as well as confirmed email. Host it free with no card, or self-host it under AGPL.",
    ),
    (
        "/compare/terraform-providers",
        "The Uptimepage provider covers the three things together: monitors, status pages and alert channels, against the same REST API the dashboard uses, with scoped tokens so a Terraform run gets a write-scoped credential rather than an all-or-nothing key. Declare a check, the page it appears on and the channel that gets paged, review it in a pull request, and apply. There is an MCP server on the same API if you would rather ask an assistant what is broken. Hosted free with no card, or self-host the whole thing under AGPL.",
    ),
    (
        "/compare/terraform-uptime-kuma",
        "If monitoring as code is the reason you are reading this, the provider being ours rather than a fork is the difference. Monitors, status pages, components and notification channels are all resources, against the same documented REST API the dashboard uses, and a Terraform run authenticates with a scoped, expiring token rather than an account password. There is an MCP server on that API too. Hosted free with no card, or self-host the whole thing under AGPL.",
    ),
    (
        "/compare/terraform-uptimerobot",
        "The Uptimepage provider covers the parts that stop at the page boundary elsewhere: the status page, the components that bind it to real monitors, and the notification channels, alongside the checks. Incidents open automatically from failing checks rather than being typed, and confirmed email and webhook subscribers hear about them. Scoped, expiring tokens for the Terraform run. Hosted free with no card, or self-hosted under AGPL.",
    ),
    (
        "/compare/terraform-statuspage",
        "The page is a resource. So are its components, which bind to real monitors rather than to a name you keep in step by hand, and so are the notification channels. Incidents open automatically from failing checks and reach confirmed email and webhook subscribers. Monitoring and the public page are the same binary, so there is one provider and one bill. Hosted free with no card, or self-hosted under AGPL.",
    ),
    (
        "/compare/mcp-servers",
        "Uptimepage's MCP server runs in-process at mcp.uptimepage.dev, with one-click OAuth, the same tenant isolation, scopes and rate limits as the dashboard, and writes fenced behind your approval. It covers monitors, incidents and status pages, so an assistant can tell you what is broken, how long it has been broken and what you told customers about it. It can also create monitors, and it runs each check once and shows you the result before saving anything, which is a stricter bar than most of this category sets for a write. That combination is good, and it is not rare, and both of those things are true. Hosted free with no card, or self-host it under AGPL.",
    ),
];

pub(in crate::marketing) fn page_fit(path: &str) -> Option<&'static str> {
    FITS.iter().find(|(p, _)| *p == path).map(|(_, body)| *body)
}

/// The one section on a face-off page that indicts both tools at once. It
/// reads as a callout rather than another column of prose.
pub(super) static CALLOUTS: &[(&str, Section)] = &[
    (
        "/compare/openstatus-vs-uptime-kuma",
        Section {
            heading: "The honest caveats on both",
            body: "OpenStatus self-hosted is a multi-service TypeScript stack with external database dependencies, harder to operate than Kuma's single container, and its open-source checker covers fewer protocols than its API schema advertises. Kuma's limits are structural: multi-user support and a management API have been open feature requests for years because the architecture was built for one operator with a browser.",
        },
    ),
    (
        "/compare/uptime-kuma-vs-gatus",
        Section {
            heading: "What neither gives you",
            body: "A customer-facing status page with subscribers, and a team. Kuma's status pages are real but nobody can subscribe to them, and the whole app is one shared login. Gatus's dashboard doubles as its status page: fine for an internal dashboard, not something you show customers, and its access control is one basic-auth or OIDC gate. Both check from wherever you run them, unless you set up more instances yourself or use Kuma's Globalping monitor type.",
        },
    ),
    (
        "/compare/pingdom-vs-statuscake",
        Section {
            heading: "The status page problem",
            body: "Read this before picking either for a customer-facing status page. Pingdom includes public status pages in its plans. StatusCake sells status pages as a separate product with its own tiers, capped by page count and subscriber count, billed on top of monitoring. If the status page is the point, that add-on can cost more than the monitoring beside it.",
        },
    ),
    (
        "/compare/uptime-kuma-vs-healthchecks",
        Section {
            heading: "The overlap, and where it breaks",
            body: "Kuma's push monitor handles the easy case: something should check in every N minutes, tell me when it stops. Reach for Healthchecks when the schedule itself is the thing you care about, because a push monitor understands an interval and nothing else. It does not know that your job is supposed to run at 03:00 in Europe/Helsinki, it will not tell you the run took nine minutes when it usually takes two, and it will not keep the failing job's stack trace for you. Going the other way, Healthchecks will never watch a URL. Plenty of teams run both, and that is a reasonable answer rather than a cop-out.",
        },
    ),
    (
        "/compare/openstatus-vs-gatus",
        Section {
            heading: "The honest caveats on both",
            body: "Gatus is explicitly a side project: its maintainer has said so in release notes, and reviews and merges have slowed. Its multi-step suites are labelled alpha and its remote-instance federation is labelled experimental, so treat both as such. It has no subscribers, no incident timeline and one basic-auth or OIDC gate for the whole app. OpenStatus's cost is operational: self-hosting it is a multi-service TypeScript stack of about eleven apps with external database dependencies, its hosted free tier is one monitor at ten-minute intervals, and its open-source checker implements HTTP, TCP and DNS even though ICMP, UDP and TLS-certificate types appear in its API schema. It also ships continuously with no tagged releases, so there is no version to pin.",
        },
    ),
    (
        "/compare/blackbox-exporter-vs-uptime-kuma",
        Section {
            heading: "The blind spot they share",
            body: "Neither watches itself. If your Prometheus is down, nothing probes and nobody is told. If your single Kuma container is on the host that just died, the same. Self-hosted monitoring that lives next to the thing it monitors will always miss the outage that takes both down, which is the whole argument for a probe that runs somewhere else.",
        },
    ),
    (
        "/compare/uptime-kuma-vs-zabbix",
        Section {
            heading: "The blind spot they share",
            body: "Both run inside the estate they watch. If the host running your Zabbix server dies, nothing collects and nothing alerts. If your single Kuma container sits on the machine that just went down, the same. Self-hosted monitoring that lives next to the thing it monitors will always miss the outage that takes both down, and that outage is the one your customers notice. It is the whole argument for a probe somewhere else.",
        },
    ),
    (
        "/compare/uptime-kuma-vs-upptime",
        Section {
            heading: "The limits of both",
            body: "Upptime keeps its data in the repository, so if you delete the repository the data goes too. Its checks run on GitHub's servers, not in a place you choose. Its status page shows what happened, but customers cannot subscribe to it. Uptime Kuma has different limits, and they come from how it is built. It has one shared login and no user roles. It has no official REST API to manage monitors, and no Terraform provider. And it checks from the server where you installed it, unless you add its Globalping monitor type, which borrows community-hosted probes you do not control.",
        },
    ),
    (
        "/compare/uptime-kuma-vs-oneuptime",
        Section {
            heading: "The limits of both",
            body: "OneUptime's size is also its price. It runs as many services, and it publishes sizing guides because you need them. Choosing it changes your whole setup, so it is harder to leave than a single monitor. Uptime Kuma has the usual limits. It has one shared login and no user roles. It has no official REST API to manage monitors, and no Terraform provider. And it sees your service from the server where you installed it, unless you add its Globalping monitor type, which borrows community-hosted probes you do not control.",
        },
    ),
    (
        "/compare/uptime-kuma-vs-kener",
        Section {
            heading: "The limits of both",
            body: "Kener's official compose setup runs Redis next to the app, so you run two parts, not one. Its check list is short, so an unusual protocol may be missing. Uptime Kuma's limits come from how it is built. It has one shared login and no user roles. It has no official REST API to manage monitors, and no Terraform provider. Its status pages offer RSS only, with no email or webhook subscribers, and it checks from the server where you installed it, unless you add its Globalping monitor type.",
        },
    ),
];

pub(in crate::marketing) fn page_callout(path: &str) -> Option<&'static Section> {
    CALLOUTS.iter().find(|(p, _)| *p == path).map(|(_, s)| s)
}

/// Reader triage above the table, in the page's own order: the tools it
/// compares first, us last. Facts about the others stay inside what the
/// page's matrix already states.
static KUMA_GATUS_PICKS: &[PickCard] = &[
    PickCard {
        label: "pick uptime kuma if",
        body: "It is your homelab, you want the widest check list and one-second intervals, and one shared login is fine.",
    },
    PickCard {
        label: "pick gatus if",
        body: "Every check belongs in Git, the dashboard is for you and your team, and nobody outside needs to see it.",
    },
    PickCard {
        label: "pick uptimepage if",
        body: "Customers need a status page they can subscribe to, teammates need their own accounts, and you still want the config in Git.",
    },
];

pub(super) fn page_picks(path: &str) -> &'static [PickCard] {
    match path {
        "/compare/uptime-kuma-vs-gatus" => KUMA_GATUS_PICKS,
        _ => &[],
    }
}

/// The same monitor three ways, so the hero shows the difference the page
/// argues about instead of describing it. Panes are ours to demonstrate;
/// the rival's pane quotes only its documented configuration format.
static KUMA_GATUS_CONFIG: &[ConfigPane] = &[
    ConfigPane {
        id: "gatus",
        tab: "gatus.yaml",
        cmd: "cat gatus.yaml",
        tag: "YAML ONLY",
        note: "gatus has no editing UI, so every change is a redeploy",
        lines: &[
            "<span class=\"mk-conf__k\">endpoints</span>:",
            "  - <span class=\"mk-conf__k\">name</span>: <span class=\"mk-conf__s\">api</span>",
            "    <span class=\"mk-conf__k\">url</span>: <span class=\"mk-conf__s\">\"https://api.acme.dev/health\"</span>",
            "    <span class=\"mk-conf__k\">interval</span>: <span class=\"mk-conf__s\">60s</span>",
            "    <span class=\"mk-conf__k\">conditions</span>:",
            "      - <span class=\"mk-conf__s\">\"[STATUS] == 200\"</span>",
            "      - <span class=\"mk-conf__s\">\"[RESPONSE_TIME] &lt; 300\"</span>",
            "<span class=\"mk-conf__c\"># redeploy the config to apply</span>",
        ],
    },
    ConfigPane {
        id: "tf",
        tab: "uptimepage.tf",
        cmd: "cat uptimepage.tf",
        tag: "CONFIG AS CODE",
        note: "the same monitor, declared, or clicked together in the UI",
        lines: &[
            "<span class=\"mk-conf__k\">resource</span> <span class=\"mk-conf__s\">\"uptimepage_target\"</span> <span class=\"mk-conf__s\">\"api\"</span> {",
            "  <span class=\"mk-conf__k\">name</span>     = <span class=\"mk-conf__s\">\"api\"</span>",
            "  <span class=\"mk-conf__k\">interval</span> = <span class=\"mk-conf__s\">60</span>",
            "  <span class=\"mk-conf__k\">check</span>    = {",
            "    <span class=\"mk-conf__k\">type</span> = <span class=\"mk-conf__s\">\"http\"</span>",
            "    <span class=\"mk-conf__k\">http</span> = { <span class=\"mk-conf__k\">url</span> = <span class=\"mk-conf__s\">\"https://api.acme.dev/health\"</span> }",
            "  }",
            "}",
        ],
    },
    ConfigPane {
        id: "rest",
        tab: "rest",
        cmd: "history | tail -1",
        tag: "201 CREATED",
        note: "monitor live, and its first check runs straight away",
        lines: &[
            "<span class=\"mk-conf__k\">curl</span> -X POST https://app.uptimepage.dev/api/v1/targets \\",
            "  -H <span class=\"mk-conf__s\">\"Authorization: Bearer $UPTIMEPAGE_TOKEN\"</span> \\",
            "  -d <span class=\"mk-conf__s\">'{\"name\":\"api\",\"interval\":60,</span>",
            "      <span class=\"mk-conf__s\">\"check\":{\"type\":\"http\",</span>",
            "        <span class=\"mk-conf__s\">\"http\":{\"url\":\"https://api.acme.dev/health\"}}}'</span>",
        ],
    },
];

pub(super) fn page_config(path: &str) -> &'static [ConfigPane] {
    match path {
        "/compare/uptime-kuma-vs-gatus" => KUMA_GATUS_CONFIG,
        _ => &[],
    }
}

/// Illustration, not data: a made-up page showing what the checks on a
/// comparison page's left-hand column end up producing.
pub(super) static MOCK_ROWS: &[MockRow] = &[
    MockRow {
        name: "api",
        uptime: "99.98%",
        note: "p95 142ms · one incident in 45 days",
        days: "uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuxuuuuuuuuuuuuu",
    },
    MockRow {
        name: "dashboard",
        uptime: "99.91%",
        note: "p95 318ms · two degraded days",
        days: "uuuuuuuuuuuudduuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu",
    },
    MockRow {
        name: "webhooks",
        uptime: "99.99%",
        note: "p95 88ms · no incidents",
        days: "uuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu",
    },
];
