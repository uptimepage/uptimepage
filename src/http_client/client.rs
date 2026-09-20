use std::sync::Arc;
use std::time::Duration;

use metrics::{Histogram, histogram};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, OtherError, RootCertStore,
    SignatureScheme,
};
use tokio_rustls::TlsConnector;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::oid_registry::OID_PKIX_ACCESS_DESCRIPTOR_CA_ISSUERS;
use x509_parser::prelude::FromDer;

use crate::config::{CheckerConfig, DnsConfig, HttpClientConfig, SecurityConfig};
use crate::error::{AppError, Result};
use crate::http_client::connector::ConnectParams;
use crate::http_client::dns::HickoryDnsResolver;
use crate::metric_names;
use crate::security::SsrfGuard;
use crate::security::cert_probe::NoVerify;

/// Shared, cheaply-clonable handles for the check path. Holds no connection
/// pool: every HTTP check connects fresh (a monitor probes each target once per
/// interval, so pooling rarely reused a socket — and fresh-connect is what lets
/// the probe time DNS/connect/TLS per check). The two TLS connectors differ
/// only in cert verification.
#[derive(Clone)]
pub struct HttpClients {
    tls_verifying: Arc<TlsConnector>,
    tls_insecure: Arc<TlsConnector>,
    pub(crate) ttfb_ms: Histogram,
    connect_ms: Histogram,
    tls_ms: Histogram,
    pub(crate) user_agent: Arc<str>,
    pub(crate) resolver: Arc<HickoryDnsResolver>,
    pub(crate) ssrf_guard: SsrfGuard,
    connect_timeout: Duration,
    tcp_keepalive: Option<Duration>,
}

impl HttpClients {
    pub(crate) fn connect_params(&self, verify_tls: bool) -> ConnectParams<'_> {
        ConnectParams {
            resolver: &self.resolver,
            ssrf_guard: self.ssrf_guard,
            tls: if verify_tls {
                &self.tls_verifying
            } else {
                &self.tls_insecure
            },
            connect_ms: &self.connect_ms,
            tls_ms: &self.tls_ms,
            connect_timeout: self.connect_timeout,
            tcp_keepalive: self.tcp_keepalive,
        }
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    pub fn resolver(&self) -> &Arc<HickoryDnsResolver> {
        &self.resolver
    }

    pub fn ssrf_guard(&self) -> SsrfGuard {
        self.ssrf_guard
    }
}

pub fn build_clients(
    http_cfg: &HttpClientConfig,
    checker_cfg: &CheckerConfig,
    dns_cfg: &DnsConfig,
    security_cfg: &SecurityConfig,
) -> Result<HttpClients> {
    install_default_crypto_provider();

    let resolver = Arc::new(HickoryDnsResolver::new(dns_cfg)?);
    let ssrf_guard = SsrfGuard::new(security_cfg.allow_private_targets);
    let tls_verifying = Arc::new(TlsConnector::from(Arc::new(build_tls_config(true)?)));
    let tls_insecure = Arc::new(TlsConnector::from(Arc::new(build_tls_config(false)?)));

    Ok(HttpClients {
        tls_verifying,
        tls_insecure,
        ttfb_ms: histogram!(metric_names::CHECK_TTFB_MS),
        connect_ms: histogram!(metric_names::CHECK_CONNECT_MS),
        tls_ms: histogram!(metric_names::CHECK_TLS_MS),
        user_agent: Arc::from(http_cfg.user_agent.as_str()),
        resolver,
        ssrf_guard,
        connect_timeout: Duration::from_millis(checker_cfg.connect_timeout_ms),
        tcp_keepalive: Some(Duration::from_secs(http_cfg.tcp_keepalive_secs))
            .filter(|d| !d.is_zero()),
    })
}

fn build_tls_config(verify: bool) -> Result<ClientConfig> {
    let mut cfg = if verify {
        let webpki = WebPkiServerVerifier::builder(Arc::new(server_roots()))
            .build()
            .map_err(|e| AppError::Other(anyhow::anyhow!("server cert verifier: {e}")))?;
        // `dangerous()` is the only door to a wrapper; what goes through it
        // is the stock webpki verifier plus a rename of one rejection.
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(DiagnosingVerifier(webpki)))
            .with_no_client_auth()
    } else {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth()
    };

    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(cfg)
}

/// Platform roots, falling back to the bundled Mozilla set when the store
/// yields nothing parsable: an empty root store fails every HTTPS check
/// identically and blames the targets.
fn server_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        tracing::warn!(errors = ?native.errors, "native root CA load errors");
    }
    let (added, _) = roots.add_parsable_certificates(native.certs);
    if added == 0 {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    roots
}

/// The `UnknownIssuer` cases a reader can act on. Both reach the customer as
/// "certificate not trusted" otherwise, which names the symptom and hides what
/// the reader has to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainFault {
    /// One cert, not self-issued, naming its issuer over AIA. The server
    /// withheld the intermediate, or its CA is one we do not carry; the leaf
    /// alone cannot say which, so the wording names neither. Browsers fetch
    /// the AIA cert and paper over the first case.
    Incomplete,
    SelfSigned,
}

