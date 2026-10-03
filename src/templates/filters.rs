//! Aggregated askama filter functions.
//!
//! askama codegens `filters::<name>(...)` at every derive site, so every
//! template-deriving module imports this module as `filters` via
//! `use crate::templates::filters;`. Group filters by concern below: a new
//! reusable filter lands in the most-specific sub-module (or a new one)
//! and is re-exported here so the derive sites don't have to know.

pub use self::display::*;
pub use self::static_refs::*;

/// Cache-busting asset URLs + AGPL-3.0 source-offer metadata.
mod static_refs {
    /// `{{ "css/app.css"|asset }}` → the cache-busting URL for that asset.
    #[askama::filter_fn]
    pub fn asset(value: &str, _: &dyn askama::Values) -> askama::Result<String> {
        Ok(crate::templates::assets::url(value))
    }

    /// AGPL-3.0 §13 source offer. `build.rs` bakes the repository URL and
    /// build commit in via `rustc-env`. `source_url` deep-links to the
    /// exact source the running binary was built from — `…/tree/<commit>`
    /// when the commit is known, the repo root otherwise. The piped value
    /// is unused — invoked as `{{ ""|source_url }}`.
    static SOURCE_URL: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| match env!("SM_SOURCE_COMMIT") {
            "" => env!("SM_SOURCE_URL").to_string(),
            commit => format!("{}/tree/{commit}", env!("SM_SOURCE_URL")),
        });

    #[askama::filter_fn]
    pub fn source_url(_: &str, _: &dyn askama::Values) -> askama::Result<&'static str> {
        Ok(SOURCE_URL.as_str())
    }

    /// Short form for display; [`source_url`] keeps the full commit.
    #[askama::filter_fn]
    pub fn source_commit(_: &str, _: &dyn askama::Values) -> askama::Result<&'static str> {
        Ok(short_commit(env!("SM_SOURCE_COMMIT")))
    }

    /// CI bakes the full `$GITHUB_SHA`; a local build already has 12. Any
    /// other override, such as a tag, prints as given.
    fn short_commit(commit: &str) -> &str {
        if commit.len() > 12 && commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            &commit[..12]
        } else {
            commit
        }
    }

    /// Product documentation. Absolute and upstream on purpose: docs are
    /// served by the marketing router on the apex host, which an app-only
    /// deployment does not run, so a relative `/docs` would land on the
    /// app's Swagger UI or 404. The input is appended, so `{{ ""|docs_url }}`
    /// is the index and `{{ "/hosted/regions"|docs_url }}` is one page.
    #[askama::filter_fn]
    pub fn docs_url(path: &str, _: &dyn askama::Values) -> askama::Result<String> {
        Ok(docs_link(path))
    }

    /// The same link off a template, for the surfaces askama does not render
    /// (mail, notifications). One owner for the base so the two cannot drift.
    pub fn docs_link(path: &str) -> String {
        format!("https://uptimepage.dev/docs{path}")
    }

    /// Crate version baked at compile time. Public build string, safe to
    /// show pre-auth — no host/tenant data. Invoked as `{{ ""|version }}`.
    #[askama::filter_fn]
    pub fn version(_: &str, _: &dyn askama::Values) -> askama::Result<&'static str> {
        Ok(env!("CARGO_PKG_VERSION"))
    }

    /// Evaluated once per process; a prod process crossing Jan 1 picks up
    /// the new year on the next deploy.
    #[askama::filter_fn]
    pub fn current_year(_: &str, _: &dyn askama::Values) -> askama::Result<i32> {
        use chrono::Datelike;
        static YEAR: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
        Ok(*YEAR.get_or_init(|| chrono::Utc::now().year()))
    }

    static SUPPORT_UI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

    /// Set once at startup from `[email].support_address`. Nav, page and
    /// endpoint ride the same flag, so a self-host install shows no dead entry.
    pub fn set_support_ui(enabled: bool) {
        let _ = SUPPORT_UI.set(enabled);
    }

    /// `{% if ""|support_ui %}` → whether the in-app help form is on.
    #[askama::filter_fn]
    pub fn support_ui(_: &str, _: &dyn askama::Values) -> askama::Result<bool> {
        Ok(*SUPPORT_UI.get().unwrap_or(&false))
    }

    static BILLING_UI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

    /// Set once at startup from whether a payment provider is configured, so
    /// a self-host install shows no billing entry.
    pub fn set_billing_ui(enabled: bool) {
        let _ = BILLING_UI.set(enabled);
    }

    /// `{% if ""|billing_ui %}` → whether the billing page is on.
    #[askama::filter_fn]
    pub fn billing_ui(_: &str, _: &dyn askama::Values) -> askama::Result<bool> {
        Ok(*BILLING_UI.get().unwrap_or(&false))
    }

    #[cfg(test)]
    mod tests {
        use super::short_commit;

        #[test]
        fn short_commit_trims_a_full_sha_and_keeps_shorter_ones() {
            assert_eq!(
                short_commit("1a84ee1a5f0c2b7d9e3a4c6b8d0f1e2a3b4c5d6e"),
                "1a84ee1a5f0c"
            );
            assert_eq!(short_commit("1a84ee1a5f0c"), "1a84ee1a5f0c");
            assert_eq!(short_commit("v0.13.0-acme.1"), "v0.13.0-acme.1");
            assert_eq!(short_commit(""), "");
        }
    }
}

