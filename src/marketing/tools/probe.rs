//! Shared plumbing for the marketing tools that open an outbound socket.
//!
//! Everything here exists to bound what a stranger can make this server do:
//! a hostname parser that refuses anything but a public DNS name, an SSRF
//! filter over the resolved addresses, a per-IP budget, and an error taxonomy
//! that keeps a stranger's dead host out of our own 5xx rate. A tool adds its
//! own port allowlist and deadline on top.

use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::time::Duration;

use axum::Extension;
use axum::Json;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use hickory_resolver::net::NetError;
use hickory_resolver::{Resolver, TokioResolver};
use serde::Serialize;

use crate::security::SsrfGuard;

use super::super::config::MarketingCfg;

/// Above this many tracked addresses, drop the ones whose budget has fully
/// refilled. Bounded growth without a background janitor.
const LIMITER_HIGH_WATER: usize = 10_000;

const DNS_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) type ProbeLimiter = RateLimiter<IpAddr, DashMapStateStore<IpAddr>, DefaultClock>;

/// Per-IP budget for one tool. Caddy already caps public pages per IP at the
/// edge; this is the app-side floor for deployments that do not front the site
/// with it. Each tool sizes its own, because a probe that opens one socket and
/// a probe that walks a redirect chain do not cost the same.
pub(super) fn limiter(per_min: u32, burst: u32) -> ProbeLimiter {
    let quota = Quota::per_minute(NonZeroU32::new(per_min).expect("nonzero"))
        .allow_burst(NonZeroU32::new(burst).expect("nonzero"));
    RateLimiter::dashmap(quota)
}

/// Our own resolver rather than `tokio::net::lookup_host`, which is
/// `getaddrinfo` on the blocking pool and cannot be cancelled: a nameserver
/// that simply never answers would hold a pool thread long past the deadline,
/// and the pool is shared with the rest of the process.
static RESOLVER: std::sync::LazyLock<Option<TokioResolver>> = std::sync::LazyLock::new(|| {
    let mut builder = Resolver::builder_tokio().ok()?;
    let opts = builder.options_mut();
    opts.timeout = DNS_TIMEOUT;
    opts.attempts = 1;
    builder.build().ok()
});

/// Collapses an IPv6 address to its /64. Any VPS is handed a routed /64 or
/// wider, so keying on the full address would hand one visitor an unbounded
/// number of independent budgets against the only routes here that open a
/// socket.
fn budget_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// Charges `cost` probes to the visitor's address, all or nothing. Missing
/// `ConnectInfo` means the server was built without it, which would silently
/// hand everyone the same budget; an unspecified address keeps them metered
/// together rather than not at all.
pub(super) fn spend_budget(
    cfg: &MarketingCfg,
    headers: &HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    limiter: &ProbeLimiter,
    cost: u32,
) -> bool {
    let peer = peer.map_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |c| {
        (c.0).0.ip()
    });
    let client = crate::request::client_ip::extract(headers, peer, &cfg.trusted_proxies);
    let cost = NonZeroU32::new(cost).expect("a probe costs at least one");
    let allowed = limiter
        .check_key_n(&budget_key(client), cost)
        .is_ok_and(|r| r.is_ok());
    if limiter.len() > LIMITER_HIGH_WATER {
        limiter.retain_recent();
    }
    allowed
}

/// Resolves, then keeps only addresses the SSRF guard allows. A name that
/// resolves entirely into private space is reported as unreachable rather than
/// as blocked: the distinction is only useful to someone probing us.
pub(super) async fn resolve(host: &str) -> Result<Vec<IpAddr>, ProbeError> {
    let Some(resolver) = RESOLVER.as_ref() else {
        return Err(ProbeError::Server(
            StatusCode::SERVICE_UNAVAILABLE,
            "The checker has no resolver configured.",
        ));
    };
    let resolved = match tokio::time::timeout(DNS_TIMEOUT, resolver.lookup_ip(host)).await {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(e)) if no_nameserver_was_reached(&e) => {
            return Err(ProbeError::Server(
                StatusCode::SERVICE_UNAVAILABLE,
                "The checker could not reach a nameserver.",
            ));
        }
        _ => return Err(ProbeError::Host("That name does not resolve.")),
    };
    let guard = SsrfGuard::strict();
    let addrs: Vec<IpAddr> = resolved.iter().filter(|ip| guard.allow(*ip)).collect();
    if addrs.is_empty() {
        return Err(ProbeError::Host(
            "That name does not resolve to a reachable public address.",
        ));
    }
    Ok(addrs)
}

/// A DNS answer we never got because nothing on the resolver path answered at
/// all. A name that times out or comes back empty is the visitor's host.
fn no_nameserver_was_reached(e: &NetError) -> bool {
    matches!(
        e,
        NetError::Io(_) | NetError::NoConnections | NetError::Busy
    )
}

