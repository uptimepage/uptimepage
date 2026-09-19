/// Wire-level placeholder substituted for populated credentials in API responses.
/// Re-submitting it on `PATCH` is rejected so a `GET → PATCH` round-trip cannot
/// silently overwrite the real value with the sentinel.
pub const REDACTED: &str = "***";

/// Renders as nothing, or ends a line where the surrounding text implies none,
/// so it can hide or forge content inside text a human and a model read
/// differently. `char::is_control` covers only the C0/C1 blocks, which leaves
/// every one of these through.
///
/// The list tracks Unicode's `Default_Ignorable_Code_Point` plus the line and
/// paragraph separators. It is hand-maintained, so it is written to cover whole
/// blocks rather than the individual characters anyone has happened to abuse.
pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}'                  // soft hyphen
        | '\u{034F}'                // combining grapheme joiner
        | '\u{061C}'                // arabic letter mark
        | '\u{115F}'..='\u{1160}'   // hangul choseong/jungseong fillers
        | '\u{17B4}'..='\u{17B5}'   // khmer inherent vowels
        | '\u{180B}'..='\u{180F}'   // mongolian selectors, vowel separator
        | '\u{200B}'..='\u{200F}'   // zero-width, bidi marks
        | '\u{2028}'..='\u{2029}'   // line and paragraph separators
        | '\u{202A}'..='\u{202E}'   // bidi embedding and override
        | '\u{2060}'..='\u{206F}'   // word joiner, invisible ops, bidi isolates, deprecated formats
        | '\u{3164}'                // hangul filler
        | '\u{FEFF}'                // zero-width no-break space
        | '\u{FFA0}'               // halfwidth hangul filler
        | '\u{FFF0}'..='\u{FFF8}'   // unassigned, renders as nothing
        | '\u{E0000}'..='\u{E007F}' // tag characters
    )
}

// Variation selectors (U+FE00..FE0F, U+E0100..E01EF) are deliberately absent.
// They are default-ignorable but only pick a glyph for the character before
// them, so they cannot conceal text — and flagging them would refuse an
// ordinary tag like "❤️ prod", since the validator rejects rather than strips.

#[cfg(test)]
mod tests {
    use super::is_invisible;

    #[test]
    fn characters_that_hide_or_forge_text_are_flagged() {
        for c in [
            '\u{00AD}',  // soft hyphen
            '\u{034F}',  // combining grapheme joiner
            '\u{061C}',  // arabic letter mark
            '\u{115F}',  // hangul choseong filler
            '\u{180E}',  // mongolian vowel separator
            '\u{200B}',  // zero-width space
            '\u{200F}',  // right-to-left mark
            '\u{202E}',  // right-to-left override
            '\u{2060}',  // word joiner
            '\u{2065}',  // unassigned, renders as nothing
            '\u{2069}',  // pop directional isolate
            '\u{206F}',  // nominal digit shapes
            '\u{3164}',  // hangul filler
            '\u{FEFF}',  // zero-width no-break space
            '\u{FFA0}',  // halfwidth hangul filler
            '\u{E0041}', // tag latin capital A
        ] {
            assert!(is_invisible(c), "{:04X} renders as nothing", c as u32);
        }
    }

    /// A separator is worse than a hidden character: it forges a line break, so
    /// customer text can add a line to a confirmation prompt the human reads as
    /// the server's own.
    #[test]
    fn line_and_paragraph_separators_cannot_forge_a_prompt_line() {
        assert!(is_invisible('\u{2028}'));
        assert!(is_invisible('\u{2029}'));
        // Neither is a control character, so `char::is_control` never sees them.
        assert!(!'\u{2028}'.is_control() && !'\u{2029}'.is_control());
    }

    /// Too broad a rule rejects real names. A tag or monitor called "café" or
    /// "日本" is ordinary text, not a hiding place.
    #[test]
    fn text_a_reader_can_see_is_left_alone() {
        for c in [
            'a', 'Z', '0', ' ', '-', '_', '/', ':', '.', 'é', 'ü', 'ß', 'い', '中', '🙂', '€',
            '\u{FE0F}', // variation selector: an emoji's presentation, not a hiding place
        ] {
            assert!(!is_invisible(c), "{:04X} is visible text", c as u32);
        }
        // An emoji written with its presentation selector stays a legal tag.
        assert!(!"❤️ prod".chars().any(is_invisible));
        // Control characters are a separate rule (`char::is_control`), so this
        // one deliberately does not claim them.
        assert!(!is_invisible('\n') && !is_invisible('\t'));
    }
}