/// Format raw values (timestamps, durations) by writing straight into the
/// template output buffer. The per-row hot path on the incidents table
/// uses these so a 100-row render makes zero intermediate `String`
/// allocations for time/duration columns.
mod display {
    use chrono::DateTime;
    use chrono::Utc;
    use chrono::format::{DelayedFormat, StrftimeItems};

    /// `{{ ts|iso_ts }}` → `"2026-05-13T12:00:00Z"`.
    #[askama::filter_fn]
    pub fn iso_ts(
        value: &DateTime<Utc>,
        _: &dyn askama::Values,
    ) -> askama::Result<DelayedFormat<StrftimeItems<'static>>> {
        Ok(value.format("%Y-%m-%dT%H:%M:%SZ"))
    }

    /// `{{ ts|human_ts }}` → `"2026-05-13 12:00 UTC"`.
    #[askama::filter_fn]
    pub fn human_ts(
        value: &DateTime<Utc>,
        _: &dyn askama::Values,
    ) -> askama::Result<DelayedFormat<StrftimeItems<'static>>> {
        Ok(value.format("%Y-%m-%d %H:%M UTC"))
    }

    /// `{{ secs|humanize_dur }}` → `"7m"` / `"2h 14m"` / `"1d 1h"`.
    #[askama::filter_fn]
    pub fn humanize_dur(
        value: &i64,
        _: &dyn askama::Values,
    ) -> askama::Result<crate::templates::format::HumanDur> {
        Ok(crate::templates::format::HumanDur(*value))
    }

    /// `{{ secs|exact_dur }}` → exact single-unit (`45s`, `5m`, `24h`).
    /// Unlike `humanize_dur` it never rounds, so config values round-trip.
    #[askama::filter_fn]
    pub fn exact_dur(value: &u64, _: &dyn askama::Values) -> askama::Result<String> {
        Ok(crate::templates::format::exact_duration(*value))
    }

    /// `{{ bytes|human_bytes }}` → `"512 KB"` / `"2 MB"`. Floors to the
    /// largest whole binary unit.
    #[askama::filter_fn]
    pub fn human_bytes(value: &u64, _: &dyn askama::Values) -> askama::Result<String> {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * KB;
        let b = *value;
        Ok(if b >= MB {
            format!("{} MB", b / MB)
        } else if b >= KB {
            format!("{} KB", b / KB)
        } else {
            format!("{b} bytes")
        })
    }

    /// `{{ n|thousands }}` → `"1,000"`. Comma-groups digits so a templated
    /// count reads the same as the prose copy beside it.
    #[askama::filter_fn]
    pub fn thousands(value: &u32, _: &dyn askama::Values) -> askama::Result<String> {
        let digits = value.to_string();
        let len = digits.len();
        let mut out = String::with_capacity(len + len / 3);
        for (i, ch) in digits.char_indices() {
            if i > 0 && (len - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        Ok(out)
    }
}