/// A connect that failed before a packet could leave this host. Reporting it as
/// the host's fault would tell every visitor their site is dead the moment we
/// lose egress, and record nothing here.
pub(super) fn egress_is_broken(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::AddrNotAvailable
    )
}

/// A real answer and a host that would not respond both come back 200, so the
/// page needs a field to tell them apart.
#[derive(Serialize)]
pub(super) struct Answer<T> {
    pub(super) ok: bool,
    #[serde(flatten)]
    pub(super) body: T,
}

pub(super) enum ProbeError {
    /// A 5xx here would file a stranger's dead host as our own broken response
    /// and spend the service error budget on it.
    Host(&'static str),
    Server(StatusCode, &'static str),
}

impl IntoResponse for ProbeError {
    fn into_response(self) -> Response {
        match self {
            Self::Host(error) => fail(StatusCode::OK, error),
            Self::Server(status, error) => fail(status, error),
        }
    }
}

pub(super) fn fail(status: StatusCode, error: &'static str) -> Response {
    #[derive(Serialize)]
    struct Failure<'a> {
        error: &'a str,
    }
    (
        status,
        probe_headers(),
        Json(Answer {
            ok: false,
            body: Failure { error },
        }),
    )
        .into_response()
}

/// Never cached and never indexed: the answer is per-host and changes the
/// moment the host does.
pub(super) fn probe_headers() -> [(header::HeaderName, HeaderValue); 2] {
    [
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, private"),
        ),
        (
            header::HeaderName::from_static("x-robots-tag"),
            HeaderValue::from_static("noindex"),
        ),
    ]
}

/// Accepts what a person pastes — a full URL, a trailing dot, mixed case — and
/// yields a bare DNS name, or nothing. Address literals are refused: they carry
/// no SNI, and allowing them would turn the resolver guard into the only thing
/// standing between these endpoints and the internal network.
pub(super) fn clean_host(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some((_, rest)) = s.split_once("://") {
        s = rest;
    }
    s = s
        .split(['/', '?', '#'])
        .next()?
        .rsplit('@')
        .next()?
        .trim_end_matches('.');
    if s.is_empty() || s.contains(':') {
        return None;
    }

    // An internationalized name is a real name with a real certificate, and
    // the /start flow already accepts one through `Url::parse`. Fold it to
    // punycode here so both surfaces agree on what a domain is; the label
    // rules below then run on the ASCII form either way.
    let host = if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        idna::domain_to_ascii(s).ok()?
    };
    if host.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    let well_formed = labels.iter().all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    // An all-digit final label is an IPv4 literal, never a registrable name.
    let is_v4_literal = labels
        .last()
        .is_some_and(|l| l.chars().all(|c| c.is_ascii_digit()));
    (well_formed && !is_v4_literal).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_what_a_person_pastes() {
        assert_eq!(
            clean_host("https://Acme.com/pricing?x=1").as_deref(),
            Some("acme.com")
        );
        assert_eq!(clean_host("  acme.com.  ").as_deref(), Some("acme.com"));
        assert_eq!(
            clean_host("mail.acme.co.uk").as_deref(),
            Some("mail.acme.co.uk")
        );
    }

    #[test]
    fn refuses_literals_and_malformed_names() {
        for raw in [
            "127.0.0.1",
            "192.168.1.10",
            "[::1]",
            "localhost",
            "acme.com:8443",
            "-acme.com",
            "acme..com",
            "",
        ] {
            assert!(clean_host(raw).is_none(), "{raw} should be refused");
        }
    }

    /// Same answer the /start flow gives, which folds through `Url::parse`.
    #[test]
    fn folds_an_internationalized_name_to_punycode() {
        assert_eq!(
            clean_host("münchen.de").as_deref(),
            Some("xn--mnchen-3ya.de")
        );
    }

    #[tokio::test]
    async fn a_host_verdict_answers_200_with_ok_false() {
        let res = ProbeError::Host("That name does not resolve.").into_response();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], false);
        assert!(json["error"].is_string(), "the page renders this sentence");
    }

    #[tokio::test]
    async fn a_fault_on_this_side_keeps_its_5xx() {
        let res =
            ProbeError::Server(StatusCode::SERVICE_UNAVAILABLE, "no resolver").into_response();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Losing egress must not read as every visitor's host being dead.
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

    #[test]
    fn an_answer_is_marked_ok_beside_its_fields() {
        let json = serde_json::to_value(Answer {
            ok: true,
            body: serde_json::json!({ "host": "acme.com" }),
        })
        .unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["host"], "acme.com");
    }

    /// One visitor must not open an unbounded number of budgets by walking a
    /// routed /64.
    #[test]
    fn an_ipv6_visitor_is_metered_by_prefix() {
        let a: IpAddr = "2001:db8:1:2:3:4:5:6".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap();
        assert_eq!(budget_key(a), budget_key(b));
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_ne!(budget_key(a), budget_key(other));
    }
}
