use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SsrfError {
    #[error("target address {0} is in a blocked range")]
    BlockedAddress(IpAddr),
    #[error("all resolved addresses for '{host}' are in blocked ranges")]
    AllBlocked { host: String },
}

#[derive(Debug, Clone, Copy)]
pub struct SsrfGuard {
    allow_private: bool,
}

impl SsrfGuard {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }

    /// Production default: block every non-public address (loopback, RFC1918,
    /// link-local, cloud metadata, IPv6 outside `2000::/3`, 6to4/NAT64/Teredo
    /// with embedded private v4).
    pub fn strict() -> Self {
        Self::new(false)
    }

    /// Test-only: allow every address, including loopback. Use from
    /// integration tests that hit `127.0.0.1` listeners (mock SMTP, mock
    /// webhook receiver, etc.). Never call from production code paths.
    pub fn relaxed_for_tests() -> Self {
        Self::new(true)
    }

    /// For destinations an operator wrote into the config, never a user. The
    /// guard answers a user aiming our client somewhere they cannot reach, and
    /// nobody is confused when the origin came from whoever set the database
    /// URL. The alternative for a self-managed GitLab on RFC1918 is
    /// `allow_private_targets`, which opens the private range to every
    /// user-created monitor and webhook.
    pub fn operator_configured_target() -> Self {
        Self::new(true)
    }

    /// Build from the same `SecurityConfig` flag the check-path reads, so all
    /// production paths agree on what "private" means without each call site
    /// hard-coding the field name.
    pub fn from_security_config(cfg: &crate::config::SecurityConfig) -> Self {
        Self::new(cfg.allow_private_targets)
    }

    /// Returns true if the address is permitted by this guard.
    pub fn allow(&self, ip: IpAddr) -> bool {
        self.allow_private || !is_blocked_ip(ip)
    }

    pub fn check(&self, ip: IpAddr) -> Result<(), SsrfError> {
        if !self.allow(ip) {
            return Err(SsrfError::BlockedAddress(ip));
        }
        Ok(())
    }

    pub fn filter(&self, host: &str, addrs: Vec<IpAddr>) -> Result<Vec<IpAddr>, SsrfError> {
        if self.allow_private {
            return Ok(addrs);
        }
        let kept: Vec<IpAddr> = addrs.into_iter().filter(|ip| !is_blocked_ip(*ip)).collect();
        if kept.is_empty() {
            return Err(SsrfError::AllBlocked {
                host: host.to_string(),
            });
        }
        Ok(kept)
    }
}

/// True for IP addresses that should be blocked by default to prevent SSRF
/// against internal infrastructure (loopback, RFC1918, link-local, ULA,
/// multicast, broadcast, reserved, documentation, cloud metadata, etc.).
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    if ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
    {
        return true;
    }
    match ip.octets() {
        // 0.0.0.0/8 — "this network" (RFC 1122)
        [0, _, _, _] => true,
        // 100.64.0.0/10 — CGNAT (RFC 6598)
        [100, b, _, _] if (64..=127).contains(&b) => true,
        // 192.0.0.0/24 — IETF protocol assignments (RFC 6890)
        [192, 0, 0, _] => true,
        // 198.18.0.0/15 — benchmarking (RFC 2544)
        [198, 18 | 19, _, _] => true,
        // 240.0.0.0/4 — reserved for future use (RFC 1112)
        [a, _, _, _] if a >= 240 => true,
        _ => false,
    }
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    // IPv4-mapped and well-known NAT64 addresses reach the IPv4 they carry.
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_blocked_v4(mapped);
    }
    let seg = ip.segments();
    if let Some(embedded) = extract_nat64(&seg) {
        return is_blocked_v4(embedded);
    }
    // 2000::/3 is the only space allocated for global unicast. Outside it sit
    // loopback, unspecified, ULA, link-local, site-local, multicast, discard,
    // local-use NAT64 and the IPv4-compatible and -translated forms.
    if seg[0] & 0xe000 != 0x2000 {
        return true;
    }
    match seg {
        // 2001:2::/48 benchmarking (RFC 5180)
        [0x2001, 0x0002, 0, ..] => true,
        // 2001:db8::/32 documentation (RFC 3849)
        [0x2001, 0x0db8, ..] => true,
        // 3fff::/20 documentation (RFC 9637)
        [0x3fff, b, ..] if b < 0x1000 => true,
        // IPv6 transition mechanisms can embed IPv4 inside the v6 address. A
        // public-looking v6 address may carry a private v4 inside it that
        // would otherwise reach internal infrastructure through a relay.
        _ => {
            extract_6to4(&seg).is_some_and(is_blocked_v4)
                || extract_teredo(&seg).is_some_and(|ends| ends.into_iter().any(is_blocked_v4))
        }
    }
}

