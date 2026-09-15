use crate::marketing::config::{BRAND, MarketingCfg};
use crate::marketing::seo::AUTHOR_PAGE;

use super::catalog::LANDINGS;
use super::faqs::{FAQ_PATHS, page_faqs};
use super::matrices::page_matrix;
use super::model::is_comparison;
use super::render::{ALIASES, render_all};
use super::sections::{CALLOUTS, FITS, MOCK_ROWS, page_config, page_fit};

#[test]
fn paths_are_unique_and_rooted() {
    let mut seen = std::collections::HashSet::new();
    for l in LANDINGS {
        assert!(l.path.starts_with('/'), "{} must be absolute", l.path);
        assert!(seen.insert(l.path), "duplicate path {}", l.path);
    }
}

/// A redirect to a page that no longer exists trades one 404 for a slower
/// one, and an alias that shadows a real path hides that page entirely.
#[test]
fn alias_targets_are_real_pages() {
    for (from, to) in ALIASES {
        let landing = LANDINGS.iter().any(|l| l.path == *to);
        // A draft target renders in debug but 404s in release.
        let post = to.strip_prefix("/blog/").is_some_and(|slug| {
            crate::marketing::blog::all()
                .iter()
                .any(|p| p.slug == slug && !p.draft)
        });
        assert!(landing || post, "{to} is a redirect target but not a page");
        assert!(
            !LANDINGS.iter().any(|l| l.path == *from),
            "{from} is both a landing and an alias"
        );
    }
}

/// Two pages chasing one query split its impressions and Google picks a
/// winner for you. Distinct titles and h1s are the cheapest guard.
#[test]
fn titles_and_headings_are_distinct() {
    let mut titles = std::collections::HashMap::new();
    let mut h1s = std::collections::HashMap::new();
    for l in LANDINGS {
        if let Some(other) = titles.insert(l.title, l.path) {
            panic!(
                "{} and {} share the title {:?}: they will cannibalize each other",
                other, l.path, l.title
            );
        }
        if let Some(other) = h1s.insert(l.h1, l.path) {
            panic!(
                "{} and {} share the h1 {:?}: they will cannibalize each other",
                other, l.path, l.h1
            );
        }
    }
}

/// Retiring a path means rewriting every href that pointed at it, and a
/// blind rewrite lands a page on itself or twice in one list.
#[test]
fn resource_links_are_distinct_and_outbound() {
    for l in LANDINGS {
        let mut seen = std::collections::HashSet::new();
        for r in l.resources {
            assert_ne!(r.href, l.path, "{} links to itself", l.path);
            assert!(
                seen.insert(r.href),
                "{} lists {} twice in its resources",
                l.path,
                r.href
            );
        }
    }
}

#[test]
fn every_landing_is_linked_in_footer() {
    let footer = include_str!("../../../templates/marketing/base.html");
    for l in LANDINGS {
        assert!(
            footer.contains(&format!("href=\"{}\"", l.path)),
            "{} is orphaned: add it to the marketing footer",
            l.path
        );
    }
}

#[test]
fn matrices_are_rectangular() {
    for l in LANDINGS {
        let Some(m) = page_matrix(l.path) else {
            continue;
        };
        assert!(
            m.us_col() < m.columns.len(),
            "{} matrix missing an Uptimepage column",
            l.path
        );
        for row in m.rows {
            assert_eq!(
                row.cells.len(),
                m.columns.len(),
                "{} row {:?}: {} cells but {} columns",
                l.path,
                row.label,
                row.cells.len(),
                m.columns.len(),
            );
        }
    }
}

/// The closing pitch is what every head-to-head page is written to
/// reach, and it is the block the status-page mock hangs off.
#[test]
fn comparison_pages_carry_a_fit_and_nothing_else_does() {
    for l in LANDINGS {
        assert_eq!(
            page_fit(l.path).is_some(),
            is_comparison(l.path),
            "{} fit block does not match its layout",
            l.path
        );
    }
}

