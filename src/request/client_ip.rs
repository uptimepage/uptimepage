//! Honest client-IP extraction across a reverse proxy.
//!
//! `ConnectInfo<SocketAddr>` is the TCP peer — fine for direct binds, but
//! behind Caddy / nginx / a CDN it collapses to a single proxy address.
//! Every `ip_hash` written to `login_attempts`, `quota_events`, magic-link
//! audit and friends then keys on that one value, so the
//! `idx_login_attempts_failures_by_ip` and `idx_quota_events_abuse_by_ip`
//! views can't distinguish a botnet from a single bad CI runner.
//!
//! Fix: trust the TCP peer to identify the proxy, then walk the
//! `X-Forwarded-For` header right-to-left, skipping any hop whose address
//! is itself in `trusted_proxies`. The first untrusted hop is the client.
//! If no header is present (or no parseable address survives), fall back
//! to the TCP peer — never trust an unverified header.
//!
//! Spoofing model: if `trusted_proxies` is empty (default), this function
//! returns `peer` unchanged. The header is never honoured for an untrusted
//! origin, so a directly-connected client cannot forge their own IP.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use ipnet::IpNet;

use crate::app::AppState;

const XFF: &str = "x-forwarded-for";
const X_REAL_IP: &str = "x-real-ip";

/// Resolve the public client IP given the TCP peer, the request headers,
/// and the configured trusted-proxy CIDRs. See module docs for the
/// algorithm. Returns `peer` whenever the trust list is empty or the
/// peer is not in it.
pub fn extract(headers: &HeaderMap, peer: IpAddr, trusted: &[IpNet]) -> IpAddr {
    if !is_trusted(peer, trusted) {
        return peer;
    }
    if let Some(ip) = client_from_xff(headers, trusted) {
        return ip;
    }
    if let Some(ip) = headers
        .get(X_REAL_IP)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
    {
        return ip;
    }
    peer
}

/// Axum extractor wrapping [`extract`]. Reads `ConnectInfo<SocketAddr>` and
/// the request headers from `Parts`, the trusted-proxy CIDRs from
/// `AppState`, and yields the resolved client IP. Removes the per-handler
/// ceremony of threading three arguments through every login/auth surface
/// — handlers just take `ClientIp(ip): ClientIp` and the spoofing model is
/// enforced by the extractor, not by call-site discipline.
pub struct ClientIp(pub IpAddr);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        // ConnectInfo is required by every handler that needs the peer
        // address; if it's missing the deployment is misconfigured (no
        // `into_make_service_with_connect_info`). Fall back to an
        // unspecified address rather than 500ing the request — the
        // resulting `ip_hash` becomes "no signal" rather than a forged one.
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip())
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        let ip = extract(
            &parts.headers,
            peer,
            &app_state.cfg.security.trusted_proxies,
        );
        Ok(ClientIp(ip))
    }
}

/// The client IP when it is worth showing a reader: behind a declared proxy an
/// internal result means the chain lost the client, with none declared it is the client.
pub fn reportable(ip: IpAddr, trusted: &[IpNet]) -> Option<IpAddr> {
    let ip = ip.to_canonical();
    if trusted.is_empty() {
        return (!ip.is_unspecified()).then_some(ip);
    }
    (!crate::security::ssrf::is_blocked_ip(ip)).then_some(ip)
}

fn is_trusted(ip: IpAddr, trusted: &[IpNet]) -> bool {
    // A dual-stack listener reports v4 peers in `::ffff:` form, which no v4 CIDR contains.
    let ip = ip.to_canonical();
    trusted.iter().any(|n| n.contains(&ip))
}

