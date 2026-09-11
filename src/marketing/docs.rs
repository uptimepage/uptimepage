//! Product documentation on the marketing host: a `/docs` index plus one
//! page per Markdown source under `docs/`. Same slice-driven shape as
//! [`super::legal`] and [`super::landings`] — the [`DOCS`] table feeds the
//! route lookup, the render cache, the sidebar, and the sitemap, so a new
//! page is one entry plus one file.
//!
//! Sources are pulled in with `include_str!`, which registers a compiler
//! dependency per file — unlike the blog's `include_dir!`, an edited page
//! cannot survive into a warm rebuild.
//!
//! The renderer sanitises. Docs take third-party pull requests exactly
//! like the blog, so the trusted legal renderer must not be reused here;
//! the allowlist is only widened for what this module's own transforms
//! emit: heading ids, and the highlighter's token classes.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use askama::Template;
use askama_web::WebTemplate;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::get;

use super::config::{BRAND, MarketingCfg};
use super::md::TocEntry;
use super::pages::{CachedRender, cached_render, not_found_page, serve_cached};
use super::seo::{
    JsonLd, OpenGraph, json_ld_breadcrumb, json_ld_breadcrumb_trail, json_ld_tech_article,
};
use crate::web::filters;

/// A deploy can change the sidebar on every page at once, so an
/// already-visited page serves its old nav until this lapses.
const DOCS_CACHE_CONTROL: HeaderValue =
    HeaderValue::from_static("public, max-age=3600, stale-while-revalidate=86400");

pub const DOCS_INDEX_PATH: &str = "/docs";

/// Which readers a page is written for. Purely an orientation signal —
/// nothing here is gated, and self-hosters see the hosted pages just as
/// hosted users see the deployment ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Everyone,
    SelfHosting,
    Hosted,
}

impl Scope {
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Scope::Everyone => None,
            Scope::SelfHosting => Some("self-hosting"),
            Scope::Hosted => Some("hosted"),
        }
    }
}

/// Sidebar grouping, rendered in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Start,
    Guide,
    Reference,
    SelfHosting,
    Hosted,
}

/// Reading order, hosted before self-hosting: most readers are on the
/// hosted service, and a self-hoster looking for their section finds it
/// either way.
pub const SECTIONS: &[Section] = &[
    Section::Start,
    Section::Guide,
    Section::Reference,
    Section::Hosted,
    Section::SelfHosting,
];

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Section::Start => "start here",
            Section::Guide => "using uptimepage",
            Section::Reference => "reference",
            Section::SelfHosting => "self-hosting",
            Section::Hosted => "hosted service",
        }
    }

    pub fn blurb(self) -> &'static str {
        match self {
            Section::Start => "What the service is and how the pieces fit together.",
            Section::Guide => "Day-to-day work: monitors, incidents, status pages, alerts.",
            Section::Reference => "The API, Terraform, MCP, and the limits everything runs under.",
            Section::SelfHosting => "Running your own instance: deploy, configure, operate, debug.",
            Section::Hosted => "What differs when we run it for you at uptimepage.dev.",
        }
    }
}

pub struct DocPage {
    /// Path under `/docs`, mirroring the source layout under `docs/`.
    pub slug: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub blurb: &'static str,
    pub section: Section,
    pub scope: Scope,
    pub lastmod: &'static str,
    source: &'static str,
    /// The source file's directory under `docs/`, so relative links in it
    /// resolve the way they do on disk.
    dir: &'static str,
}

impl DocPage {
    pub fn path(&self) -> String {
        format!("{DOCS_INDEX_PATH}/{}", self.slug)
    }

    /// Source Markdown, pre-render — inlined verbatim into `llms-full.txt`.
    pub fn body_md(&self) -> &'static str {
        self.source
    }
}

