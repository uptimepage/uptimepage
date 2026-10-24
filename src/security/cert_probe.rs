//! Reads a TLS leaf certificate and reports what it says.
//!
//! Deliberately standalone: it reads from a stream the caller opened to an
//! address it already passed through [`SsrfGuard`](super::SsrfGuard), so it
//! carries no resolver, no metrics and no application state.

use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, TimeZone, Utc};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use thiserror::Error;
use tokio_rustls::TlsConnector;
use x509_parser::asn1_rs::{Any, Tag};
use x509_parser::prelude::*;

#[derive(Debug, Error)]
pub enum CertProbeError {
    #[error("invalid server name '{0}'")]
    ServerName(String),
    #[error("tls handshake failed")]
    Handshake(#[source] io::Error),
    #[error("server returned no certificate chain")]
    NoChain,
    #[error("parsing leaf certificate: {0}")]
    Parse(String),
    #[error("certificate validity is out of representable range")]
    Validity,
}

/// What a leaf certificate says, flattened into the fields a monitor grades on
/// and a person reads.
#[derive(Debug, Clone)]
pub struct CertFacts {
    pub subject_common_name: Option<String>,
    pub issuer_common_name: Option<String>,
    pub issuer_organization: Option<String>,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub days_remaining: i64,
    pub san_dns_names: Vec<String>,
    pub serial: String,
    /// Whether the name we asked for is covered by the certificate's SANs,
    /// falling back to the subject CN when the certificate carries no SAN.
    pub name_matches: bool,
    pub self_signed: bool,
    pub chain_len: usize,
    pub handshake_ms: u32,
    pub peer: IpAddr,
}

/// Completes a TLS handshake over an already-open stream and parses the leaf.
///
/// Certificate validation is deliberately skipped: an expired or self-signed
/// leaf is exactly what the caller needs to read and report on, and a verifying
/// handshake would refuse it and lose the dates.
pub async fn read_cert<S>(
    stream: S,
    server_name: &str,
    peer: IpAddr,
) -> Result<CertFacts, CertProbeError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let dns_name = ServerName::try_from(server_name.to_owned())
        .map_err(|_| CertProbeError::ServerName(server_name.to_owned()))?;

    let tls_config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));

    let handshake_start = Instant::now();
    let tls = connector
        .connect(dns_name, stream)
        .await
        .map_err(CertProbeError::Handshake)?;
    let handshake_ms = handshake_start.elapsed().as_millis() as u32;

    let (_io, session) = tls.get_ref();
    let chain = session.peer_certificates().ok_or(CertProbeError::NoChain)?;
    let leaf = chain.first().ok_or(CertProbeError::NoChain)?;
    parse_leaf(leaf, chain.len(), server_name, handshake_ms, peer)
}

fn parse_leaf(
    leaf: &CertificateDer<'_>,
    chain_len: usize,
    server_name: &str,
    handshake_ms: u32,
    peer: IpAddr,
) -> Result<CertFacts, CertProbeError> {
    let (_, parsed) = X509Certificate::from_der(leaf.as_ref())
        .map_err(|e| CertProbeError::Parse(e.to_string()))?;

    let not_after = to_utc(parsed.validity().not_after.timestamp())?;
    let not_before = to_utc(parsed.validity().not_before.timestamp())?;
    let days_remaining = (not_after - Utc::now()).num_days();

    let subject_common_name = first_cn(parsed.subject());
    let issuer_common_name = first_cn(parsed.issuer());
    let issuer_organization = first_organization(parsed.issuer());
    let san_dns_names = san_dns_names(&parsed);

    let name_matches = match san_dns_names.as_slice() {
        // A certificate with no SAN at all predates the extension being
        // mandatory; browsers reject it outright, but reading the CN still
        // tells the visitor which name it was issued for.
        [] => subject_common_name
            .as_deref()
            .is_some_and(|cn| name_covers(cn, server_name)),
        sans => sans.iter().any(|san| name_covers(san, server_name)),
    };

    Ok(CertFacts {
        self_signed: parsed.subject() == parsed.issuer(),
        subject_common_name,
        issuer_common_name,
        issuer_organization,
        not_before,
        not_after,
        days_remaining,
        san_dns_names,
        serial: parsed.raw_serial_as_string(),
        name_matches,
        chain_len,
        handshake_ms,
        peer,
    })
}

fn to_utc(ts: i64) -> Result<DateTime<Utc>, CertProbeError> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .ok_or(CertProbeError::Validity)
}

fn first_cn(name: &X509Name<'_>) -> Option<String> {
    name.iter_common_name()
        .next()
        .and_then(|cn| attr_string(cn.attr_value()))
}

