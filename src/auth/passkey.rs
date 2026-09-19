//! Passkeys, the one credential this deployment mints rather than a vendor.
//!
//! Registration is discoverable, so the credential carries the user id and the
//! sign-in page never asks for an address. Asking would answer whether an
//! address has a passkey, the enumeration oracle magic links avoid.

use url::Url;
use webauthn_rs::prelude::*;

use crate::config::AuthConfig;
use crate::error::{AppError, Result};

/// Long enough to pick a device and touch it, short enough that a stolen row is
/// worth nothing.
pub const CEREMONY_TTL_SECONDS: i64 = 300;

/// Derived rather than configured, so the id every credential is bound to cannot
/// drift from the origin the browser reports.
pub fn relying_party_id(public_base_url: &str) -> Result<String> {
    Ok(parse_base(public_base_url)?
        .host_str()
        .ok_or_else(|| bad_base("has no host"))?
        .to_string())
}

/// The list is the policy switch; the capability half is asked of the builder,
/// which rejects a base URL naming an IP rather than a domain.
pub fn login_enabled(cfg: &AuthConfig) -> bool {
    cfg.method_enabled("passkey") && build(&cfg.public_base_url).is_ok()
}

/// Built once at startup and held in state. Subdomains stay excluded: on the
/// SaaS layout the tenant status pages are siblings of the app host, and a
/// credential minted for one should not answer for another.
pub fn build(public_base_url: &str) -> Result<Webauthn> {
    let origin = parse_base(public_base_url)?;
    let rp_id = relying_party_id(public_base_url)?;
    WebauthnBuilder::new(&rp_id, &origin)
        .and_then(|b| b.rp_name(&rp_id).build())
        .map_err(|e| AppError::Other(anyhow::anyhow!("build webauthn for {rp_id:?}: {e}")))
}

fn parse_base(public_base_url: &str) -> Result<Url> {
    Url::parse(public_base_url.trim_end_matches('/')).map_err(|e| bad_base(&format!("{e}")))
}

fn bad_base(why: &str) -> AppError {
    AppError::Other(anyhow::anyhow!("auth.public_base_url {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relying_party_is_the_host_alone() {
        assert_eq!(
            relying_party_id("https://app.uptimepage.dev").unwrap(),
            "app.uptimepage.dev"
        );
    }

    #[test]
    fn a_port_and_trailing_slash_do_not_reach_the_relying_party_id() {
        // The browser reports the host without either, so carrying them here
        // would fail every ceremony on a dev deployment.
        assert_eq!(
            relying_party_id("http://localhost:8080/").unwrap(),
            "localhost"
        );
    }

    #[test]
    fn a_base_url_without_a_host_is_refused() {
        assert!(relying_party_id("not-a-url").is_err());
    }

    #[test]
    fn the_dev_default_builds() {
        assert!(build("http://localhost:8080").is_ok());
    }
}