/// Single source of truth for the documentation. Order within a section is
/// the order readers see.
pub const DOCS: &[DocPage] = &[
    DocPage {
        slug: "getting-started",
        title: "Getting started",
        description: "Sign in with GitHub or Google, add your first monitor, pick an interval and regions, route an alert to Slack or email, and publish a status page in ten minutes.",
        blurb: "Sign in, add a monitor, get alerted, and publish a status page in about ten minutes.",
        section: Section::Start,
        scope: Scope::Everyone,
        lastmod: "2026-09-08",
        source: include_str!("../../docs/getting-started.md"),
        dir: "",
    },
    DocPage {
        slug: "overview",
        title: "Overview",
        description: "What uptimepage is: one Rust binary that runs uptime checks, keeps config in Postgres and results in ClickHouse, and serves the app, the API and status pages.",
        blurb: "What uptimepage is, what it runs on, and where to start reading.",
        section: Section::Start,
        scope: Scope::Everyone,
        lastmod: "2026-08-27",
        source: include_str!("../../docs/overview.md"),
        dir: "",
    },
    DocPage {
        slug: "architecture",
        title: "Architecture",
        description: "How a check and a request move through uptimepage: scheduler, executor, ingest, the Postgres and ClickHouse stores, probe agents, and the invariants each keeps.",
        blurb: "How checks are scheduled, executed, batched, and stored across Postgres and ClickHouse.",
        section: Section::Start,
        scope: Scope::Everyone,
        lastmod: "2026-08-16",
        source: include_str!("../../docs/architecture.md"),
        dir: "",
    },
    DocPage {
        slug: "ui",
        title: "Web UI",
        description: "A tour of the app: the dashboard cards and monitor table, the monitor detail view with its charts and timeline, incidents, settings, and sharing one monitor.",
        blurb: "A tour of the app: dashboard, monitors, incidents, settings, and sharing a monitor.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-08",
        source: include_str!("../../docs/ui.md"),
        dir: "",
    },
    DocPage {
        slug: "incidents",
        title: "Incident management",
        description: "How a run of failing checks becomes a tracked incident: acknowledgement, ownership, on-call, escalation, public updates, and the retrospective that closes it.",
        blurb: "Acknowledgement, ownership, on-call, escalation, and the retrospective around a failing check.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-08",
        source: include_str!("../../docs/incidents.md"),
        dir: "",
    },
    DocPage {
        slug: "monitor-types",
        title: "Monitor types",
        description: "The eight check kinds, HTTP, TCP, ping, heartbeat, DNS, TLS certificate, domain expiry and browser flow, what each one really proves, and which to reach for.",
        blurb: "The eight check kinds, what each one actually proves, and which to reach for.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-08",
        source: include_str!("../../docs/monitor-types.md"),
        dir: "",
    },
    DocPage {
        slug: "notifications",
        title: "Notifications",
        description: "Alert channels for Slack, email, webhooks, PagerDuty, Telegram and more, how a monitor binds the ones that should hear about it, and what decides when it fires.",
        blurb: "Alert channels, how a monitor binds them, and what decides when an alert fires.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-08",
        source: include_str!("../../docs/notifications.md"),
        dir: "",
    },
    DocPage {
        slug: "public-status",
        title: "Public status page",
        description: "The customer-facing surface: components and their uptime, incident updates, scheduled maintenance, subscribers, badges, and the public JSON and RSS endpoints.",
        blurb: "The customer-facing surface: components, incidents, maintenance, badges, JSON and RSS.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-10",
        source: include_str!("../../docs/public-status.md"),
        dir: "",
    },
    DocPage {
        slug: "per-org-status",
        title: "Per-org status pages",
        description: "Running one or more branded status pages per organization, each on its own subdomain, showing only the monitors, incidents and maintenance you curate onto it.",
        blurb: "Running one or more branded status pages per organization, each on its own subdomain.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-10",
        source: include_str!("../../docs/per-org-status.md"),
        dir: "",
    },
    DocPage {
        slug: "share-links",
        title: "Share links",
        description: "Read-only capability URLs that open one monitor's full dashboard, with status, uptime, latency charts and incident history, for anyone you send the link to.",
        blurb: "Read-only capability URLs that open one monitor's full dashboard without an account.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-08-30",
        source: include_str!("../../docs/share-links.md"),
        dir: "",
    },
    DocPage {
        slug: "team",
        title: "Team",
        description: "Organizations, the member and owner roles and what each can do, inviting people by email, seats, and what happens to their work when someone leaves the org.",
        blurb: "Roles, inviting people, seats, and what a member can do that an owner cannot.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-05",
        source: include_str!("../../docs/team.md"),
        dir: "",
    },
    DocPage {
        slug: "organizations",
        title: "Organizations",
        description: "Running more than one organization: when a second is worth it, switching between them, what changing a slug breaks, and the 30-day window to undo a delete.",
        blurb: "Creating, switching, renaming and deleting orgs, and the restore window.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-07",
        source: include_str!("../../docs/organizations.md"),
        dir: "",
    },
    DocPage {
        slug: "variables",
        title: "Variables and secrets",
        description: "Reusable org-scoped values and write-only secrets, referenced in a monitor's HTTP request fields, so a credential lives in one place instead of every monitor.",
        blurb: "Reusable org-scoped values and write-only secrets referenced from monitor request fields.",
        section: Section::Guide,
        scope: Scope::Everyone,
        lastmod: "2026-09-06",
        source: include_str!("../../docs/variables.md"),
        dir: "",
    },
    DocPage {
        slug: "authentication",
        title: "Authentication",
        description: "OAuth sign-in, passkeys, magic links and the code beside them, managing the sign-in methods that open an account, and org-bound API tokens with scoped access.",
        blurb: "OAuth sign-in, passkeys, magic links, API tokens, and the scopes that bound them.",
        section: Section::Reference,
        scope: Scope::Everyone,
        lastmod: "2026-08-24",
        source: include_str!("../../docs/authentication.md"),
        dir: "",
    },
    DocPage {
        slug: "api",
        title: "REST API",
        description: "The /api/v1 surface: monitors, incidents, notification channels, status pages and the public endpoints, with the OpenAPI document and token authentication.",
        blurb: "The /api/v1 surface: monitors, incidents, channels, status pages, and the public endpoints.",
        section: Section::Reference,
        scope: Scope::Everyone,
        lastmod: "2026-09-11",
        source: include_str!("../../docs/api.md"),
        dir: "",
    },
    DocPage {
        slug: "terraform",
        title: "Terraform",
        description: "Managing monitors, notification channels and status pages as code with the uptimepage Terraform provider, from install and credentials to a first apply.",
        blurb: "Managing monitors, channels, and status pages as code with the official provider.",
        section: Section::Reference,
        scope: Scope::Everyone,
        lastmod: "2026-09-08",
        source: include_str!("../../docs/terraform.md"),
        dir: "",
    },
    DocPage {
        slug: "mcp",
        title: "MCP server",
        description: "Let an LLM client answer questions about one organization and take guarded actions over the Model Context Protocol, with scopes, confirmations and audit.",
        blurb: "Letting an LLM client answer operational questions and take guarded actions on one org.",
        section: Section::Reference,
        scope: Scope::Everyone,
        lastmod: "2026-09-06",
        source: include_str!("../../docs/mcp.md"),
        dir: "",
    },
    DocPage {
        slug: "quotas",
        title: "Quotas and rate limits",
        description: "How a plan bounds resources and per-minute request budgets, where each limit is enforced, and what an API client sees when it reaches the ceiling on either one.",
        blurb: "How plans bound resources and request budgets, and how each limit is enforced.",
        section: Section::Reference,
        scope: Scope::Everyone,
        lastmod: "2026-09-11",
        source: include_str!("../../docs/quotas.md"),
        dir: "",
    },
    DocPage {
        slug: "multi-tenancy",
        title: "Multi-tenancy",
        description: "The org model, how the active org is resolved from the authenticated session, and how tenant isolation is enforced in every query rather than by convention.",
        blurb: "The org model, how the active org is resolved, and how tenant isolation is enforced.",
        section: Section::Reference,
        scope: Scope::Everyone,
        lastmod: "2026-09-10",
        source: include_str!("../../docs/multi-tenancy.md"),
        dir: "",
    },
    DocPage {
        slug: "hosted/plans-and-limits",
        title: "Plans and limits",
        description: "The quotas and rate budgets each hosted plan carries, how the Standard and Founding tiers differ, and what happens when an organization reaches a ceiling.",
        blurb: "The quotas and rate budgets each hosted plan carries, and what happens at the ceiling.",
        section: Section::Hosted,
        scope: Scope::Hosted,
        lastmod: "2026-09-07",
        source: include_str!("../../docs/hosted/plans-and-limits.md"),
        dir: "hosted",
    },
    DocPage {
        slug: "hosted/regions",
        title: "Probe regions",
        description: "Where the hosted service checks from, how to pick regions for a monitor, how agreement between regions confirms an outage, and what our probes cannot reach.",
        blurb: "Where the hosted service checks from, how to pick regions for a monitor, and what our probes cannot reach.",
        section: Section::Hosted,
        scope: Scope::Hosted,
        lastmod: "2026-09-08",
        source: include_str!("../../docs/hosted/regions.md"),
        dir: "hosted",
    },
    DocPage {
        slug: "hosted/data-retention",
        title: "Data retention",
        description: "How long raw check results, rollups, incidents and audit records are kept on the hosted service, and which layer a chart reads for the range you asked for.",
        blurb: "How long raw checks, rollups, incidents, and audit records are kept on the hosted service.",
        section: Section::Hosted,
        scope: Scope::Hosted,
        lastmod: "2026-09-10",
        source: include_str!("../../docs/hosted/data-retention.md"),
        dir: "hosted",
    },
    DocPage {
        slug: "hosted/support",
        title: "Support",
        description: "How to reach us about an account, a security report, a bug or a feature request, what to include so we can help, and where the hosted service stands on SLAs.",
        blurb: "How to reach us, what to include, and where the hosted service stands on SLAs.",
        section: Section::Hosted,
        scope: Scope::Hosted,
        lastmod: "2026-08-07",
        source: include_str!("../../docs/hosted/support.md"),
        dir: "hosted",
    },
    DocPage {
        slug: "configuration",
        title: "Configuration",
        description: "Every configuration key, its default, and the environment variable that overrides it, across the server, storage, scheduler, alerting and status page sections.",
        blurb: "Every configuration key, its default, and the environment variable that overrides it.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-09-07",
        source: include_str!("../../docs/configuration.md"),
        dir: "",
    },
    DocPage {
        slug: "deployment",
        title: "Deployment",
        description: "Running the production stack behind Caddy: automatic TLS, the auth boundary, the public status surface, outbound email, database backups, and the upgrade path.",
        blurb: "Running the production stack: Caddy, TLS, the public status surface, and email.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-08-28",
        source: include_str!("../../docs/deployment.md"),
        dir: "",
    },
    DocPage {
        slug: "kubernetes",
        title: "Kubernetes",
        description: "Installing the Helm charts for the control plane and for standalone probe agents: external databases, ingress timeouts, ICMP permissions and cluster versions.",
        blurb: "Installing the Helm charts: external databases, ingress timeouts, ICMP, and probe agents.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-08-31",
        source: include_str!("../../docs/kubernetes.md"),
        dir: "",
    },
    DocPage {
        slug: "multi-region",
        title: "Multi-region probes",
        description: "Running probe agents in more than one region, how an agent pulls its config and ships results back, and the operator surface that manages regions and keys.",
        blurb: "Running probe agents in more than one region, and the operator surface that manages them.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-09-03",
        source: include_str!("../../docs/multi-region.md"),
        dir: "",
    },
    DocPage {
        slug: "metrics",
        title: "Metrics and tracing",
        description: "The Prometheus series the service exposes, what each one measures and the labels it carries, and how to ship traces off the box to an OpenTelemetry collector.",
        blurb: "The Prometheus series the service exposes and how to ship traces off the box.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-09-06",
        source: include_str!("../../docs/metrics.md"),
        dir: "",
    },
    DocPage {
        slug: "troubleshooting",
        title: "Troubleshooting",
        description: "Symptoms you are likely to hit while operating an instance, from a failing readiness probe to missing metrics and checks that never run, and what each means.",
        blurb: "Symptoms you are likely to hit while operating an instance, and what they mean.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-09-03",
        source: include_str!("../../docs/troubleshooting.md"),
        dir: "",
    },
    DocPage {
        slug: "development",
        title: "Development",
        description: "Local setup for working on the service itself: the toolchain, Postgres and ClickHouse containers, everyday workflows, and the test gates a change has to pass.",
        blurb: "Local setup for working on the service itself: toolchain, workflows, and the test gates.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-08-17",
        source: include_str!("../../docs/development.md"),
        dir: "",
    },
    DocPage {
        slug: "benchmarks",
        title: "Benchmarks",
        description: "Criterion micro-benchmarks measuring the cost of a single check through the same production HTTP path, how to run them, and how to read the numbers they print.",
        blurb: "Criterion micro-benchmarks measuring the cost of a single check through the production path.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-05-14",
        source: include_str!("../../docs/benchmarks.md"),
        dir: "",
    },
    DocPage {
        slug: "loadtest",
        title: "Load test",
        description: "The end-to-end harness that drives the real check executor against in-process mock servers, how to run it, and what its throughput and latency output tells you.",
        blurb: "The end-to-end harness that drives the real check executor against in-process mock servers.",
        section: Section::SelfHosting,
        scope: Scope::SelfHosting,
        lastmod: "2026-07-25",
        source: include_str!("../../docs/loadtest.md"),
        dir: "",
    },
];

