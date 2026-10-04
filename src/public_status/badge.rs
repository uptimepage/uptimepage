//! SVG status badge rendering.
//!
//! Produces a shields.io-style "flat" two-segment badge that operators can
//! embed in README files or external dashboards. Hand-rolled SVG (no third
//! party renderer) — output is ~600 bytes and depends only on the input
//! state, label, and status text.

use crate::domain::{OverallState, PublicComponentStatus};
use crate::i18n::Tr;
use crate::public_status::xml::xml_escape;

/// Color palette aligned with the public status page UI.
const COLOR_GREEN: &str = "#4c1";
const COLOR_YELLOW: &str = "#dfb317";
const COLOR_ORANGE: &str = "#fe7d37";
const COLOR_RED: &str = "#e05d44";
const COLOR_BLUE: &str = "#007ec6";
const COLOR_GREY: &str = "#9f9f9f";

/// Approximate character width in 11px Verdana, in user-units. Verdana renders
/// roughly 6.5px/char on average; round up to 7 so longer strings still fit
/// inside the colored pill on common viewers.
const CHAR_WIDTH: u32 = 7;
/// Horizontal padding inside each segment.
const SEGMENT_PADDING: u32 = 10;
const BADGE_HEIGHT: u32 = 20;
/// Hard cap on characters considered for segment sizing. Names beyond this
/// length still render — the segment just stops growing, so an operator can't
/// blow the SVG `width` attribute up to multi-megabytes with a pathological
/// `site_name` or `public_name`.
const MAX_CHARS_PER_SEGMENT: u32 = 64;

/// Mapping from overall page state to (status text, color).
pub fn overall_badge(state: OverallState, tr: Tr) -> (String, &'static str) {
    let (id, color) = match state {
        OverallState::Operational => ("badge-operational", COLOR_GREEN),
        OverallState::MinorDisruption => ("badge-minor", COLOR_YELLOW),
        OverallState::Maintenance => ("badge-maintenance", COLOR_BLUE),
        OverallState::PartialOutage => ("badge-partial", COLOR_ORANGE),
        OverallState::MajorOutage => ("badge-major", COLOR_RED),
    };
    (tr.t(id), color)
}

/// Mapping from per-component status to (status text, color).
pub fn component_badge(state: PublicComponentStatus, tr: Tr) -> (String, &'static str) {
    let (id, color) = match state {
        PublicComponentStatus::Operational => ("badge-operational", COLOR_GREEN),
        PublicComponentStatus::Degraded => ("badge-degraded", COLOR_YELLOW),
        PublicComponentStatus::Maintenance => ("badge-maintenance", COLOR_BLUE),
        PublicComponentStatus::PartialOutage => ("badge-partial", COLOR_ORANGE),
        PublicComponentStatus::MajorOutage => ("badge-major", COLOR_RED),
        PublicComponentStatus::NoData => ("badge-no-data", COLOR_GREY),
    };
    (tr.t(id), color)
}

/// Build a flat shields.io-style SVG badge. The label segment is gray; the
/// status segment carries the state color.
pub fn render_badge(label: &str, status: &str, color: &str) -> String {
    let label_w = segment_width(label);
    let status_w = segment_width(status);
    let total = label_w + status_w;
    let label_mid = label_w / 2;
    let status_mid = label_w + status_w / 2;

    let label_esc = xml_escape(label);
    let status_esc = xml_escape(status);

    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{total}" height="{h}" role="img" aria-label="{label_esc}: {status_esc}">
<title>{label_esc}: {status_esc}</title>
<linearGradient id="g" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
<mask id="m"><rect width="{total}" height="{h}" rx="3" fill="#fff"/></mask>
<g mask="url(#m)"><rect width="{label_w}" height="{h}" fill="#555"/><rect x="{label_w}" width="{status_w}" height="{h}" fill="{color}"/><rect width="{total}" height="{h}" fill="url(#g)"/></g>
<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11"><text x="{label_mid}" y="15" fill="#010101" fill-opacity=".3">{label_esc}</text><text x="{label_mid}" y="14">{label_esc}</text><text x="{status_mid}" y="15" fill="#010101" fill-opacity=".3">{status_esc}</text><text x="{status_mid}" y="14">{status_esc}</text></g>
</svg>"##,
        h = BADGE_HEIGHT,
    )
}

