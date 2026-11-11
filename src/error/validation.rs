//! Validators that answer with a field error: the text limits for operator
//! narration (titles, descriptions, update messages), and the paging settings
//! monitors, monitor groups and alert sources share.
//!
//! Constants live here so the limits are one source of truth across
//! maintenance + incident endpoints.

use crate::domain::TargetAlerts;
use crate::error::codes;
use crate::error::{AppError, Result};

pub const MAX_TITLE: usize = 200;
pub const MAX_DESCRIPTION: usize = 5_000;
pub const MAX_MESSAGE: usize = 2_000;

/// Rejects whitespace-only strings as `code_empty` and too-long strings as
/// `code_too_long`. `field` populates the API error's `field` pointer.
pub fn check_required_text(
    value: &str,
    field: &'static str,
    max: usize,
    code_empty: &'static str,
    code_too_long: &'static str,
) -> Result<()> {
    if value.trim().is_empty() {
        return Err(AppError::bad_request_field(
            code_empty,
            format!("{field} cannot be empty"),
            field,
        ));
    }
    check_length(value, field, max, code_too_long)
}

/// Length-only check (allows empty). Use for fields where empty is meaningful
/// (e.g. a description that defaults to None).
pub fn check_length(
    value: &str,
    field: &'static str,
    max: usize,
    code_too_long: &'static str,
) -> Result<()> {
    if value.chars().count() > max {
        return Err(AppError::bad_request_field(
            code_too_long,
            format!("{field} exceeds {max} characters"),
            field,
        ));
    }
    Ok(())
}

/// Convenience: validates a `&str` title against `EMPTY_TITLE` / `TITLE_TOO_LONG`.
pub fn validate_title(value: &str, field: &'static str) -> Result<()> {
    check_required_text(
        value,
        field,
        MAX_TITLE,
        codes::EMPTY_TITLE,
        codes::TITLE_TOO_LONG,
    )
}

/// Convenience: validates an optional description against `DESCRIPTION_TOO_LONG`.
pub fn validate_description(value: Option<&str>, field: &'static str) -> Result<()> {
    if let Some(v) = value {
        check_length(v, field, MAX_DESCRIPTION, codes::DESCRIPTION_TOO_LONG)?;
    }
    Ok(())
}

/// Convenience: validates an update message against
/// `EMPTY_MESSAGE` / `MESSAGE_TOO_LONG`.
pub fn validate_message(value: &str, field: &'static str) -> Result<()> {
    check_required_text(
        value,
        field,
        MAX_MESSAGE,
        codes::EMPTY_MESSAGE,
        codes::MESSAGE_TOO_LONG,
    )
}

/// Double-Option-aware title validator: leaves missing and null PATCH fields
/// alone, length-checks present strings. Whitespace is *not* a clear request
/// — callers should send JSON `null`.
pub fn validate_optional_title(title: Option<&Option<String>>, field: &'static str) -> Result<()> {
    let Some(Some(t)) = title else { return Ok(()) };
    validate_title(t, field)
}

/// Double-Option-aware description validator (allows empty values; only the
/// length cap matters here).
pub fn validate_optional_description(
    desc: Option<&Option<String>>,
    field: &'static str,
) -> Result<()> {
    let Some(Some(d)) = desc else { return Ok(()) };
    validate_description(Some(d), field)
}

/// The outage reminder cadence is either off (0) or no tighter than a minute —
/// a sub-minute reminder would just spam responders.
/// The column is an INTEGER, so the top is i32's.
pub(crate) fn validate_renotify_interval(secs: Option<u32>) -> Result<()> {
    if matches!(secs, Some(n) if (n > 0 && n < 60) || i32::try_from(n).is_err()) {
        return Err(AppError::bad_request_field(
            codes::INVALID_ALERT_CONFIG,
            "renotify_interval_secs must be 0 (off) or from 60 to 2147483647",
            "renotify_interval_secs",
        ));
    }
    Ok(())
}

/// Refuses a channel bound twice: each open and resolve goes out once per
/// bound channel, so a duplicate would page it twice. Structural only, no I/O:
/// that each channel belongs to the caller's org is checked by the write.
pub(crate) fn validate_alerts(alerts: &TargetAlerts) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for (i, b) in alerts.iter().enumerate() {
        if !seen.insert(b.channel_id) {
            return Err(AppError::bad_request_field(
                codes::INVALID_ALERT_CONFIG,
                format!(
                    "alerts[{i}]: duplicate binding for channel {}",
                    b.channel_id
                ),
                format!("alerts[{i}].channel_id"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_title_rejects_empty_and_long() {
        assert!(matches!(
            validate_title("", "title"),
            Err(AppError::BadRequest { code, .. }) if code == codes::EMPTY_TITLE
        ));
        let long = "x".repeat(MAX_TITLE + 1);
        assert!(matches!(
            validate_title(&long, "title"),
            Err(AppError::BadRequest { code, .. }) if code == codes::TITLE_TOO_LONG
        ));
        assert!(validate_title("ok", "title").is_ok());
    }

    #[test]
    fn validate_message_rejects_empty_and_long() {
        assert!(matches!(
            validate_message("  ", "message"),
            Err(AppError::BadRequest { code, .. }) if code == codes::EMPTY_MESSAGE
        ));
        let long = "x".repeat(MAX_MESSAGE + 1);
        assert!(matches!(
            validate_message(&long, "message"),
            Err(AppError::BadRequest { code, .. }) if code == codes::MESSAGE_TOO_LONG
        ));
    }

    #[test]
    fn renotify_interval_fits_the_column() {
        assert!(validate_renotify_interval(Some(0)).is_ok());
        assert!(validate_renotify_interval(Some(60)).is_ok());
        assert!(validate_renotify_interval(Some(i32::MAX as u32)).is_ok());
        assert!(validate_renotify_interval(Some(59)).is_err());
        assert!(validate_renotify_interval(Some(i32::MAX as u32 + 1)).is_err());
    }
}