pub fn find(slug: &str) -> Option<&'static DocPage> {
    DOCS.iter().find(|d| d.slug == slug)
}

/// The index lists every page's title and blurb, so it changes whenever any
/// of them does. Dates are ISO, so lexical max is chronological.
pub fn index_lastmod() -> Option<&'static str> {
    DOCS.iter().map(|d| d.lastmod).max()
}

/// Sanitising render. The allowlist adds ids on h2–h4, plus whatever the
/// highlighter needs. The visible sidebar keeps the h2/h3 subset; an h4 id
/// exists only as a deep-link target.
fn render(markdown: &str, dir: &str) -> (String, Vec<TocEntry>) {
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    opts.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    let parser = pulldown_cmark::Parser::new_ext(markdown, opts);
    let (events, toc) =
        super::md::anchor_headings(super::md::rewrite_doc_links(parser, dir).collect());
    let events = super::highlight::code_blocks(events.into_iter());
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, super::md::wrap_tables(events.into_iter()));
    let mut safe = ammonia::Builder::default();
    safe.link_rel(Some("noopener noreferrer"))
        .add_allowed_classes("div", &["mk-table-scroll"])
        .add_tag_attributes("div", &["tabindex"])
        .add_tag_attributes("h2", &["id"])
        .add_tag_attributes("h3", &["id"])
        .add_tag_attributes("h4", &["id"]);
    super::highlight::allow_markup(&mut safe);
    (safe.clean(&html).to_string(), toc)
}

