//! Startup validation: a bad number is a named config error, never a panic in
//! router or layer construction.

use secrecy::ExposeSecret;

use crate::error::Result;

use super::{AppConfig, EmailProvider};
use crate::domain::mailbox;

impl AppConfig {
    /// Reject `< 1` quota / rate / interval values at load with a
    /// field-named error (I6). A bad number is a clean startup *config*
    /// error, never a `.expect()` crash-loop in router/layer construction.
    pub fn validate_quotas_and_limits(&self) -> Result<()> {
        fn ge1_u64(v: u64, field: &str) -> Result<()> {
            if v < 1 {
                return Err(crate::error::AppError::Other(anyhow::anyhow!(
                    "{field} must be >= 1 (got {v})"
                )));
            }
            Ok(())
        }
        ge1_u64(
            self.quotas.plan_cache_ttl_secs,
            "quotas.plan_cache_ttl_secs",
        )?;
        ge1_u64(
            self.quotas.usage_cache_ttl_secs,
            "quotas.usage_cache_ttl_secs",
        )?;
        ge1_u64(
            self.rate_limits.janitor.cleanup_interval_hours,
            "rate_limits.janitor.cleanup_interval_hours",
        )?;
        ge1_u64(
            self.rate_limits.janitor.idle_threshold_hours,
            "rate_limits.janitor.idle_threshold_hours",
        )?;
        ge1_u64(
            self.scheduler.target_refresh_interval_secs,
            "scheduler.target_refresh_interval_secs",
        )?;
        if self.checker.per_host_max_inflight == 0 {
            return Err(crate::error::AppError::Other(anyhow::anyhow!(
                "checker.per_host_max_inflight must be >= 1"
            )));
        }
        if self.checker.rdap_max_inflight == 0 {
            return Err(crate::error::AppError::Other(anyhow::anyhow!(
                "checker.rdap_max_inflight must be >= 1"
            )));
        }
        if self.escalation.reconcile_window_secs <= self.escalation.tick_interval_secs {
            return Err(crate::error::AppError::Other(anyhow::anyhow!(
                "escalation.reconcile_window_secs ({}) must exceed tick_interval_secs ({}) \
                 or the reconcile scan never matches",
                self.escalation.reconcile_window_secs,
                self.escalation.tick_interval_secs
            )));
        }
        // A zero hold would read as "held" and release on the very next tick,
        // and the operator-facing copy would say "0 minutes".
        if self.escalation.flap_max_opens > 0 {
            ge1_u64(self.escalation.flap_hold_secs, "escalation.flap_hold_secs")?;
            ge1_u64(
                self.escalation.flap_window_secs,
                "escalation.flap_window_secs",
            )?;
            if self.escalation.flap_hold_secs >= self.escalation.flap_window_secs {
                return Err(crate::error::AppError::Other(anyhow::anyhow!(
                    "escalation.flap_hold_secs ({}) must be under flap_window_secs ({}) \
                     or a held alert outlives the window that judged it flapping",
                    self.escalation.flap_hold_secs,
                    self.escalation.flap_window_secs
                )));
            }
        }
        if self.escalation.enabled {
            ge1_u64(
                self.escalation.tick_interval_secs,
                "escalation.tick_interval_secs",
            )?;
            if self.escalation.max_attempts < 1 {
                return Err(crate::error::AppError::Other(anyhow::anyhow!(
                    "escalation.max_attempts must be >= 1 (got {})",
                    self.escalation.max_attempts
                )));
            }
        }
        // Zero is "keep nothing", not "keep nothing older than the window".
        for (days, field) in [
            (
                self.retention.login_attempts_days,
                "retention.login_attempts_days",
            ),
            (
                self.retention.quota_events_days,
                "retention.quota_events_days",
            ),
            (self.retention.audit_log_days, "retention.audit_log_days"),
            (self.retention.mcp_audit_days, "retention.mcp_audit_days"),
            (
                self.auth.session.idle_timeout_days,
                "auth.session.idle_timeout_days",
            ),
            (
                self.tenancy.deletion_grace_period_days,
                "tenancy.deletion_grace_period_days",
            ),
        ] {
            ge1_u64(u64::from(days), field)?;
        }
        Ok(())
    }

