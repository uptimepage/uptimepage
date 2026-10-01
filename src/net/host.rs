//! Host-name canonical forms shared by the check path and the ingest boundary.

/// Canonical host key: IDN-encoded + ASCII-lowercased + trailing dot stripped.
/// Infallible — falls back to ASCII-lowercase when IDN encoding fails, so the
/// worker hot path never panics on unexpected input.
///
/// Used by:
/// - circuit-breaker key (`host_for_spec`) — `Example.COM`, `example.com.`,
///   `BÄHN.de`, and `xn--bhn-qla.de` share one breaker.
/// - per-(org, host, port) throttle key.
/// - per-TLD RDAP throttle (`rdap_tld`).
/// - cross-tenant RDAP singleflight cache key.
///
/// Use [`canonical_host_strict`] at the API ingest boundary to reject malformed
/// IDN with a 400 instead of silently falling back.
pub fn canonical_host(host: &str) -> String {
    let trimmed = host.trim_end_matches('.');
    idna::domain_to_ascii(trimmed).unwrap_or_else(|_| trimmed.to_ascii_lowercase())
}

/// Strict variant of [`canonical_host`] for use at the API ingest boundary.
/// Uses UTS46 with UseSTD3ASCIIRules, so leading/trailing hyphens, embedded
/// underscores, and other malformed IDN are rejected with a 400 instead of
/// silently stored.
pub fn canonical_host_strict(host: &str) -> Result<String, idna::Errors> {
    idna::domain_to_ascii_strict(host.trim_end_matches('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_normalization_collapses_trailing_dot_and_case() {
        assert_eq!(canonical_host("Example.COM."), "example.com");
        assert_eq!(canonical_host("example.com"), "example.com");
    }

    #[test]
    fn host_normalization_idn_round_trips_to_punycode() {
        let punycode = "xn--bhn-qla.de";
        assert_eq!(canonical_host("Bähn.de"), punycode);
        assert_eq!(canonical_host("BÄHN.de"), punycode);
        assert_eq!(canonical_host("bähn.de."), punycode);
        assert_eq!(canonical_host("xn--bhn-qla.de"), punycode);
    }

    #[test]
    fn canonical_host_strict_rejects_bad_idn() {
        assert!(canonical_host_strict("--invalid-leading.com").is_err());
    }
}