/// A sidebar link. Resolved once so the nav is identical on every page
/// apart from which entry is current.
#[derive(Debug, Clone)]
pub struct NavItem {
    pub href: String,
    pub title: &'static str,
    pub slug: &'static str,
    pub badge: Option<&'static str>,
    pub blurb: &'static str,
}

#[derive(Debug, Clone)]
pub struct NavSection {
    pub label: &'static str,
    pub blurb: &'static str,
    pub items: Vec<NavItem>,
}

static NAV: OnceLock<Vec<NavSection>> = OnceLock::new();

fn nav() -> &'static [NavSection] {
    NAV.get_or_init(|| {
        SECTIONS
            .iter()
            .map(|section| NavSection {
                label: section.label(),
                blurb: section.blurb(),
                items: DOCS
                    .iter()
                    .filter(|d| d.section == *section)
                    .map(|d| NavItem {
                        href: d.path(),
                        title: d.title,
                        slug: d.slug,
                        badge: d.scope.badge(),
                        blurb: d.blurb,
                    })
                    .collect(),
            })
            .collect()
    })
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/docs_page.html")]
struct DocsPage {
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    article_ld: JsonLd,
    breadcrumb_ld: JsonLd,
    title: &'static str,
    badge: Option<&'static str>,
    lastmod: &'static str,
    body_html: String,
    toc: Vec<TocEntry>,
    sections: &'static [NavSection],
    current: &'static str,
    version: &'static str,
}