    /// Marketing-site boot invariants. Cheap startup errors, never
    /// panics in router construction. Skipped wholesale when
    /// `marketing.enabled = false` so self-host deployments need not set
    /// any of these.
    pub fn validate_marketing(&self) -> Result<()> {
        fn err(msg: String) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg))
        }
        let m = &self.marketing;
        if !m.enabled {
            return Ok(());
        }
        let base = self.public_status.base_domain.trim();
        if base.is_empty() || !base.contains('.') {
            return Err(err(format!(
                "marketing.enabled = true requires public_status.base_domain to be a non-empty FQDN (got {base:?})"
            )));
        }
        for (field, value) in [
            ("marketing.canonical_origin", m.canonical_origin.as_str()),
            ("marketing.app_url", m.app_url.as_str()),
        ] {
            let v = value.trim();
            if v.is_empty() {
                return Err(err(format!(
                    "{field} is required when marketing.enabled = true"
                )));
            }
            if !v.starts_with("https://") {
                return Err(err(format!("{field} must start with https:// (got {v:?})")));
            }
            if v.ends_with('/') {
                return Err(err(format!(
                    "{field} must not end with a trailing slash (got {v:?})"
                )));
            }
        }
        for sub in &m.reserved_subdomains {
            let lower = sub.to_ascii_lowercase();
            if !crate::domain::reserved_slugs::is_reserved(&lower) {
                return Err(err(format!(
                    "marketing.reserved_subdomains entry {sub:?} is not in \
                     domain::reserved_slugs::RESERVED_SLUGS — keep the two lists aligned"
                )));
            }
        }
        // The session cookie must not be scoped to a parent zone that the
        // marketing host inherits; otherwise the app's session ID rides
        // along to the apex and the marketing CDN cache becomes Vary:
        // Cookie. Host-only (empty Domain) is always safe.
        let cd = self.auth.session.cookie_domain.trim();
        if !cd.is_empty() {
            let stripped = cd.trim_start_matches('.');
            if stripped == base || base.ends_with(&format!(".{stripped}")) {
                return Err(err(format!(
                    "auth.session.cookie_domain={cd:?} overlaps marketing host {base:?}; \
                     leave cookie_domain empty (host-only) so the apex marketing surface \
                     is not Vary: Cookie"
                )));
            }
        }
        Ok(())
    }

    /// Trace-export config is a clean startup error when inconsistent,
    /// never a runtime panic. Credentials are required only when export
    /// is actually active (`tracing_enabled` AND `grafana.enabled`); the
    /// sample ratio is always range-checked.
    pub fn validate_observability(&self) -> Result<()> {
        fn err(msg: String) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg))
        }
        let g = &self.observability.grafana;
        let r = g.trace_sample_ratio;
        if !(0.0..=1.0).contains(&r) {
            return Err(err(format!(
                "observability.grafana.trace_sample_ratio must be in [0.0, 1.0] (got {r})"
            )));
        }
        if self.observability.tracing_enabled && g.enabled {
            if g.otlp_endpoint.trim().is_empty() {
                return Err(err(
                    "observability.grafana.otlp_endpoint is required when tracing_enabled and grafana.enabled are true".into(),
                ));
            }
            if g.instance_id.trim().is_empty() {
                return Err(err(
                    "observability.grafana.instance_id is required when tracing_enabled and grafana.enabled are true".into(),
                ));
            }
            if g.api_key.expose_secret().trim().is_empty() {
                return Err(err(
                    "UPTIMEPAGE_OBSERVABILITY__GRAFANA__API_KEY is required when tracing_enabled and grafana.enabled are true".into(),
                ));
            }
        }
        let hb = &self.observability.heartbeat;
        if hb.enabled {
            if hb.url.trim().is_empty() {
                return Err(err(
                    "UPTIMEPAGE_OBSERVABILITY__HEARTBEAT__URL is required when observability.heartbeat.enabled is true".into(),
                ));
            }
            if hb.interval_seconds == 0 {
                return Err(err(
                    "observability.heartbeat.interval_seconds must be > 0".into()
                ));
            }
        }
        Ok(())
    }

    /// Central-bot invariants, enforced only when `telegram.bot_token` is set.
    /// A misconfigured bot here is a clean startup error rather than a half-up
    /// feature that mints dead deep links.
    pub fn validate_telegram(&self) -> Result<()> {
        fn err(msg: &str) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg.to_string()))
        }
        let t = &self.telegram;
        if !t.enabled() {
            return Ok(());
        }
        if t.bot_username.trim().is_empty() {
            return Err(err(
                "telegram.bot_username is required when telegram.bot_token is set",
            ));
        }
        if t.webhook_secret.expose_secret().trim().len() < 32 {
            return Err(err(
                "UPTIMEPAGE_TELEGRAM__WEBHOOK_SECRET must be at least 32 chars when telegram.bot_token is set",
            ));
        }
        let base = self.auth.public_base_url.trim();
        match url::Url::parse(base) {
            Ok(u) if u.scheme() == "https" && u.host_str().is_some() => {}
            _ => {
                return Err(err(
                    "auth.public_base_url must be an https:// URL with a host for the telegram webhook",
                ));
            }
        }
        Ok(())
    }

    /// A named provider must come with everything it needs; a half-configured
    /// one would mint checkouts nobody can complete or drop every webhook.
    pub fn validate_billing(&self) -> Result<()> {
        fn err(msg: &str) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg.to_string()))
        }
        let b = &self.billing;
        match b.provider.as_str() {
            "none" => return Ok(()),
            "paddle" => {}
            other => {
                return Err(crate::error::AppError::Other(anyhow::anyhow!(
                    "billing.provider must be \"none\" or \"paddle\" (got {other:?})"
                )));
            }
        }
        if super::PaddleEnvironment::parse(&b.paddle.environment).is_none() {
            return Err(err(
                "billing.paddle.environment must be \"sandbox\" or \"live\"",
            ));
        }
        if !b.paddle.complete() {
            return Err(err(
                "billing.provider = \"paddle\" needs UPTIMEPAGE_BILLING__PADDLE__API_KEY, \
                 UPTIMEPAGE_BILLING__PADDLE__WEBHOOK_SECRET and billing.paddle.client_token",
            ));
        }
        let base = self.auth.public_base_url.trim();
        match url::Url::parse(base) {
            Ok(u) if u.scheme() == "https" && u.host_str().is_some() => {}
            _ => {
                return Err(err(
                    "auth.public_base_url must be an https:// URL with a host for the billing webhook and pay page",
                ));
            }
        }
        Ok(())
    }

    /// An unrecognised `signup_policy` must not fall back to a permissive
    /// default: a typo would silently disable the gate the operator thought
    /// they turned on. Same contract for the source URLs — a malformed one is
    /// a startup error rather than a warning nobody reads at 03:00.
    pub fn validate_email_policy(&self) -> Result<()> {
        fn err(msg: String) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg))
        }
        let p = &self.email_policy;
        if !p.enabled {
            return Ok(());
        }
        if p.signup_policy().is_none() {
            return Err(err(format!(
                "email_policy.signup_policy must be one of allow/flag/block (got {:?})",
                p.signup_policy
            )));
        }
        if p.sources.is_empty() {
            return Err(err(
                "email_policy.enabled = true needs at least one entry in email_policy.sources"
                    .to_string(),
            ));
        }
        for raw in &p.sources {
            match url::Url::parse(raw) {
                Ok(u) if u.scheme() == "https" => {}
                _ => {
                    return Err(err(format!(
                        "email_policy.sources entries must be https:// URLs (got {raw:?})"
                    )));
                }
            }
        }
        if p.min_domains == 0 || p.min_domains >= p.max_domains {
            return Err(err(format!(
                "email_policy.min_domains ({}) must be > 0 and below max_domains ({})",
                p.min_domains, p.max_domains
            )));
        }
        if p.max_shrink_pct > 100 {
            return Err(err("email_policy.max_shrink_pct must be 0-100".to_string()));
        }
        Ok(())
    }

    /// A half-configured Resend sender is a clean startup error, not a
    /// per-send failure after cutover. The webhook secret alone is fine —
    /// the bounce receiver works regardless of the sending provider.
    pub fn validate_email(&self) -> Result<()> {
        fn err(msg: &str) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg.to_string()))
        }
        let e = &self.email;
        if e.provider == EmailProvider::Memory {
            return Err(err(
                "email.provider = \"memory\" keeps every mail in process for tests; use \"log\" or \"resend\"",
            ));
        }
        // The sender feeds the operator-domain block under every provider,
        // so its shape is checked whenever it is set at all.
        if !e.from_address.is_empty() {
            let Some((local, _)) = mailbox::parse_bare(&e.from_address) else {
                return Err(err(
                    "email.from_address must be one bare user@domain address: no display name, brackets, quotes or whitespace",
                ));
            };
            if mailbox::is_no_reply(local) {
                tracing::warn!(
                    from_address = %e.from_address,
                    "email.from_address is a no-reply sender; send from an inbox somebody reads"
                );
            }
        }
        if e.support_enabled() && mailbox::parse_bare(&e.support_address).is_none() {
            return Err(err(
                "email.support_address must be one bare user@domain address: no display name, brackets, quotes or whitespace",
            ));
        }
        if e.provider != EmailProvider::Resend {
            return Ok(());
        }
        if e.resend.api_key.expose_secret().trim().is_empty() {
            return Err(err(
                "email.resend.api_key is required when email.provider = \"resend\"",
            ));
        }
        if e.from_address.is_empty() {
            return Err(err(
                "email.from_address is required when email.provider = \"resend\"",
            ));
        }
        Ok(())
    }

    /// The URL builder has no fallback on purpose: defaulting a mistyped
    /// single-tenant lock to `common` would admit every Microsoft account on
    /// earth without a word in the log.
    pub fn validate_microsoft_oauth(&self) -> Result<()> {
        let m = &self.auth.microsoft;
        if !m.client.is_configured() {
            return Ok(());
        }
        if !m.tenant_is_valid() {
            return Err(crate::error::AppError::Other(anyhow::anyhow!(
                "auth.microsoft.tenant {:?} is not addressable — use \"common\", \"organizations\", \"consumers\", a tenant GUID, or a domain",
                m.tenant
            )));
        }
        Ok(())
    }

    /// The URL builder has no fallback on purpose: defaulting a mistyped
    /// instance to gitlab.com would orphan every identity minted so far.
    pub fn validate_gitlab_oauth(&self) -> Result<()> {
        let g = &self.auth.gitlab;
        if !g.client.is_configured() {
            return Ok(());
        }
        if !g.base_url_is_valid() {
            return Err(crate::error::AppError::Other(anyhow::anyhow!(
                "auth.gitlab.base_url {:?} is not an https origin — use \"https://gitlab.com\" or your instance's own https URL",
                g.base_url
            )));
        }
        Ok(())
    }

    /// A half-configured operator WhatsApp number is a clean startup error,
    /// not a dead webhook or a failing send after the flag flip.
    pub fn validate_whatsapp_app(&self) -> Result<()> {
        fn err(msg: &str) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg.to_string()))
        }
        let w = &self.whatsapp_app;
        if !w.enabled {
            return Ok(());
        }
        if w.access_token.expose_secret().trim().is_empty()
            || w.phone_number_id.trim().is_empty()
            || w.app_secret.expose_secret().trim().is_empty()
            || w.verify_token.expose_secret().trim().is_empty()
        {
            return Err(err(
                "whatsapp_app.enabled needs access_token, phone_number_id, app_secret and verify_token set",
            ));
        }
        if w.verify_token.expose_secret().trim().len() < 32 {
            return Err(err(
                "UPTIMEPAGE_WHATSAPP_APP__VERIFY_TOKEN must be at least 32 chars",
            ));
        }
        let n = w.public_number.trim();
        if !(5..=20).contains(&n.len()) || !n.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err(
                "whatsapp_app.public_number must be the display number as international digits",
            ));
        }
        if w.template_name.is_empty()
            || !w
                .template_name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(err(
                "whatsapp_app.template_name is required (lowercase letters, digits, and _ only)",
            ));
        }
        if w.language_code.is_empty()
            || !w
                .language_code
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(err(
                "whatsapp_app.language_code must be a code like en or en_US",
            ));
        }
        // Deliberate: sends are operator-paid Meta template messages with no
        // per-org cap yet — the flag flip is the only spend control.
        tracing::warn!(
            "whatsapp_app.enabled — operator-paid template sends are UNCAPPED; \
             monitor spend until per-org send caps land"
        );
        Ok(())
    }

    /// Validate the regional-agent section. Only enforced when `agent.enabled`.
    pub fn validate_runtime(&self) -> Result<()> {
        fn err(msg: &str) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg.to_string()))
        }
        let agent = &self.agent;
        if !agent.enabled {
            return Ok(());
        }
        if agent.control_plane_url.trim().is_empty() {
            return Err(err(
                "agent.control_plane_url is required when agent.enabled",
            ));
        }
        // Resolved secrets and decrypted credentials ride the config-pull
        // response, so the control-plane transport must be encrypted. Cleartext
        // is permitted only when private targets are explicitly opted in (a
        // trusted private-network or localhost control plane for dev/integration).
        let url = url::Url::parse(agent.control_plane_url.trim())
            .map_err(|_| err("agent.control_plane_url is not a valid URL"))?;
        if url.scheme() != "https" && !self.security.allow_private_targets {
            return Err(err(
                "agent.control_plane_url must use https; cleartext is permitted \
                 only with security.allow_private_targets for a trusted \
                 private-network or localhost control plane",
            ));
        }
        if agent.region.trim().is_empty() {
            return Err(err("agent.region is required when agent.enabled"));
        }
        if agent.token.expose_secret().trim().is_empty() {
            return Err(err(
                "UPTIMEPAGE_AGENT__TOKEN is required when agent.enabled is true",
            ));
        }
        if agent.pull_interval_secs == 0 {
            return Err(err("agent.pull_interval_secs must be > 0"));
        }
        if agent.buffer_capacity == 0 {
            return Err(err("agent.buffer_capacity must be > 0"));
        }
        Ok(())
    }

    /// Reject the published `monitor` credentials; unoverridden they expose every tenant row.
    pub fn validate_storage(&self) -> Result<()> {
        const SHIPPED: &str = "monitor";
        const OPT_IN: &str = "set UPTIMEPAGE_STORAGE__ALLOW_DEFAULT_CREDENTIALS=true \
                              for a local stack";
        fn err(msg: &str) -> crate::error::AppError {
            crate::error::AppError::Other(anyhow::anyhow!(msg.to_string()))
        }
        if self.storage.allow_default_credentials {
            return Ok(());
        }
        let pg = url::Url::parse(&self.storage.postgres.url)
            .map_err(|_| err("storage.postgres.url is not a valid URL"))?;
        if pg.username() == SHIPPED && pg.password() == Some(SHIPPED) {
            return Err(err(&format!(
                "storage.postgres.url still carries the shipped credentials; \
                 set UPTIMEPAGE_STORAGE__POSTGRES__URL, or {OPT_IN}"
            )));
        }
        let ch = self.storage.clickhouse.password.expose_secret();
        if ch.is_empty() || ch == SHIPPED {
            return Err(err(&format!(
                "storage.clickhouse.password is empty or still the shipped value; \
                 set UPTIMEPAGE_STORAGE__CLICKHOUSE__PASSWORD, or {OPT_IN}"
            )));
        }
        Ok(())
    }
}
