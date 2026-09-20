//! CI invariant: the marketing module may only reach a fixed set of leaf
//! modules, so extracting it into its own service stays a copy + delete.
//! Belt-and-braces regex scan of `src/marketing/`. A behavioural
//! counterpart (every route serves a 2xx with no DB pool) lives in
//! `marketing_no_db_test.rs`.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `crate::<module>` marketing touches. Each is stateless and travels
/// with the site on extraction: no pool, no `AppState`, no scheduler.
const ALLOWED_CRATE_MODULES: &[&str] = &[
    "marketing",
    "templates",
    "security",
    "request",
    "http_outbound",
];

const FORBIDDEN_SYMBOLS: &[&str] = &["AppState"];

fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(root).expect("read marketing dir") {
        let path = e.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The module named right after each `crate::` in a line of code.
fn crate_modules(code: &str) -> impl Iterator<Item = &str> {
    code.match_indices("crate::").map(|(at, _)| {
        let rest = &code[at + "crate::".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        &rest[..end]
    })
}

#[test]
fn marketing_module_only_reaches_portable_leaves() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/marketing");
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);
    assert!(!files.is_empty(), "no marketing source files found");

    let mut offenders: Vec<(String, String, String)> = Vec::new();
    for path in files {
        let body = fs::read_to_string(&path).expect("read marketing source");
        for (lineno, line) in body.lines().enumerate() {
            // Strip the line comment portion so prose can name modules the
            // rule forbids (this file's doc comment does).
            let code = line.split("//").next().unwrap_or(line);
            let at = || format!("{}:{}", path.display(), lineno + 1);
            for module in crate_modules(code) {
                if !ALLOWED_CRATE_MODULES.contains(&module) {
                    offenders.push((at(), line.trim().to_string(), format!("crate::{module}")));
                }
            }
            for needle in FORBIDDEN_SYMBOLS {
                if code.contains(needle) {
                    offenders.push((at(), line.trim().to_string(), needle.to_string()));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "marketing module may only reach {ALLOWED_CRATE_MODULES:?}:\n{}",
        offenders
            .iter()
            .map(|(at, line, what)| format!("  {at} ({what})\n    {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn crate_modules_reads_the_segment_after_crate() {
    let found: Vec<&str> =
        crate_modules("use crate::security::{SsrfGuard}; crate::marketing::x").collect();
    assert_eq!(found, ["security", "marketing"]);
    assert_eq!(crate_modules("let x = 1;").count(), 0);
}
