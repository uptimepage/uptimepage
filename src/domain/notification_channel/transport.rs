//! The per-transport contract. Every delivery destination implements this
//! over its own config struct; [`super::ChannelConfig`] only delegates, so a
//! new transport is a new module here plus one enum variant — the compiler
//! walks you through the rest.

use super::ChannelKind;
use crate::domain::REDACTED;

pub const MASK: &str = REDACTED;

pub trait TransportConfig {
    const KIND: ChannelKind;

    fn kind(&self) -> ChannelKind {
        Self::KIND
    }

    /// Overwrite every secret-bearing field with the `***` mask in place.
    /// Non-secret routing shape (header *names*, chat id) is kept so the UI
    /// can still show which channel this is.
    fn redact_in_place(&mut self);

    /// True if any secret-bearing field still carries the redaction
    /// sentinel. A `GET → PATCH` round-trip that re-submits a masked config
    /// must be rejected, never written back as the literal `***`.
    fn has_redaction_sentinel(&self) -> bool;

    /// Cheap structural validation (no network). Returns a human message on
    /// the first problem. Reachability / SSRF checks belong to the notifier
    /// transport, not here.
    fn validate(&self) -> Result<(), String>;

    /// The customer-controlled destination URL to run through the abuse
    /// deny-list. `None` means deliveries go to a fixed vendor endpoint and
    /// there is nothing to inspect. No default on purpose: skipping the
    /// abuse gate must be an explicit per-transport decision, not an
    /// omission.
    fn abuse_url(&self) -> Option<&str>;

    /// True when only the operator's own flow may produce this config (a
    /// caller-supplied destination would ride the operator's credentials).
    /// No default on purpose, like [`Self::abuse_url`].
    fn operator_managed(&self) -> bool;

    /// Clean up what a paste carries in, before [`Self::validate`] judges the
    /// shape. No default on purpose, like [`Self::abuse_url`]: the console
    /// trims in the browser, so a transport that skips this is broken only for
    /// the API, MCP and Terraform, where nobody would notice.
    fn normalize(&mut self);

    /// Drop the pings that wake a whole room, keeping the targeted ones, so a
    /// config test proves the routing without paging everybody. No default on
    /// purpose, like [`Self::abuse_url`]: a transport that grows a ping field
    /// must decide here, not inherit silence.
    fn quiet_broadcast_mention(&mut self);

    /// Non-secret destination id mirrored into the plaintext `external_ref`
    /// column so provider lifecycle events (bot kicked, address bounced)
    /// can find channels without opening sealed configs. `None` = this
    /// transport has no provider-side lifecycle.
    fn lifecycle_ref(&self) -> Option<&str> {
        None
    }
}

/// Not a phone-number parser: no country is inferred and nothing is
/// reformatted, so a wrong number still reads back as the one that was typed.
/// Only for a field that can be nothing else — a sender id may carry a dash.
pub(super) fn strip_phone_separators(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '\u{a0}' | '-' | '(' | ')' | '.'))
        .collect()
}

pub(super) fn trim_in_place(s: &mut String) {
    let t = s.trim();
    if t.len() != s.len() {
        *s = t.to_string();
    }
}

/// `https://`-only URL rule shared by the URL-bearing transports.
pub(super) fn require_https(u: &str, field: &str) -> Result<(), String> {
    let parsed = url::Url::parse(u).map_err(|_| format!("{field} is not a valid URL"))?;
    if parsed.scheme() != "https" {
        return Err(format!("{field} must be an https:// URL"));
    }
    Ok(())
}

/// Host-pinned https rule for provider-branded webhook kinds: exact host or
/// dot-suffix match (root dot normalized so it can't widen the suffix).
pub(super) fn require_provider_webhook(
    u: &str,
    provider: &str,
    domains: &[&str],
    path_prefix: Option<&str>,
) -> Result<(), String> {
    let mismatch = || {
        format!(
            "this doesn't look like a {provider} webhook URL — use the webhook type for \
             nonstandard endpoints"
        )
    };
    let parsed = url::Url::parse(u).map_err(|_| "webhook_url is not a valid URL".to_string())?;
    if parsed.scheme() != "https" {
        return Err("webhook_url must be an https:// URL".into());
    }
    let host = parsed
        .host_str()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let pinned = domains
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")));
    if !pinned {
        return Err(mismatch());
    }
    if let Some(prefix) = path_prefix
        && !parsed.path().starts_with(prefix)
    {
        return Err(mismatch());
    }
    Ok(())
}
