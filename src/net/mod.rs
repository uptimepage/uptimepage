//! Transport-layer primitives that aren't tied to a specific protocol.
//! Currently just `happy_eyeballs`, used by both the check-path connector
//! and the outbound connector to race v6/v4 connects.

pub mod happy_eyeballs;

/// Loopback per RFC 8252 §7.3, on the typed host so `[::1]` counts.
pub fn is_loopback_http(u: &url::Url) -> bool {
    use url::Host;
    u.scheme() == "http"
        && match u.host() {
            Some(Host::Domain(d)) => d == "localhost",
            Some(Host::Ipv4(v4)) => v4.is_loopback(),
            Some(Host::Ipv6(v6)) => v6.is_loopback(),
            None => false,
        }
}