#[derive(Template, WebTemplate)]
#[template(path = "marketing/docs_index.html")]
struct DocsIndex {
    canonical_url: String,
    app_url: String,
    og: OpenGraph,
    breadcrumb_ld: JsonLd,
    sections: &'static [NavSection],
    current: &'static str,
    version: &'static str,
}

static RENDERED: OnceLock<HashMap<&'static str, CachedRender>> = OnceLock::new();
static SOURCES: OnceLock<HashMap<&'static str, CachedRender>> = OnceLock::new();
static INDEX_CACHED: OnceLock<CachedRender> = OnceLock::new();

/// Served verbatim under `Accept: text/markdown` — same bytes the HTML is
/// rendered from, so the two can never disagree.
fn sources() -> HashMap<&'static str, CachedRender> {
    DOCS.iter()
        .map(|doc| (doc.slug, cached_render(doc.source.to_string())))
        .collect()
}

fn render_all(cfg: &MarketingCfg) -> HashMap<&'static str, CachedRender> {
    DOCS.iter()
        .map(|doc| {
            let path = doc.path();
            let canonical_url = format!("{}{path}", cfg.canonical_origin);
            let mut og = OpenGraph::default_for(
                &format!("{} | {BRAND} docs", doc.title),
                &canonical_url,
                &cfg.canonical_origin,
            );
            og.description = doc.description.to_string();
            let (body_html, mut toc) = render(doc.source, doc.dir);
            // h4 carries an id so deep links resolve; the visible contents stay h2/h3.
            toc.retain(|t| t.level < 4);
            let page = DocsPage {
                article_ld: json_ld_tech_article(
                    &cfg.canonical_origin,
                    &path,
                    doc.title,
                    doc.description,
                    doc.lastmod,
                ),
                breadcrumb_ld: json_ld_breadcrumb_trail(
                    &cfg.canonical_origin,
                    &[("Docs", DOCS_INDEX_PATH), (doc.title, &path)],
                ),
                canonical_url,
                app_url: cfg.app_url.clone(),
                og,
                title: doc.title,
                badge: doc.scope.badge(),
                lastmod: doc.lastmod,
                body_html,
                toc,
                sections: nav(),
                current: doc.slug,
                version: env!("CARGO_PKG_VERSION"),
            };
            let body = page
                .render()
                .unwrap_or_else(|e| format!("<!-- docs render failed: {e} -->"));
            (doc.slug, cached_render(body))
        })
        .collect()
}

