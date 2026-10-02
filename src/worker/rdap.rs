use std::collections::HashMap;

use anyhow::{Context, anyhow};
use serde::Deserialize;
use tokio::sync::OnceCell;
use url::Url;

use crate::error::Result;
use crate::http_outbound::{OutboundHttpClient, get_json};
use crate::security::rdap::{DomainResponse, override_url};
use crate::worker::registration::types::{RegistrationAnswer, RegistrationError};

/// IANA-published RDAP bootstrap registry for DNS. Maps TLDs to one or more
/// RDAP server base URLs. Public and rarely changes.
const IANA_BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";

pub struct RdapClient {
    http: OutboundHttpClient,
    bootstrap_url: String,
    bootstrap: OnceCell<BootstrapMap>,
}

impl RdapClient {
    pub fn new(http: OutboundHttpClient) -> Self {
        Self {
            http,
            bootstrap_url: IANA_BOOTSTRAP_URL.to_owned(),
            bootstrap: OnceCell::new(),
        }
    }

    /// Constructor used by integration tests to point the client at a fixture
    /// bootstrap registry hosted by a local axum server. Not `#[cfg(test)]`
    /// because integration tests live in separate binaries that don't see the
    /// crate's test cfg.
    #[doc(hidden)]
    pub fn with_bootstrap_url(http: OutboundHttpClient, bootstrap_url: String) -> Self {
        Self {
            http,
            bootstrap_url,
            bootstrap: OnceCell::new(),
        }
    }

    /// `registrable` is the public suffix plus one label: expiry is a property
    /// of that name, not of a subdomain. The caller reduces it.
    pub async fn lookup_expiration(
        &self,
        registrable: &str,
        tld: &str,
    ) -> Result<RegistrationAnswer> {
        let base = match override_url(tld) {
            Some(base) => base.to_owned(),
            None => self
                .bootstrap()
                .await?
                .by_tld
                .get(tld)
                .cloned()
                .ok_or_else(|| {
                    crate::error::AppError::from(RegistrationError::TldUnsupported {
                        tld: tld.to_owned(),
                    })
                })?,
        };
        let mut url =
            Url::parse(&base).with_context(|| format!("invalid RDAP base url '{base}'"))?;
        // RDAP base URLs are typically registered with a trailing slash; the
        // path is `domain/<name>`. Normalise either way.
        if !url.path().ends_with('/') {
            let new_path = format!("{}/", url.path());
            url.set_path(&new_path);
        }
        let url = url
            .join(&format!("domain/{registrable}"))
            .with_context(|| format!("building RDAP request for '{registrable}'"))?;

        let resp: DomainResponse = get_json(&self.http, &url).await?;
        let expiration = resp.expiration().ok_or_else(|| {
            crate::error::AppError::Other(anyhow!("no expiration event in RDAP response"))
        })?;
        Ok(RegistrationAnswer {
            expiration,
            registrar: resp.registrar(),
        })
    }

    async fn bootstrap(&self) -> Result<&BootstrapMap> {
        self.bootstrap
            .get_or_try_init(|| async { fetch_bootstrap(&self.http, &self.bootstrap_url).await })
            .await
    }
}

#[derive(Debug)]
pub struct BootstrapMap {
    by_tld: HashMap<String, String>,
}

#[derive(Deserialize)]
struct RawBootstrap {
    services: Vec<(Vec<String>, Vec<String>)>,
}

async fn fetch_bootstrap(client: &OutboundHttpClient, url: &str) -> Result<BootstrapMap> {
    let parsed = Url::parse(url).context("parsing IANA bootstrap URL")?;
    let raw: RawBootstrap = get_json(client, &parsed).await?;
    let mut by_tld = HashMap::with_capacity(raw.services.len() * 2);
    for (tlds, servers) in raw.services {
        let Some(server) = servers.into_iter().next() else {
            continue;
        };
        for tld in tlds {
            by_tld.insert(tld.to_ascii_lowercase(), server.clone());
        }
    }
    Ok(BootstrapMap { by_tld })
}