fn client_from_xff(headers: &HeaderMap, trusted: &[IpNet]) -> Option<IpAddr> {
    let raw = headers.get(XFF)?.to_str().ok()?;
    let hops: Vec<IpAddr> = raw
        .split(',')
        .filter_map(|h| h.trim().parse::<IpAddr>().ok())
        .map(|ip| ip.to_canonical())
        .collect();
    // Walk right-to-left, skipping trusted proxies. First untrusted is
    // the originating client. If every hop is trusted (full chain of
    // proxies, no client recorded) — keep the leftmost as best-effort.
    for ip in hops.iter().rev() {
        if !is_trusted(*ip, trusted) {
            return Some(*ip);
        }
    }
    hops.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> IpNet {
        s.parse().unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    fn headers_xff(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(XFF, v.parse().unwrap());
        h
    }

    #[test]
    fn empty_trust_list_returns_peer_unchanged() {
        let h = headers_xff("1.2.3.4");
        assert_eq!(extract(&h, ip("203.0.113.7"), &[]), ip("203.0.113.7"));
    }

    #[test]
    fn untrusted_peer_ignores_xff() {
        let h = headers_xff("1.2.3.4");
        let trusted = [cidr("172.17.0.0/16")];
        assert_eq!(extract(&h, ip("203.0.113.7"), &trusted), ip("203.0.113.7"));
    }

    #[test]
    fn trusted_peer_with_single_xff_returns_client() {
        let h = headers_xff("1.2.3.4");
        let trusted = [cidr("172.17.0.0/16")];
        assert_eq!(extract(&h, ip("172.17.0.5"), &trusted), ip("1.2.3.4"));
    }

    #[test]
    fn trusted_peer_walks_xff_right_to_left_skipping_trusted_hops() {
        // CDN forwarded "client, cdn-edge"; Caddy appended its own address
        // when forwarding. Effective chain: client, cdn-edge, caddy.
        let h = headers_xff("1.2.3.4, 198.51.100.7, 172.17.0.5");
        let trusted = [cidr("172.17.0.0/16"), cidr("198.51.100.0/24")];
        assert_eq!(extract(&h, ip("172.17.0.5"), &trusted), ip("1.2.3.4"));
    }

    #[test]
    fn falls_back_to_real_ip_when_xff_absent() {
        let mut h = HeaderMap::new();
        h.insert(X_REAL_IP, "1.2.3.4".parse().unwrap());
        let trusted = [cidr("172.17.0.0/16")];
        assert_eq!(extract(&h, ip("172.17.0.5"), &trusted), ip("1.2.3.4"));
    }

    #[test]
    fn falls_back_to_peer_when_no_proxy_header() {
        let h = HeaderMap::new();
        let trusted = [cidr("172.17.0.0/16")];
        assert_eq!(extract(&h, ip("172.17.0.5"), &trusted), ip("172.17.0.5"));
    }

    #[test]
    fn all_hops_trusted_falls_back_to_leftmost() {
        // No real client recorded — every hop is a proxy. Best-effort: take
        // the leftmost so the data point isn't lost, even though it isn't a
        // true client IP. Mirrors the conventional "client, proxy1, proxy2"
        // shape where the leftmost is what the original proxy claims.
        let h = headers_xff("172.17.0.10, 172.17.0.5");
        let trusted = [cidr("172.17.0.0/16")];
        assert_eq!(extract(&h, ip("172.17.0.5"), &trusted), ip("172.17.0.10"));
    }

    #[test]
    fn reportable_keeps_public_addresses() {
        let trusted = [cidr("172.16.0.0/12")];
        assert_eq!(
            reportable(ip("115.186.231.19"), &trusted),
            Some(ip("115.186.231.19"))
        );
        assert_eq!(
            reportable(ip("2a01:4f9::1"), &trusted),
            Some(ip("2a01:4f9::1"))
        );
    }

    #[test]
    fn reportable_drops_internal_addresses_behind_a_proxy() {
        let trusted = [cidr("172.16.0.0/12")];
        assert_eq!(reportable(ip("172.18.0.1"), &trusted), None);
        assert_eq!(reportable(ip("::ffff:172.18.0.1"), &trusted), None);
        assert_eq!(reportable(ip("10.0.0.4"), &trusted), None);
        assert_eq!(reportable(ip("169.254.1.1"), &trusted), None);
        assert_eq!(reportable(ip("127.0.0.1"), &trusted), None);
        assert_eq!(reportable(ip("fd4f:420f:fd92::1"), &trusted), None);
        assert_eq!(reportable(ip("fe80::1"), &trusted), None);
        assert_eq!(reportable(ip("::"), &trusted), None);
    }

    #[test]
    fn reportable_keeps_a_lan_client_when_no_proxy_is_declared() {
        assert_eq!(
            reportable(ip("192.168.1.50"), &[]),
            Some(ip("192.168.1.50"))
        );
        assert_eq!(reportable(ip("0.0.0.0"), &[]), None);
    }

    #[test]
    fn v4_mapped_peer_is_trusted_against_a_v4_cidr() {
        let h = headers_xff("1.2.3.4");
        let trusted = [cidr("172.17.0.0/16")];
        assert_eq!(
            extract(&h, ip("::ffff:172.17.0.5"), &trusted),
            ip("1.2.3.4")
        );
    }

    #[test]
    fn ipv6_loopback_trust_works() {
        let h = headers_xff("2001:db8::1");
        let trusted = [cidr("::1/128"), cidr("fe80::/10")];
        let peer: IpAddr = "::1".parse().unwrap();
        assert_eq!(extract(&h, peer, &trusted), ip("2001:db8::1"));
    }
}