const INDEX_DESCRIPTION: &str = "Documentation for uptimepage: getting started, monitor types, incidents, status pages, \
     the REST API, Terraform, MCP, self-hosting and the hosted service.";

fn render_index(cfg: &MarketingCfg) -> CachedRender {
    let canonical_url = format!("{}{DOCS_INDEX_PATH}", cfg.canonical_origin);
    let mut og = OpenGraph::default_for(
        &format!("Documentation | {BRAND}"),
        &canonical_url,
        &cfg.canonical_origin,
    );
    og.description = INDEX_DESCRIPTION.to_string();
    let body = DocsIndex {
        breadcrumb_ld: json_ld_breadcrumb(&cfg.canonical_origin, "Docs", DOCS_INDEX_PATH),
        canonical_url,
        app_url: cfg.app_url.clone(),
        og,
        sections: nav(),
        current: "",
        version: env!("CARGO_PKG_VERSION"),
    }
    .render()
    .unwrap_or_else(|e| format!("<!-- docs index render failed: {e} -->"));
    cached_render(body)
}

pub(crate) fn warm(cfg: &MarketingCfg) {
    INDEX_CACHED.get_or_init(|| render_index(cfg));
    RENDERED.get_or_init(|| render_all(cfg));
    SOURCES.get_or_init(sources);
}

async fn index(State(cfg): State<Arc<MarketingCfg>>, headers: HeaderMap) -> Response {
    let cached = INDEX_CACHED.get_or_init(|| render_index(&cfg));
    serve_cached(&headers, cached, &DOCS_CACHE_CONTROL)
}

async fn page(
    State(cfg): State<Arc<MarketingCfg>>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    let cache = RENDERED.get_or_init(|| render_all(&cfg));
    let slug = slug.trim_end_matches('/');
    match cache.get(slug) {
        Some(cached) => {
            let source = SOURCES
                .get_or_init(sources)
                .get(slug)
                .expect("DOCS drives both");
            super::negotiate::serve(&headers, cached, source, &DOCS_CACHE_CONTROL)
        }
        None => not_found_page(&cfg),
    }
}