fn segment_width(text: &str) -> u32 {
    // Count Unicode scalar values rather than bytes so multibyte names render
    // with sensible spacing. Clamped so a hostile operator-supplied name can't
    // blow the SVG width attribute up to multi-megabytes.
    let n = (text.chars().count() as u32).min(MAX_CHARS_PER_SEGMENT);
    n * CHAR_WIDTH + SEGMENT_PADDING * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overall_state_covers_every_variant() {
        for s in [
            OverallState::Operational,
            OverallState::MinorDisruption,
            OverallState::Maintenance,
            OverallState::PartialOutage,
            OverallState::MajorOutage,
        ] {
            let (text, color) = overall_badge(s, Tr::default());
            assert!(!text.is_empty());
            assert!(color.starts_with('#'));
        }
    }

    #[test]
    fn component_state_covers_every_variant() {
        for s in [
            PublicComponentStatus::Operational,
            PublicComponentStatus::Degraded,
            PublicComponentStatus::Maintenance,
            PublicComponentStatus::PartialOutage,
            PublicComponentStatus::MajorOutage,
            PublicComponentStatus::NoData,
        ] {
            let (text, color) = component_badge(s, Tr::default());
            assert!(!text.is_empty());
            assert!(color.starts_with('#'));
        }
    }

    #[test]
    fn a_german_page_badge_reads_german() {
        let de = Tr::new(crate::domain::Locale::De);
        assert_eq!(
            overall_badge(OverallState::MajorOutage, de).0,
            "größerer Ausfall"
        );
        assert_eq!(
            component_badge(PublicComponentStatus::NoData, de).0,
            "keine Daten"
        );
    }

    #[test]
    fn a_silent_component_badge_is_grey_and_says_no_data() {
        // Never a green badge: a README embedding this must not read as a
        // health claim for something that has reported nothing.
        let (text, color) = component_badge(PublicComponentStatus::NoData, Tr::default());
        assert_eq!(text, "no data");
        assert_ne!(color, COLOR_GREEN);
    }

    #[test]
    fn render_produces_valid_xml_root() {
        let svg = render_badge("API", "operational", COLOR_GREEN);
        assert!(svg.starts_with("<svg "));
        assert!(svg.contains("xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(svg.trim_end().ends_with("</svg>"));
    }

    #[test]
    fn render_escapes_xml_special_chars() {
        let svg = render_badge("a&b<c>", "x\"y'z", COLOR_GREEN);
        assert!(svg.contains("a&amp;b&lt;c&gt;"));
        assert!(svg.contains("x&quot;y&apos;z"));
        assert!(!svg.contains("a&b<c>"));
    }

    #[test]
    fn render_includes_accessible_title() {
        let svg = render_badge("API", "degraded", COLOR_YELLOW);
        assert!(svg.contains("aria-label=\"API: degraded\""));
        assert!(svg.contains("<title>API: degraded</title>"));
    }

    #[test]
    fn render_carries_state_color() {
        let svg = render_badge("X", "major outage", COLOR_RED);
        assert!(svg.contains(COLOR_RED));
    }

    #[test]
    fn segment_width_caps_pathological_input() {
        let huge: String = "x".repeat(10_000);
        let capped = segment_width(&huge);
        let unbounded = 10_000 * CHAR_WIDTH + SEGMENT_PADDING * 2;
        assert!(capped < unbounded);
        assert_eq!(
            capped,
            MAX_CHARS_PER_SEGMENT * CHAR_WIDTH + SEGMENT_PADDING * 2
        );
    }
}