/// Every optional extra is looked up by path, so a renamed page silently
/// drops its cards rather than failing to build.
#[test]
fn per_page_extras_name_real_pages() {
    let paths: std::collections::HashSet<_> = LANDINGS.iter().map(|l| l.path).collect();
    let keyed = FITS
        .iter()
        .map(|(p, _)| *p)
        .chain(CALLOUTS.iter().map(|(p, _)| *p))
        .chain(["/compare/uptime-kuma-vs-gatus"]);
    for path in keyed {
        assert!(paths.contains(path), "{path} is keyed but is not a landing");
        assert!(is_comparison(path), "{path} is not a comparison page");
    }
}

/// Shipping a check kind and forgetting the tables is the drift that let
/// heartbeats and browser flows go unmentioned for weeks while rival
/// columns counted theirs. A row that says "check types" says every kind the
/// surface it describes can actually do, which is fewer for Terraform than for
/// the API.
#[test]
fn check_type_copy_matches_the_surface_it_describes() {
    const KINDS: &[&str] = &[
        "HTTP",
        "TCP",
        "DNS",
        "TLS",
        "domain",
        "ping",
        "heartbeat",
        "flow",
    ];
    // Declarable through the REST API but not through Terraform. Empty since
    // provider v0.5.2 shipped ping and heartbeat, so the provider now covers
    // every kind: a page may name all of KINDS. Kept rather than deleted
    // because the next kind lands in the API first, and this is where it is
    // held back from the page until the provider catches up.
    const NOT_IN_PROVIDER: &[&str] = &[];
    // Keyed on the path, not on the label: prose is edited freely, so a rule
    // that reads the label can be switched off by renaming a row.
    const PROVIDER_SCOPED: &[&str] = &["/terraform-uptime-monitoring"];
    // Any wording a label can take, matched loosely: a guard that only fires on
    // the exact labels in the catalog today is bypassed by the next one added.
    const PHRASES: &[&str] = &[
        "check types",
        "check kinds",
        "what it checks",
        "what we check",
        "probe types",
        "endpoint checks",
        "check breadth",
    ];
    fn names_kinds(label: &str) -> bool {
        let label = label.to_lowercase();
        // Bare "checks" only as the whole label: as a substring it catches rows
        // like "auto incidents from checks", which name no kinds.
        label == "checks" || PHRASES.iter().any(|l| label.contains(l))
    }
    // Whole words only. Substring matching reads "ping" out of "shipping" and
    // out of a rival named Pingdom, and fails copy that claims neither.
    fn words(value: &str) -> Vec<String> {
        value
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    }
    fn assert_kinds(path: &str, label: &str, value: &str) {
        let provider = PROVIDER_SCOPED.contains(&path);
        let said = words(value);
        for kind in KINDS {
            let absent_here = provider && NOT_IN_PROVIDER.contains(kind);
            let named = said.iter().any(|w| w == &kind.to_lowercase());
            if absent_here {
                assert!(
                    !named,
                    "{path} {label:?} claims {kind}, which Terraform cannot declare: {value:?}"
                );
            } else {
                assert!(named, "{path} {label:?} omits {kind}: {value:?}");
            }
        }
    }
    for l in LANDINGS {
        // A feature row naming the kinds drifts the same way a matrix cell
        // does, and no matrix covers a use-case page.
        for f in l.features.iter().filter(|f| names_kinds(f.label)) {
            assert_kinds(l.path, f.label, f.value);
        }
        let Some(m) = page_matrix(l.path) else {
            continue;
        };
        for row in m.rows.iter().filter(|r| names_kinds(r.label)) {
            assert_kinds(l.path, row.label, row.cells[m.us_col()].0);
        }
    }
    // The claim that reached Google was in the description, not a row: prose on
    // a provider-scoped page must not promise a kind Terraform cannot declare.
    for l in LANDINGS
        .iter()
        .filter(|l| PROVIDER_SCOPED.contains(&l.path))
    {
        for (field, text) in [("meta_description", l.meta_description), ("lede", l.lede)] {
            let said = words(text);
            for kind in NOT_IN_PROVIDER {
                assert!(
                    !said.iter().any(|w| w == &kind.to_lowercase()),
                    "{} {field} promises {kind}, which Terraform cannot declare: {text:?}",
                    l.path
                );
            }
        }
    }
}

/// A head-to-head page states its facts in the matrix, in both columns.
/// A one-column summary beside it only repeats the table a scroll down.
#[test]
fn comparison_pages_state_their_facts_once() {
    for l in LANDINGS.iter().filter(|l| is_comparison(l.path)) {
        assert!(
            l.features.is_empty(),
            "{} repeats its matrix as a feature list",
            l.path
        );
    }
}

