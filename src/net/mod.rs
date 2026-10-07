//! Transport-layer primitives that aren't tied to a specific protocol.
//! Currently just `happy_eyeballs`, used by both the check-path connector
//! and the outbound connector to race v6/v4 connects.

pub mod happy_eyeballs;
pub mod host;

use std::io;

/// A connect that failed before a packet could leave this host: it says
/// nothing about the target.
pub fn egress_is_broken(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::AddrNotAvailable
    )
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_that_never_left_this_host_is_ours() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::NetworkUnreachable,
            io::ErrorKind::NetworkDown,
            io::ErrorKind::AddrNotAvailable,
        ] {
            assert!(egress_is_broken(&io::Error::from(kind)), "{kind:?}");
        }
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut,
            io::ErrorKind::HostUnreachable,
        ] {
            assert!(!egress_is_broken(&io::Error::from(kind)), "{kind:?}");
        }
    }
}