/// One wildcard route rather than a mount per entry: slugs are nested
/// (`hosted/regions`), and the lookup is the same table either way.
pub fn mount(router: Router<Arc<MarketingCfg>>) -> Router<Arc<MarketingCfg>> {
    router
        .route(DOCS_INDEX_PATH, get(index))
        .route("/docs/{*slug}", get(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Markdown file under `docs/` that is not legal copy or an
    /// internal runbook must be reachable, or it is dead weight nobody can
    /// find.
    #[test]
    fn every_source_file_is_published() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        let mut found: Vec<String> = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read docs dir") {
                let path = entry.expect("dir entry").path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if path.is_dir() {
                    if name != "legal" && name != "internal" {
                        stack.push(path);
                    }
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let rel = path.strip_prefix(&root).expect("under docs");
                    found.push(rel.with_extension("").to_string_lossy().replace('\\', "/"));
                }
            }
        }
        for slug in found {
            assert!(
                find(&slug).is_some(),
                "docs/{slug}.md is not listed in DOCS, so nothing links to it"
            );
        }
    }

    #[test]
    fn slugs_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for doc in DOCS {
            assert!(seen.insert(doc.slug), "duplicate slug {}", doc.slug);
        }
    }

    /// `SECTIONS` drives the sidebar and the index, so a page in a section
    /// missing from it renders nowhere despite being routed.
    #[test]
    fn every_section_in_use_is_listed() {
        for doc in DOCS {
            assert!(
                SECTIONS.contains(&doc.section),
                "{}: section {:?} is not in SECTIONS, so the page has no nav entry",
                doc.slug,
                doc.section
            );
        }
    }

    #[test]
    fn descriptions_fit_serp_limits() {
        let width = |s: &str| s.chars().count();
        for doc in DOCS
            .iter()
            .map(|d| (d.slug, d.description))
            .chain([("docs index", INDEX_DESCRIPTION)])
        {
            let (slug, description) = doc;
            assert!(
                (150..=160).contains(&width(description)),
                "{slug}: description is {} chars, want 150-160",
                width(description)
            );
        }
        for doc in DOCS {
            assert!(
                !doc.blurb.is_empty() && width(doc.blurb) <= 110,
                "{}: blurb is {} chars",
                doc.slug,
                width(doc.blurb)
            );
        }
    }

    /// Replaces the link checker that retired with the mdBook build: every
    /// cross-page link must resolve to a published page, and every anchor
    /// to a heading id that page actually emits.
    #[test]
    fn internal_links_and_anchors_resolve() {
        let rendered: HashMap<&str, (String, Vec<TocEntry>)> = DOCS
            .iter()
            .map(|doc| (doc.slug, render(doc.source, doc.dir)))
            .collect();
        for doc in DOCS {
            let (html, _) = &rendered[doc.slug];
            for href in hrefs(html) {
                // A bare `#anchor` stays same-page, so it resolves against
                // the linking document rather than another entry.
                let (slug, anchor) = match href.strip_prefix('#') {
                    Some(anchor) => (doc.slug, Some(anchor)),
                    None => {
                        let Some(rest) = href.strip_prefix("/docs/") else {
                            continue;
                        };
                        match rest.split_once('#') {
                            Some((s, a)) => (s, Some(a)),
                            None => (rest, None),
                        }
                    }
                };
                assert!(
                    find(slug).is_some(),
                    "{}: links to /docs/{slug}, which is not a published page",
                    doc.slug
                );
                let Some(anchor) = anchor else { continue };
                let (_, toc) = &rendered[slug];
                assert!(
                    toc.iter().any(|t| t.id == anchor),
                    "{}: links to /docs/{slug}#{anchor}, which that page has no heading for",
                    doc.slug
                );
            }
        }
    }

    fn hrefs(html: &str) -> Vec<String> {
        html.match_indices("href=\"")
            .filter_map(|(i, m)| {
                let rest = &html[i + m.len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect()
    }

    #[test]
    fn headings_keep_their_ids_through_sanitisation() {
        let (html, toc) = render("## Retry budget\n\ntext\n", "");
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].id, "retry-budget");
        assert!(html.contains("id=\"retry-budget\""), "got: {html}");
    }

    #[test]
    fn renderer_strips_script_and_handlers() {
        let html = render(
            "<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>",
            "",
        )
        .0;
        assert!(!html.contains("<script"), "got: {html}");
        assert!(!html.contains("onerror"), "got: {html}");
    }

    #[test]
    fn relative_doc_links_become_site_paths() {
        let html = render("[api](api.md) and [cfg](configuration.md#logging)", "").0;
        assert!(html.contains("href=\"/docs/api\""), "got: {html}");
        assert!(
            html.contains("href=\"/docs/configuration#logging\""),
            "got: {html}"
        );
    }

    #[test]
    fn parent_relative_links_resolve_from_the_source_directory() {
        let html = render("[api](../api.md)", "hosted").0;
        assert!(html.contains("href=\"/docs/api\""), "got: {html}");
    }
}