fn first_organization(name: &X509Name<'_>) -> Option<String> {
    name.iter_organization()
        .next()
        .and_then(|o| attr_string(o.attr_value()))
}

/// `as_str` reads only the modern string types, and older CAs wrote names in
/// the rest. TeletexString carries UTF-8 from some CAs and Latin-1 from the
/// oldest ones.
fn attr_string(value: &Any<'_>) -> Option<String> {
    let data = value.as_bytes();
    match value.tag() {
        Tag::T61String => Some(match std::str::from_utf8(data) {
            Ok(s) => s.to_owned(),
            Err(_) => data.iter().copied().map(char::from).collect(),
        }),
        Tag::BmpString => value.as_bmpstring().ok().map(|s| s.string()),
        Tag::UniversalString => value.as_universalstring().ok().map(|s| s.string()),
        Tag::NumericString
        | Tag::PrintableString
        | Tag::Utf8String
        | Tag::Ia5String
        | Tag::VisibleString
        | Tag::GeneralString
        | Tag::GraphicString
        | Tag::VideotexString => std::str::from_utf8(data).ok().map(str::to_owned),
        _ => None,
    }
}

fn san_dns_names(cert: &X509Certificate<'_>) -> Vec<String> {
    let Ok(Some(san)) = cert.subject_alternative_name() else {
        return Vec::new();
    };
    san.value
        .general_names
        .iter()
        .filter_map(|n| match n {
            GeneralName::DNSName(d) => Some((*d).to_owned()),
            _ => None,
        })
        .collect()
}

/// RFC 6125 name matching, restricted to the shape certificate authorities
/// actually issue: an exact match, or one leading `*` standing for exactly one
/// label. A wildcard never spans a dot, so `*.acme.com` does not cover
/// `a.b.acme.com`.
fn name_covers(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .split_once('.')
            .is_some_and(|(_, rest)| rest == suffix && !suffix.is_empty()),
        None => pattern == host,
    }
}

/// Accepts every chain. Used by the `verify_tls = false` HTTP client path and
/// by every certificate read here, which must see expired and self-signed
/// leaves rather than have the handshake refuse them.
#[derive(Debug)]
pub(crate) struct NoVerify;

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_covers_one_label_only() {
        assert!(name_covers("*.acme.com", "www.acme.com"));
        assert!(!name_covers("*.acme.com", "a.b.acme.com"));
        assert!(!name_covers("*.acme.com", "acme.com"));
    }

    #[test]
    fn exact_match_ignores_case_and_trailing_dot() {
        assert!(name_covers("ACME.com", "acme.com."));
        assert!(!name_covers("acme.com", "acme.org"));
    }

    #[test]
    fn names_in_legacy_string_types_are_read() {
        use rcgen::string::{BmpString, TeletexString};
        use rcgen::{CertificateParams, DnType, DnValue, KeyPair};

        let mut params = CertificateParams::new(Vec::new()).expect("params");
        params.distinguished_name.push(
            DnType::CommonName,
            DnValue::TeletexString(TeletexString::try_from("*.legacy.test").unwrap()),
        );
        params.distinguished_name.push(
            DnType::OrganizationName,
            DnValue::BmpString(BmpString::try_from("Київстар").unwrap()),
        );
        let cert = params
            .self_signed(&KeyPair::generate().expect("key"))
            .expect("cert");
        let (_, parsed) = X509Certificate::from_der(cert.der()).expect("parse");

        assert_eq!(first_cn(parsed.subject()).as_deref(), Some("*.legacy.test"));
        assert_eq!(
            first_organization(parsed.subject()).as_deref(),
            Some("Київстар")
        );
    }

    #[test]
    fn teletex_reads_utf8_and_falls_back_to_latin1() {
        let t61 = |data| attr_string(&Any::from_tag_and_data(Tag::T61String, data));
        assert_eq!(t61("Львів".as_bytes()).as_deref(), Some("Львів"));
        assert_eq!(t61(b"Caf\xe9 Ltd").as_deref(), Some("Café Ltd"));
    }

    #[test]
    fn every_string_type_reads_and_others_do_not() {
        let read = |tag, data: &[u8]| attr_string(&Any::from_tag_and_data(tag, data));
        let ucs4: Vec<u8> = "Київ"
            .chars()
            .flat_map(|c| u32::from(c).to_be_bytes())
            .collect();
        assert_eq!(read(Tag::UniversalString, &ucs4).as_deref(), Some("Київ"));
        for tag in [Tag::VisibleString, Tag::GeneralString, Tag::GraphicString] {
            assert_eq!(
                read(tag, b"Legacy CA").as_deref(),
                Some("Legacy CA"),
                "{tag}"
            );
        }
        assert_eq!(read(Tag::OctetString, b"Legacy CA"), None);
    }
}
