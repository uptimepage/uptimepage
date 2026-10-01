//! Domain registration lookup across transports: RDAP first, WHOIS where the
//! bootstrap has no entry. Registries that publish no expiry at all are
//! reported as such rather than retried forever.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::error::{AppError, Result};
use crate::http_client::HttpClients;
use crate::worker::rdap::RdapClient;
use crate::worker::whois::WhoisClient;

#[derive(Debug, Clone)]
pub struct RegistrationAnswer {
    pub expiration: DateTime<Utc>,
    pub registrar: Option<String>,
}

/// Permanent verdicts about the TLD, not about the attempt.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("no registration lookup available for TLD '.{tld}'")]
    TldUnsupported { tld: String },

    #[error("the .{tld} registry does not publish domain expiry dates")]
    NoPublicExpiry { tld: String },
}

impl From<RegistrationError> for AppError {
    fn from(err: RegistrationError) -> Self {
        AppError::Other(err.into())
    }
}

/// Saves callers from matching on message strings.
pub fn tld_verdict(err: &anyhow::Error) -> Option<&RegistrationError> {
    err.downcast_ref::<RegistrationError>()
}

pub struct RegistrationClient {
    rdap: Arc<RdapClient>,
    whois: WhoisClient,
}

impl RegistrationClient {
    pub fn new(rdap: Arc<RdapClient>) -> Self {
        Self {
            rdap,
            whois: WhoisClient::new(),
        }
    }

    #[doc(hidden)]
    pub fn with_whois(rdap: Arc<RdapClient>, whois: WhoisClient) -> Self {
        Self { rdap, whois }
    }

    pub async fn lookup_expiration(
        &self,
        domain: &str,
        clients: &HttpClients,
    ) -> Result<RegistrationAnswer> {
        let registrable = crate::domain::registered_domain(domain);
        let Some(tld) = registrable.rsplit('.').next().filter(|t| !t.is_empty()) else {
            return Err(AppError::Other(anyhow::anyhow!(
                "domain '{domain}' missing TLD"
            )));
        };

        if crate::domain::publishes_no_expiry(tld) {
            return Err(RegistrationError::NoPublicExpiry {
                tld: tld.to_owned(),
            }
            .into());
        }

        match self.rdap.lookup_expiration(&registrable, tld).await {
            Err(AppError::Other(e))
                if matches!(
                    tld_verdict(&e),
                    Some(RegistrationError::TldUnsupported { .. })
                ) =>
            {
                // Only a missing server falls through. Retrying a transport
                // failure over WHOIS doubles load during an outage for nothing.
                self.whois
                    .lookup_expiration(&registrable, tld, clients)
                    .await
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tld_verdict_survives_anyhow_wrapping() {
        let err: anyhow::Error = RegistrationError::NoPublicExpiry { tld: "de".into() }.into();
        let wrapped = err.context("probing registration");
        assert!(matches!(
            tld_verdict(&wrapped),
            Some(RegistrationError::NoPublicExpiry { .. })
        ));
    }

    #[test]
    fn unrelated_errors_have_no_verdict() {
        let err = anyhow::anyhow!("connection reset");
        assert!(tld_verdict(&err).is_none());
    }
}
