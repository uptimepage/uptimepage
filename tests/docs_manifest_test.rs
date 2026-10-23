//! `docs/index.toml` is the documentation's table of contents, and the site
//! builds from it. A page the manifest misses is unreachable, and a bad entry
//! would break the site's build far from the commit that caused it.

use std::collections::HashSet;
use std::path::Path;

const SECTIONS: [&str; 5] = ["start", "guide", "reference", "hosted", "self-hosting"];
const SCOPES: [&str; 3] = ["everyone", "hosted", "self-hosting"];

fn docs_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/docs"))
}

fn pages() -> Vec<toml::Table> {
    let manifest: toml::Table = std::fs::read_to_string(docs_dir().join("index.toml"))
        .expect("read docs/index.toml")
        .parse()
        .expect("docs/index.toml parses");
    manifest["page"]
        .as_array()
        .expect("[[page]] entries")
        .iter()
        .map(|p| p.as_table().expect("[[page]] is a table").clone())
        .collect()
}

fn text<'a>(page: &'a toml::Table, key: &str) -> &'a str {
    page.get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("{page:?} needs {key}"))
}

/// Markdown under `docs/` other than the hosted service's legal copy and the
/// operations runbooks.
fn markdown_slugs() -> HashSet<String> {
    let mut found = HashSet::new();
    let mut stack = vec![docs_dir().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read docs dir") {
            let path = entry.expect("dir entry").path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "legal" && name != "internal" {
                    stack.push(path);
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let rel = path.strip_prefix(docs_dir()).expect("under docs");
                found.insert(rel.with_extension("").to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found
}

#[test]
fn every_page_is_listed_and_every_entry_has_its_page() {
    let listed: Vec<String> = pages().iter().map(|p| text(p, "slug").to_owned()).collect();
    let unique: HashSet<String> = listed.iter().cloned().collect();
    assert_eq!(unique.len(), listed.len(), "a slug is listed twice");

    let on_disk = markdown_slugs();
    let mut unlisted: Vec<_> = on_disk.difference(&unique).collect();
    unlisted.sort();
    assert!(
        unlisted.is_empty(),
        "not in docs/index.toml, so nothing links to them: {unlisted:?}"
    );
    let mut missing: Vec<_> = unique.difference(&on_disk).collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "docs/index.toml lists pages with no Markdown file: {missing:?}"
    );
}

/// Search engines cut a snippet past ~160 characters and treat a short one
/// as missing; the blurb sits on one line of the index.
#[test]
fn descriptions_and_blurbs_fit_where_they_are_shown() {
    for page in pages() {
        let slug = text(&page, "slug");
        let width = |s: &str| s.chars().count();
        let description = text(&page, "description");
        assert!(
            (150..=160).contains(&width(description)),
            "{slug}: description is {} chars, want 150-160",
            width(description)
        );
        let blurb = text(&page, "blurb");
        assert!(
            !blurb.is_empty() && width(blurb) <= 110,
            "{slug}: blurb is {} chars, want 1-110",
            width(blurb)
        );
    }
}

#[test]
fn every_page_names_a_known_section_and_scope() {
    for page in pages() {
        let slug = text(&page, "slug");
        assert!(
            SECTIONS.contains(&text(&page, "section")),
            "{slug}: section"
        );
        assert!(SCOPES.contains(&text(&page, "scope")), "{slug}: scope");
    }
}

#[test]
fn every_page_is_created_no_later_than_its_lastmod() {
    for page in pages() {
        let slug = text(&page, "slug");
        let date = |key: &str| {
            let value = page
                .get(key)
                .and_then(toml::Value::as_datetime)
                .unwrap_or_else(|| panic!("{slug}: {key} must be a date"));
            assert!(
                value.date.is_some() && value.time.is_none() && value.offset.is_none(),
                "{slug}: {key} must be a plain date such as 2026-10-23"
            );
            value.to_string()
        };
        let (created, lastmod) = (date("created"), date("lastmod"));
        assert!(
            created <= lastmod,
            "{slug}: created {created} is after lastmod {lastmod}"
        );
    }
}