impl std::fmt::Display for ChainFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Incomplete => "server sent only its own certificate",
            Self::SelfSigned => "certificate is self-signed",
        })
    }
}

impl std::error::Error for ChainFault {}

/// Splits `UnknownIssuer` into the [`ChainFault`] cases, which needs the peer
/// chain and so has to happen inside verification. Only ever renames a
/// rejection, so it cannot widen trust.
#[derive(Debug)]
struct DiagnosingVerifier(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for DiagnosingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let err = match self.0.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => return Ok(verified),
            Err(e) => e,
        };
        // Either fault takes the same shape: a chain truncated below the leaf.
        // Anything longer that still fails is a root we do not carry.
        if !intermediates.is_empty()
            || !matches!(
                err,
                rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer)
            )
        {
            return Err(err);
        }
        match diagnose_lone_leaf(end_entity) {
            // Trades the `unknown_ca` alert for `certificate_unknown`. Both are
            // fatal and the peer has already sent everything it is going to.
            Some(fault) => Err(rustls::Error::InvalidCertificate(CertificateError::Other(
                OtherError(Arc::new(fault)),
            ))),
            None => Err(err),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.0.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[rustls::DistinguishedName]> {
        self.0.root_hint_subjects()
    }
}

/// Self-issued means self-signed. Otherwise an AIA `caIssuers` pointer puts
/// the gap in what the server sent rather than in who signed it.
fn diagnose_lone_leaf(leaf: &CertificateDer<'_>) -> Option<ChainFault> {
    let (_, cert) = X509Certificate::from_der(leaf.as_ref()).ok()?;
    if cert.subject() == cert.issuer() {
        return Some(ChainFault::SelfSigned);
    }
    cert.extensions()
        .iter()
        .filter_map(|e| match e.parsed_extension() {
            ParsedExtension::AuthorityInfoAccess(aia) => Some(aia),
            _ => None,
        })
        .flat_map(|aia| aia.iter())
        .any(|d| d.access_method == OID_PKIX_ACCESS_DESCRIPTOR_CA_ISSUERS)
        .then_some(ChainFault::Incomplete)
}

pub(crate) fn install_default_crypto_provider() {
    if CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, DnType, IsCa, Issuer, KeyPair,
    };

    /// `AuthorityInfoAccessSyntax` with one id-ad-caIssuers URI. Hand-rolled
    /// because rcgen models no AIA.
    fn aia_ca_issuers_der(uri: &str) -> Vec<u8> {
        const CA_ISSUERS: [u8; 10] = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x02];
        let mut desc = CA_ISSUERS.to_vec();
        desc.push(0x86);
        desc.push(u8::try_from(uri.len()).expect("short test uri"));
        desc.extend_from_slice(uri.as_bytes());

        let mut inner = vec![0x30, u8::try_from(desc.len()).expect("short desc")];
        inner.extend_from_slice(&desc);
        let mut out = vec![0x30, u8::try_from(inner.len()).expect("short aia")];
        out.extend_from_slice(&inner);
        out
    }

    fn ca_issued_leaf(aia_uri: Option<&str>) -> CertificateDer<'static> {
        let ca_key = KeyPair::generate().expect("ca key");
        let mut ca_params = CertificateParams::new(Vec::new()).expect("ca params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "Test Issuing CA");
        let issuer = Issuer::from_params(&ca_params, &ca_key);

        let key = KeyPair::generate().expect("leaf key");
        let mut params =
            CertificateParams::new(vec!["example.test".to_string()]).expect("leaf params");
        params
            .distinguished_name
            .push(DnType::CommonName, "example.test");
        if let Some(uri) = aia_uri {
            params
                .custom_extensions
                .push(CustomExtension::from_oid_content(
                    &[1, 3, 6, 1, 5, 5, 7, 1, 1],
                    aia_ca_issuers_der(uri),
                ));
        }
        params
            .signed_by(&key, &issuer)
            .expect("leaf cert")
            .der()
            .clone()
    }

    #[test]
    fn self_issued_leaf_reads_as_self_signed() {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(vec!["example.test".to_string()]).expect("params");
        params
            .distinguished_name
            .push(DnType::CommonName, "example.test");
        let cert = params.self_signed(&key).expect("cert");
        assert_eq!(diagnose_lone_leaf(cert.der()), Some(ChainFault::SelfSigned));
    }

    #[test]
    fn ca_issued_leaf_with_aia_reads_as_truncated_chain() {
        assert_eq!(
            diagnose_lone_leaf(&ca_issued_leaf(Some("http://ca.test/i.crt"))),
            Some(ChainFault::Incomplete)
        );
    }

    /// A CA that publishes nothing is untrusted rather than truncated.
    #[test]
    fn ca_issued_leaf_without_aia_stays_unclassified() {
        assert_eq!(diagnose_lone_leaf(&ca_issued_leaf(None)), None);
    }

    #[test]
    fn server_roots_are_never_empty() {
        assert!(!server_roots().is_empty());
    }
}
