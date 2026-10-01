//! Cleaning rules for the fields an operator types onto a status page, shared by
//! the REST handlers and the MCP tools so both store the same thing.

use crate::error::codes;
use crate::error::{AppError, Result};

pub fn validate_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 80 {
        return Err(AppError::bad_request_field(
            codes::BRANDING_INVALID,
            "name must be 1–80 characters",
            "name",
        ));
    }
    Ok(trimmed.to_owned())
}

pub fn normalise_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Trim a per-page curation field, treat blank as cleared (`None`), and bound
/// it to the DB CHECK's max so an over-long value is a 400 with the field name
/// rather than an opaque 500 from the constraint. `max` is in characters to
/// match Postgres `char_length`.
pub fn clean_curation(
    v: Option<String>,
    field: &'static str,
    max: usize,
) -> Result<Option<String>> {
    let v = normalise_opt(v);
    if let Some(ref s) = v
        && s.chars().count() > max
    {
        return Err(AppError::bad_request_field(
            codes::BRANDING_INVALID,
            format!("{field} must be at most {max} characters"),
            field,
        ));
    }
    Ok(v)
}

/// Patch-flavoured [`clean_curation`]: preserves the present/absent distinction
/// (outer `None` = leave unchanged) while normalising + validating the inner
/// value (an explicit blank or `null` clears the override).
pub fn clean_curation_patch(
    v: Option<Option<String>>,
    field: &'static str,
    max: usize,
) -> Result<Option<Option<String>>> {
    match v {
        None => Ok(None),
        Some(inner) => Ok(Some(clean_curation(inner, field, max)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_blanks_to_none() {
        assert_eq!(normalise_opt(Some("  ".into())), None);
        assert_eq!(normalise_opt(Some("  hi ".into())), Some("hi".into()));
    }
}