/// 6to4 (RFC 3056) encodes an IPv4 address into the second and third 16-bit
/// segments of a `2002::/16` address.
fn extract_6to4(seg: &[u16; 8]) -> Option<Ipv4Addr> {
    (seg[0] == 0x2002).then(|| v4_from(seg[1], seg[2]))
}

/// NAT64 well-known prefix `64:ff9b::/96` (RFC 6052) carries the embedded
/// IPv4 in the final two 16-bit segments.
fn extract_nat64(seg: &[u16; 8]) -> Option<Ipv4Addr> {
    (seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6].iter().all(|s| *s == 0))
        .then(|| v4_from(seg[6], seg[7]))
}

/// Teredo (RFC 4380) carries the server's IPv4 in segments 2-3 and the
/// client's, bit-inverted, in segments 6-7 of a `2001::/32` address. A relay
/// sends to both: indirect bubbles go via the server (RFC 4380 §5.2.4).
fn extract_teredo(seg: &[u16; 8]) -> Option<[Ipv4Addr; 2]> {
    (seg[0] == 0x2001 && seg[1] == 0).then(|| [v4_from(seg[2], seg[3]), v4_from(!seg[6], !seg[7])])
}

fn v4_from(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn blocked(ip: &str) -> bool {
        is_blocked_ip(ip.parse().unwrap())
    }

    #[test]
    fn blocks_ipv4_private_and_loopback() {
        assert!(blocked("127.0.0.1"));
        assert!(blocked("127.255.255.255"));
        assert!(blocked("10.0.0.1"));
        assert!(blocked("10.255.255.255"));
        assert!(blocked("172.16.0.1"));
        assert!(blocked("172.31.255.255"));
        assert!(blocked("192.168.0.1"));
        assert!(blocked("192.168.255.255"));
    }

    #[test]
    fn blocks_ipv4_link_local_and_metadata() {
        assert!(blocked("169.254.0.1"));
        assert!(blocked("169.254.169.254")); // AWS / GCP metadata
        assert!(blocked("169.254.255.255"));
    }

    #[test]
    fn blocks_ipv4_unspecified_and_broadcast_and_multicast() {
        assert!(blocked("0.0.0.0"));
        assert!(blocked("255.255.255.255"));
        assert!(blocked("224.0.0.1"));
        assert!(blocked("239.255.255.255"));
    }

    #[test]
    fn blocks_ipv4_this_network() {
        assert!(blocked("0.1.2.3"));
        assert!(blocked("0.255.255.255"));
        assert!(blocked("::ffff:0.1.2.3"));
    }

    #[test]
    fn blocks_ipv4_reserved_ranges() {
        assert!(blocked("100.64.0.1")); // CGNAT
        assert!(blocked("100.127.255.255"));
        assert!(blocked("192.0.0.1")); // IETF
        assert!(blocked("192.0.2.1")); // TEST-NET-1
        assert!(blocked("198.18.0.1")); // benchmarking
        assert!(blocked("198.19.255.255"));
        assert!(blocked("198.51.100.1")); // TEST-NET-2
        assert!(blocked("203.0.113.1")); // TEST-NET-3
        assert!(blocked("240.0.0.1")); // reserved
    }

    #[test]
    fn allows_public_ipv4() {
        assert!(!blocked("1.1.1.1"));
        assert!(!blocked("8.8.8.8"));
        assert!(!blocked("93.184.216.34"));
        assert!(!blocked("100.63.255.255"));
        assert!(!blocked("100.128.0.1"));
    }

    #[test]
    fn blocks_ipv6_loopback_link_local_ula_unspecified() {
        assert!(blocked("::1"));
        assert!(blocked("::"));
        assert!(blocked("fe80::1"));
        assert!(blocked("febf:ffff::1"));
        assert!(blocked("fc00::1"));
        assert!(blocked("fdff:ffff::1"));
        assert!(blocked("ff02::1"));
    }

    #[test]
    fn blocks_ipv6_site_local() {
        assert!(blocked("fec0::1"));
        assert!(blocked("feff:ffff::1"));
    }

    #[test]
    fn blocks_ipv6_mapped_private_v4() {
        assert!(blocked("::ffff:127.0.0.1"));
        assert!(blocked("::ffff:169.254.169.254"));
        assert!(blocked("::ffff:10.0.0.1"));
    }

    #[test]
    fn blocks_ipv6_documentation_and_discard() {
        assert!(blocked("2001:db8::1"));
        assert!(blocked("3fff::1"));
        assert!(blocked("3fff:fff:ffff::1"));
        assert!(blocked("100::1"));
    }

    #[test]
    fn blocks_ipv6_benchmarking() {
        assert!(blocked("2001:2::1"));
        assert!(blocked("2001:2:0:ffff::1"));
    }

    #[test]
    fn blocks_ipv6_outside_global_unicast() {
        assert!(blocked("4000::1"));
        assert!(blocked("5f00::1")); // SRv6 SIDs
        assert!(blocked("e000::1"));
        // ::a00:1 → deprecated IPv4-compatible 10.0.0.1
        assert!(blocked("::a00:1"));
        // ::ffff:0:a9fe:a9fe → IPv4-translated 169.254.169.254
        assert!(blocked("::ffff:0:a9fe:a9fe"));
        // 64:ff9b::/32 outside the /96 well-known layout
        assert!(blocked("64:ff9b:0:0:a00:1::"));
    }

    #[test]
    fn blocks_6to4_carrying_private_ipv4() {
        // 2002:0a00:0001:: → embedded 10.0.0.1
        assert!(blocked("2002:0a00:0001::"));
        // 2002:7f00:0001:: → embedded 127.0.0.1
        assert!(blocked("2002:7f00:0001::"));
        // 2002:a9fe:a9fe:: → embedded 169.254.169.254 (AWS metadata)
        assert!(blocked("2002:a9fe:a9fe::"));
    }

    #[test]
    fn allows_6to4_carrying_public_ipv4() {
        // 2002:0101:0101:: → embedded 1.1.1.1
        assert!(!blocked("2002:0101:0101::"));
    }

    #[test]
    fn blocks_nat64_carrying_private_ipv4() {
        // 64:ff9b::a00:1 → embedded 10.0.0.1
        assert!(blocked("64:ff9b::a00:1"));
        // 64:ff9b::7f00:1 → embedded 127.0.0.1
        assert!(blocked("64:ff9b::7f00:1"));
        // 64:ff9b::a9fe:a9fe → embedded 169.254.169.254
        assert!(blocked("64:ff9b::a9fe:a9fe"));
    }

    #[test]
    fn allows_nat64_carrying_public_ipv4() {
        // 64:ff9b::0101:0101 → embedded 1.1.1.1
        assert!(!blocked("64:ff9b::0101:0101"));
    }

    #[test]
    fn blocks_local_use_nat64_whatever_it_carries() {
        // 64:ff9b:1::a00:1 → embedded 10.0.0.1 under a /96 layout
        assert!(blocked("64:ff9b:1::a00:1"));
        // 64:ff9b:1::101:101 → embedded 1.1.1.1 under a /96 layout
        assert!(blocked("64:ff9b:1::101:101"));
        assert!(blocked("64:ff9b:1:ffff:ffff:ffff:ffff:ffff"));
    }

    #[test]
    fn blocks_teredo_carrying_private_ipv4() {
        // server 65.54.227.120, client !80ff:fffe → 127.0.0.1
        assert!(blocked("2001:0:4136:e378:8000:63bf:80ff:fffe"));
        // server 65.54.227.120, client !5601:5601 → 169.254.169.254
        assert!(blocked("2001:0:4136:e378:8000:63bf:5601:5601"));
        // server 10.0.0.1, client !fefe:fefe → 1.1.1.1
        assert!(blocked("2001:0:a00:1:8000:63bf:fefe:fefe"));
    }

    #[test]
    fn allows_teredo_carrying_public_ipv4() {
        // server 65.54.227.120, client !fefe:fefe → 1.1.1.1
        assert!(!blocked("2001:0:4136:e378:8000:63bf:fefe:fefe"));
    }

    #[test]
    fn allows_public_ipv6() {
        assert!(!blocked("2606:4700:4700::1111"));
        assert!(!blocked("2001:4860:4860::8888"));
        assert!(!blocked("3fff:1000::1"));
        // AS112-v6, globally reachable inside the IETF 2001::/23 block
        assert!(!blocked("2001:4:112::1"));
    }

    #[test]
    fn guard_with_allow_private_passes_everything() {
        let g = SsrfGuard::new(true);
        assert!(g.check(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))).is_ok());
        assert!(g.check(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_ok());
    }

    #[test]
    fn guard_filter_drops_blocked_keeps_public() {
        let g = SsrfGuard::new(false);
        let addrs = vec![
            "127.0.0.1".parse().unwrap(),
            "1.1.1.1".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
        ];
        let kept = g.filter("h", addrs).unwrap();
        assert_eq!(kept, vec!["1.1.1.1".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn guard_filter_errors_when_all_blocked() {
        let g = SsrfGuard::new(false);
        let addrs = vec!["127.0.0.1".parse().unwrap(), "10.0.0.1".parse().unwrap()];
        let err = g.filter("internal", addrs).unwrap_err();
        match err {
            SsrfError::AllBlocked { host } => assert_eq!(host, "internal"),
            _ => panic!("wrong variant"),
        }
    }
}
