//! Message catalogue for the public status surface and subscriber emails.

use std::borrow::Cow;
use std::collections::HashMap;

use chrono::{DateTime, Datelike, Utc};
use fluent_templates::fluent_bundle::FluentValue;
use fluent_templates::{LanguageIdentifier, langid, static_loader};

use crate::domain::Locale;
use crate::duration::{DurationUnit, duration_parts};

static_loader! {
    static PUBLIC = {
        locales: "./locales",
        fallback_language: "en",
        // Fluent wraps each placeable in U+2068/U+2069 by default, which leaks
        // into titles, subjects and aria labels as stray characters.
        customise: |bundle| bundle.set_use_isolating(false),
    };
}

static EN: LanguageIdentifier = langid!("en");
static DE: LanguageIdentifier = langid!("de");

/// Message lookups in one page's language.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tr(Locale);

impl Tr {
    pub fn new(locale: Locale) -> Self {
        Self(locale)
    }

    pub fn locale(self) -> Locale {
        self.0
    }

    pub fn lang(self) -> &'static str {
        self.0.as_str()
    }

    pub fn og_locale(self) -> &'static str {
        match self.0 {
            Locale::En => "en_US",
            Locale::De => "de_DE",
        }
    }

    /// Locale the browser formats dates in. English leaves it to the visitor's
    /// browser, as the console does.
    pub fn date_locale(self) -> Option<&'static str> {
        match self.0 {
            Locale::En => None,
            Locale::De => Some("de"),
        }
    }

    fn lang_id(self) -> &'static LanguageIdentifier {
        match self.0 {
            Locale::En => &EN,
            Locale::De => &DE,
        }
    }

    pub fn t(self, id: &str) -> String {
        self.lookup(id, None)
    }

    pub fn t_args<'a>(
        self,
        id: &str,
        args: impl IntoIterator<Item = (&'static str, FluentValue<'a>)>,
    ) -> String {
        let args: HashMap<Cow<'static, str>, FluentValue<'a>> = args
            .into_iter()
            .map(|(k, v)| (Cow::Borrowed(k), v))
            .collect();
        self.lookup(id, Some(&args))
    }

    fn lookup(
        self,
        id: &str,
        args: Option<&HashMap<Cow<'static, str>, FluentValue<'_>>>,
    ) -> String {
        let english = || {
            PUBLIC
                .lookup_single_language(&EN, id, args)
                .unwrap_or_else(|error| {
                    tracing::warn!(id, %error, "public message unusable");
                    id.to_owned()
                })
        };
        if self.0 == Locale::En {
            return english();
        }
        PUBLIC
            .lookup_single_language(self.lang_id(), id, args)
            .unwrap_or_else(|error| {
                tracing::warn!(id, lang = self.lang(), %error, "public message unusable, falling back to English");
                english()
            })
    }

    pub fn month(self, month: u32) -> String {
        self.t(&format!("month-{month}"))
    }

    pub fn month_short(self, month: u32) -> String {
        self.t(&format!("month-short-{month}"))
    }

    pub fn month_year(self, at: DateTime<Utc>) -> String {
        self.t_args(
            "date-month-year",
            [
                ("month", self.month(at.month()).into()),
                ("year", at.year().into()),
            ],
        )
    }

    pub fn day(self, at: DateTime<Utc>) -> String {
        self.t_args(
            "date-day",
            [
                ("day", at.day().into()),
                ("month", self.month_short(at.month()).into()),
                ("year", at.year().into()),
            ],
        )
    }

    /// Wall-clock stamp for mail, always UTC: an inbox has no viewer timezone.
    pub fn utc_stamp(self, at: DateTime<Utc>) -> String {
        self.t_args(
            "date-stamp",
            [
                ("day", at.day().into()),
                ("month", self.month_short(at.month()).into()),
                ("year", at.year().into()),
                ("time", at.format("%H:%M").to_string().into()),
            ],
        )
    }

    /// Two-unit duration, e.g. `2h 14m`.
    pub fn duration(self, secs: i64) -> String {
        let (major, minor) = duration_parts(secs);
        let mut out = self.duration_part(major);
        if let Some(minor) = minor {
            out.push(' ');
            out.push_str(&self.duration_part(minor));
        }
        out
    }

    fn duration_part(self, (n, unit): (i64, DurationUnit)) -> String {
        let id = match unit {
            DurationUnit::Second => "duration-seconds",
            DurationUnit::Minute => "duration-minutes",
            DurationUnit::Hour => "duration-hours",
            DurationUnit::Day => "duration-days",
        };
        self.t_args(id, [("n", n.into())])
    }

    pub fn ago(self, secs: i64) -> String {
        self.t_args("elapsed-ago", [("duration", self.duration(secs).into())])
    }

    /// Title of an incident whose operator never set `public_title`: the
    /// component plus the status it opened in, as in `"API down"`.
    pub fn auto_incident_title(self, component_name: &str, status_at_start: &str) -> String {
        if component_name.is_empty() {
            return self.t("auto-title-generic");
        }
        let status = match status_at_start {
            "down" => self.t("auto-title-down"),
            "degraded" => self.t("auto-title-degraded"),
            "error" => self.t("auto-title-error"),
            other => other.replace('_', " "),
        };
        self.t_args(
            "auto-title",
            [
                ("component", component_name.into()),
                ("status", status.into()),
            ],
        )
    }

    pub fn percent(self, pct: f64) -> String {
        let value = format!("{pct:.2}").replace('.', &self.t("decimal-separator"));
        self.t_args("percent", [("value", value.into())])
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use chrono::TimeZone;

    use super::*;

    fn catalogue(locale: Locale) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("locales")
            .join(locale.as_str())
            .join("public.ftl");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// Each message id with its whole body, continuation lines included.
    fn message_bodies(ftl: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for line in ftl.lines() {
            match message_ids(line).into_iter().next() {
                Some(id) => out.push((id, line.to_owned())),
                None if line.starts_with(' ') => {
                    if let Some((_, body)) = out.last_mut() {
                        body.push_str(line);
                    }
                }
                None => {}
            }
        }
        out
    }

    fn message_ids(ftl: &str) -> BTreeSet<String> {
        ftl.lines()
            .filter_map(|line| {
                let (id, _) = line.split_once('=')?;
                let id = id.trim_end();
                let first = id.chars().next()?;
                (first.is_ascii_lowercase()
                    && id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
                .then(|| id.to_owned())
            })
            .collect()
    }

    /// Ids the code can ask for: `t("…")` calls in templates, and every
    /// id-shaped literal outside tests in a Rust file that uses `Tr`, which
    /// catches ids picked by a `match` and passed along. `prefixes` keeps
    /// header names and CSS classes out.
    fn referenced_ids(prefixes: &BTreeSet<String>) -> BTreeSet<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let call = regex::Regex::new(r#"\bt(?:_args)?\(\s*"([a-z][a-z0-9-]*)""#).unwrap();
        let literal = regex::Regex::new(r#""([a-z][a-z0-9]*(?:-[a-z0-9]+)+)""#).unwrap();
        let mut ids = BTreeSet::new();
        let mut stack = vec![root.join("templates"), root.join("src")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                match path.extension().and_then(|e| e.to_str()) {
                    Some("html") => {
                        ids.extend(call.captures_iter(&text).map(|c| c[1].to_owned()));
                    }
                    Some("rs") if path.file_name().is_some_and(|n| n != "tests.rs") => {
                        let source = text.split("#[cfg(test)]").next().unwrap_or_default();
                        if source.contains("i18n::Tr") || path.ends_with("src/i18n.rs") {
                            ids.extend(
                                literal
                                    .captures_iter(source)
                                    .map(|c| c[1].to_owned())
                                    .filter(|id| {
                                        id.split('-').next().is_some_and(|p| prefixes.contains(p))
                                    }),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        ids
    }

    #[test]
    fn every_locale_carries_exactly_the_english_ids() {
        let en = message_ids(&catalogue(Locale::En));
        assert!(en.len() > 100, "parsed only {} ids", en.len());
        for locale in Locale::ALL {
            let ids = message_ids(&catalogue(locale));
            let missing: Vec<_> = en.difference(&ids).collect();
            let extra: Vec<_> = ids.difference(&en).collect();
            assert!(missing.is_empty(), "{locale:?} lacks {missing:?}");
            assert!(extra.is_empty(), "{locale:?} has unknown {extra:?}");
            assert_ne!(Tr::new(locale).t("month-1"), "month-1", "{locale:?}");
        }
    }

    /// Each message is formatted with every variable the English one takes, so
    /// a translation naming a variable that never arrives fails here instead of
    /// quietly rendering English.
    #[test]
    fn every_message_formats_in_every_locale() {
        let en = catalogue(Locale::En);
        let variable = regex::Regex::new(r"\$([a-z_]+)").unwrap();
        for (id, body) in message_bodies(&en) {
            let args: HashMap<Cow<'static, str>, FluentValue> = variable
                .captures_iter(&body)
                .map(|c| (Cow::Owned(c[1].to_owned()), FluentValue::from(1)))
                .collect();
            for locale in Locale::ALL {
                let tr = Tr::new(locale);
                if let Err(e) = PUBLIC.lookup_single_language(tr.lang_id(), &id, Some(&args)) {
                    panic!("{locale:?} {id}: {e}");
                }
            }
        }
    }

    #[test]
    fn every_referenced_id_exists() {
        let en = message_ids(&catalogue(Locale::En));
        let prefixes: BTreeSet<String> = en
            .iter()
            .filter_map(|id| id.split('-').next())
            .map(str::to_owned)
            .collect();
        let referenced = referenced_ids(&prefixes);
        assert!(
            referenced.contains("notice-check-address"),
            "scan reached subscribe.rs"
        );
        let missing: Vec<_> = referenced
            .into_iter()
            .filter(|id| !en.contains(id))
            .collect();
        assert!(missing.is_empty(), "no message for {missing:?}");
    }

    #[test]
    fn the_catalogue_holds_no_markup() {
        for locale in Locale::ALL {
            let ftl = catalogue(locale);
            assert!(!ftl.contains('<'), "{locale:?} catalogue contains markup");
        }
    }

    #[test]
    fn placeables_carry_no_isolation_marks() {
        for locale in Locale::ALL {
            let s = Tr::new(locale).t_args("day-strip-label", [("name", "API".into())]);
            assert!(s.contains("API"), "{s}");
            assert!(!s.contains(['\u{2068}', '\u{2069}']), "{s:?}");
        }
    }

    #[test]
    fn english_formats_match_the_pages_before_translation() {
        let tr = Tr::new(Locale::En);
        let at = Utc.with_ymd_and_hms(2026, 8, 20, 1, 5, 0).unwrap();
        assert_eq!(tr.month_year(at), "August 2026");
        assert_eq!(tr.day(at), "20 Aug 2026");
        assert_eq!(tr.utc_stamp(at), "20 Aug 2026 01:05 UTC");
        assert_eq!(tr.duration(134 * 60), "2h 14m");
        assert_eq!(tr.duration(25 * 3600), "1d 1h");
        assert_eq!(tr.ago(45), "45s ago");
        assert_eq!(tr.percent(99.9666), "99.97%");
    }

    #[test]
    fn german_formats_its_own_dates_numbers_and_durations() {
        let tr = Tr::new(Locale::De);
        let at = Utc.with_ymd_and_hms(2026, 3, 2, 14, 5, 0).unwrap();
        assert_eq!(tr.month_year(at), "März 2026");
        assert_eq!(tr.day(at), "2. März 2026");
        assert_eq!(tr.utc_stamp(at), "2. März 2026, 14:05 UTC");
        assert_eq!(tr.duration(134 * 60), "2 Std. 14 Min.");
        assert_eq!(tr.ago(45), "vor 45 Sek.");
        assert_eq!(tr.percent(99.9666), "99,97 %");
        assert_eq!(tr.auto_incident_title("API", "down"), "API: ausgefallen");
        assert_eq!(tr.auto_incident_title("", "down"), "Dienststörung");
        assert_eq!(Tr::default().auto_incident_title("API", "down"), "API down");
    }

    #[test]
    fn an_unknown_id_renders_as_itself() {
        assert_eq!(Tr::default().t("no-such-message"), "no-such-message");
    }
}