#[test]
fn config_panes_are_distinct_and_captioned() {
    for l in LANDINGS {
        let mut seen = std::collections::HashSet::new();
        for pane in page_config(l.path) {
            assert!(seen.insert(pane.id), "{} repeats pane {}", l.path, pane.id);
            assert!(
                !pane.lines.is_empty(),
                "{} pane {} is empty",
                l.path,
                pane.id
            );
        }
    }
}

/// The strip is drawn on a fixed 45-day grid, so a row of another length
/// would leave a ragged edge next to the ones beside it.
#[test]
fn mock_rows_cover_the_same_window() {
    for row in MOCK_ROWS {
        assert_eq!(row.days.chars().count(), 45, "{} strip is ragged", row.name);
        assert!(
            row.days.chars().all(|d| matches!(d, 'u' | 'd' | 'x')),
            "{} strip holds a day with no class",
            row.name
        );
    }
}

#[test]
fn every_landing_renders() {
    let cfg = MarketingCfg {
        app_url: "https://app.uptimepage.dev".into(),
        canonical_origin: "https://uptimepage.dev".into(),
        blog_enabled: false,
        mcp_url: None,
        checkout_open: false,
        trusted_proxies: Vec::new(),
    };
    for (path, page) in render_all(&cfg) {
        let html = std::str::from_utf8(&page.body).expect("landings render UTF-8");
        assert!(
            !html.contains("landing render failed"),
            "{path} failed to render"
        );
    }
}

/// FAQs are keyed by path, so renaming a page drops its answers and its
/// FAQPage JSON-LD without failing anything else.
#[test]
fn faq_keys_name_real_pages() {
    let paths: std::collections::HashSet<_> = LANDINGS.iter().map(|l| l.path).collect();
    for path in FAQ_PATHS {
        assert!(paths.contains(path), "{path} has FAQs but is not a landing");
        assert!(!page_faqs(path).is_empty(), "{path} is listed with no FAQs");
    }
    // Catches an arm added without listing it here, which would leave that
    // page's FAQs outside the rename guard above.
    let answered = LANDINGS
        .iter()
        .filter(|l| !page_faqs(l.path).is_empty())
        .count();
    assert_eq!(answered, FAQ_PATHS.len(), "FAQ_PATHS is out of date");
}

/// A head-to-head page with no FAQ loses the block the layout is built around.
#[test]
fn comparison_pages_carry_faqs() {
    for l in LANDINGS.iter().filter(|l| is_comparison(l.path)) {
        assert!(!page_faqs(l.path).is_empty(), "{} missing FAQ", l.path);
    }
}

#[test]
fn every_landing_has_seo_essentials() {
    for l in LANDINGS {
        assert!(!l.title.is_empty(), "{} missing title", l.path);
        let rendered_title = l.title.len() + " | ".len() + BRAND.len();
        assert!(
            rendered_title <= 60,
            "{} rendered <title> {} chars > 60",
            l.path,
            rendered_title
        );
        assert!(!l.h1.is_empty(), "{} missing h1", l.path);
        assert!(
            l.meta_description.len() <= 160,
            "{} meta description {} chars > 160",
            l.path,
            l.meta_description.len()
        );
        assert!(
            !l.features.is_empty() || page_matrix(l.path).is_some(),
            "{} has neither features nor a matrix",
            l.path
        );
        assert!(!l.sections.is_empty(), "{} missing sections", l.path);
    }
}

#[test]
fn only_the_author_page_carries_the_person_node() {
    let cfg = MarketingCfg {
        app_url: "https://app.uptimepage.dev".into(),
        canonical_origin: "https://uptimepage.dev".into(),
        blog_enabled: false,
        mcp_url: None,
        checkout_open: false,
        trusted_proxies: Vec::new(),
    };
    let rendered = render_all(&cfg);
    let marker = "\"@id\":\"https://uptimepage.dev/about#author\"";
    for (path, page) in &rendered {
        let html = std::str::from_utf8(&page.body).expect("landings render UTF-8");
        assert_eq!(
            html.contains(marker),
            *path == AUTHOR_PAGE,
            "{path} Person node presence does not match the author page"
        );
    }
}
