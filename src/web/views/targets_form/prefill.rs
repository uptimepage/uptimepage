//! Prefilling a create form from the query string, so a link out of the
//! coverage hints or the marketing hero lands on a half-filled form.

use serde::Deserialize;
use uuid::Uuid;

use crate::domain::CheckSpec;

use super::model::FormModel;

#[derive(Debug, Default, Deserialize)]
pub struct NewParams {
    /// When set, prefill the create form from an existing monitor (the
    /// "Copy" action on the list) so similar monitors can be added fast.
    #[serde(default)]
    pub from: Option<Uuid>,
    /// From the coverage hints. Unrecognised values are ignored rather than
    /// rejected: this arrives in a URL people bookmark and edit.
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    /// Carried from the marketing hero, typed before they have an account.
    #[serde(default)]
    pub url: Option<String>,
}

/// Unrecognised kinds are ignored, not rejected: people hand-edit this URL.
pub(super) fn apply_kind_param(form: &mut FormModel, kind: &str) -> bool {
    let Some(kind) = CheckSpec::ALL_KINDS.iter().find(|k| **k == kind).copied() else {
        return false;
    };
    form.check_type = kind;
    form.interval_s = crate::domain::interval_hints_for_kind(kind).default;
    form.recovery_period_secs = crate::domain::default_recovery_period_secs_for_kind(kind);
    true
}

/// Unparseable input is dropped rather than shown back broken. `http` only:
/// flow can still be downgraded below, stranding a URL in the flow field.
pub(super) fn prefill_url(form: &mut FormModel, raw: &str) {
    if form.check_type != "http" {
        return;
    }
    let Some(url) = parse_monitor_url(raw) else {
        return;
    };
    form.name = url.host_str().unwrap_or_default().to_owned();
    form.http.url = url.into();
}

/// A bare host is promoted to `https://`; only the two web schemes are honoured
/// so a `javascript:` or `file:` value never reaches the field.
pub(super) fn parse_monitor_url(raw: &str) -> Option<url::Url> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 2048 {
        return None;
    }
    let candidate = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    let url = url::Url::parse(&candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return None;
    }
    Some(url)
}

/// Kinds with no single host to fill (http, flow, heartbeat) leave the form
/// untouched: there is no sensible half-prefill for a URL or a dead-man's switch.
pub(super) fn prefill_host(form: &mut FormModel, host: &str) {
    let host = host.trim();
    if host.is_empty() {
        return;
    }
    match form.check_type {
        "tls_cert" => form.tls_cert.host = host.to_owned(),
        "domain_expiry" => form.domain_expiry.domain = host.to_owned(),
        "dns" => form.dns.domain = host.to_owned(),
        "tcp" => form.tcp.host = host.to_owned(),
        "ping" => form.ping.host = host.to_owned(),
        _ => return,
    }
    form.name = format!("{host} {}", form.check_type).replace('_', " ");
}
