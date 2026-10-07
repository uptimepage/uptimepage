//! Happy Eyeballs v2 (RFC 8305) TCP connect.
//!
//! Given a pre-filtered list of socket addresses (the caller has already
//! resolved DNS and applied any policy filtering), race the IPv6 and IPv4
//! attempts with a `STAGGER` delay between starts. First connection that
//! succeeds wins; the rest are dropped and their futures cancelled.
//!
//! Without this, a single broken AAAA record on a long-tail webhook target
//! costs `CONNECT_TIMEOUT` (10 s) per attempt before the v4 fallback runs —
//! enough to make notifications visibly late and per-check monitoring miss
//! intervals. Top-tier providers (Slack, Discord, Telegram, etc.) have clean
//! dual-stack so this is invisible there; the cost matters for self-hosted /
//! small-SaaS endpoints we'll integrate with later.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::net::TcpStream;

use super::egress_is_broken;

/// RFC 8305 §4 "Connection Attempt Delay". 250 ms is the spec's default and
/// matches what mainstream stacks (Chrome, Firefox, macOS, curl) ship.
pub const STAGGER: Duration = Duration::from_millis(250);

/// Race the connects for `addrs` with v6/v4 interleaved, returning the first
/// successful `TcpStream`. Bounds the *overall* operation at `overall_timeout`
/// (not per-address) so a long list of addresses can't drag the whole connect
/// past the budget the caller granted.
///
/// `addrs` must already be DNS-resolved and policy-filtered — this function
/// only runs the connect race. If `addrs` is empty, returns an error.
pub async fn connect(addrs: Vec<SocketAddr>, overall_timeout: Duration) -> io::Result<TcpStream> {
    if addrs.is_empty() {
        return Err(io::Error::other("happy-eyeballs: no addresses to connect"));
    }
    let interleaved = interleave_v6_first(addrs);

    let deadline = tokio::time::Instant::now() + overall_timeout;
    let mut attempts: FuturesUnordered<_> = FuturesUnordered::new();
    let mut iter = interleaved.into_iter().peekable();
    let mut kept_err: Option<io::Error> = None;

    // Seed with the first address immediately.
    if let Some(addr) = iter.next() {
        attempts.push(TcpStream::connect(addr));
    }

    loop {
        if attempts.is_empty() && iter.peek().is_none() {
            return Err(
                kept_err.unwrap_or_else(|| io::Error::other("happy-eyeballs: connect failed"))
            );
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(deadline_error(kept_err));
        }

        // Cap the stagger by the overall deadline so we don't sleep past it.
        let stagger = tokio::time::sleep_until((now + STAGGER).min(deadline));

        tokio::select! {
            // Existing attempt finished. Success wins immediately; failure is
            // ranked against the one kept so far. On failure
            // we fall through to the loop head, which will queue the next
            // address (if any) and immediately await — no extra stagger
            // penalising a fast-failing address.
            Some(res) = attempts.next() => {
                match res {
                    Ok(stream) => return Ok(stream),
                    Err(e) => kept_err = Some(more_telling(kept_err, e)),
                }
                if let Some(addr) = iter.next() {
                    attempts.push(TcpStream::connect(addr));
                }
            }
            // No success yet within the stagger window: start the next
            // address in parallel. Bounded by overall_timeout because
            // `stagger_until` was clamped to `deadline`.
            _ = stagger => {
                if let Some(addr) = iter.next() {
                    attempts.push(TcpStream::connect(addr));
                }
            }
        }
    }
}

/// Keeps the first failure unless only a later one speaks for the target.
fn more_telling(kept: Option<io::Error>, next: io::Error) -> io::Error {
    match kept {
        Some(kept) if !egress_is_broken(&kept) || egress_is_broken(&next) => kept,
        _ => next,
    }
}

/// An address still pending at the deadline timed out, which says more about
/// the target than a failure that never left this host.
fn deadline_error(kept: Option<io::Error>) -> io::Error {
    kept.filter(|e| !egress_is_broken(e)).unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::TimedOut, "happy-eyeballs: overall timeout")
    })
}

/// Interleave addresses so IPv6 is tried first, then v4, alternating
/// thereafter (RFC 8305 §4). Preserves the resolver's intra-family ordering.
/// DNS typically returns ≤4 addresses, so the per-call partition cost is
/// dominated by the TCP RTT of the first attempt.
fn interleave_v6_first(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let (v6, v4): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|sa| sa.is_ipv6());
    let mut out = Vec::with_capacity(v6.len() + v4.len());
    let mut v6 = v6.into_iter();
    let mut v4 = v4.into_iter();
    loop {
        match (v6.next(), v4.next()) {
            (Some(a), Some(b)) => {
                out.push(a);
                out.push(b);
            }
            (Some(a), None) => out.push(a),
            (None, Some(b)) => out.push(b),
            (None, None) => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn interleaves_v6_then_v4_alternating() {
        let input = vec![
            sock("1.1.1.1:80"),
            sock("[2606:4700::1]:80"),
            sock("8.8.8.8:80"),
            sock("[2001:4860::1]:80"),
        ];
        let out = interleave_v6_first(input);
        // v6 first, alternating with v4.
        assert!(out[0].is_ipv6(), "first must be v6: {:?}", out);
        assert!(out[1].is_ipv4());
        assert!(out[2].is_ipv6());
        assert!(out[3].is_ipv4());
    }

    #[test]
    fn interleave_handles_v4_only() {
        let input = vec![sock("1.1.1.1:80"), sock("8.8.8.8:80")];
        let out = interleave_v6_first(input);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|sa| sa.is_ipv4()));
    }

    #[test]
    fn interleave_handles_v6_only() {
        let input = vec![sock("[2606:4700::1]:80"), sock("[2001:4860::1]:80")];
        let out = interleave_v6_first(input);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|sa| sa.is_ipv6()));
    }

    #[test]
    fn interleave_v6_longer_than_v4_keeps_trailing_v6() {
        let input = vec![
            sock("1.1.1.1:80"),
            sock("[2606:4700::1]:80"),
            sock("[2001:4860::1]:80"),
            sock("[2001:db8::1]:80"),
        ];
        let out = interleave_v6_first(input);
        assert!(out[0].is_ipv6());
        assert!(out[1].is_ipv4());
        assert!(out[2].is_ipv6());
        assert!(out[3].is_ipv6());
    }

    #[test]
    fn a_missing_probe_route_never_masks_the_targets_answer() {
        use io::ErrorKind::{ConnectionRefused, NetworkUnreachable, TimedOut};
        let kind = |kept: Option<io::ErrorKind>, next| {
            more_telling(kept.map(io::Error::from), io::Error::from(next)).kind()
        };
        assert_eq!(kind(None, NetworkUnreachable), NetworkUnreachable);
        assert_eq!(
            kind(Some(NetworkUnreachable), ConnectionRefused),
            ConnectionRefused
        );
        assert_eq!(kind(Some(ConnectionRefused), TimedOut), ConnectionRefused);
        assert_eq!(
            kind(Some(NetworkUnreachable), NetworkUnreachable),
            NetworkUnreachable
        );

        let at_deadline =
            |kept: Option<io::ErrorKind>| deadline_error(kept.map(io::Error::from)).kind();
        assert_eq!(at_deadline(None), TimedOut);
        assert_eq!(at_deadline(Some(NetworkUnreachable)), TimedOut);
        assert_eq!(at_deadline(Some(ConnectionRefused)), ConnectionRefused);
    }

    #[tokio::test]
    async fn errors_on_empty_address_list() {
        let err = connect(vec![], Duration::from_secs(1))
            .await
            .expect_err("empty list must error");
        assert!(err.to_string().contains("no addresses"));
    }

    #[tokio::test]
    async fn picks_listening_v4_when_only_v4_listens() {
        // Bind a v4 listener; v6 address points at a closed port on loopback.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let v4_port = listener.local_addr().unwrap().port();
        // ::1:1 is almost certainly closed; the connect fails immediately on
        // most systems, so v4 wins by being a working listener.
        let addrs = vec![sock("[::1]:1"), sock(&format!("127.0.0.1:{v4_port}"))];
        let connect_fut = connect(addrs, Duration::from_secs(2));
        let accept_fut = listener.accept();
        let (conn, accepted) = tokio::join!(connect_fut, accept_fut);
        conn.expect("v4 must succeed");
        accepted.expect("listener must accept");
    }

    #[tokio::test]
    async fn a_silent_first_address_does_not_hold_up_the_next() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // TEST-NET-1 drops the SYN, the way a firewalled AAAA does.
        let addrs = vec![sock("192.0.2.1:80"), sock(&format!("127.0.0.1:{port}"))];
        let start = tokio::time::Instant::now();
        let (conn, accepted) =
            tokio::join!(connect(addrs, Duration::from_secs(5)), listener.accept());
        conn.expect("the reachable address must win");
        accepted.expect("listener must accept");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn returns_an_error_when_all_attempts_fail() {
        let addrs = vec![sock("127.0.0.1:1"), sock("127.0.0.1:2")];
        let err = connect(addrs, Duration::from_secs(2))
            .await
            .expect_err("both refused must error");
        // The OS error is ConnectionRefused regardless of family; we mainly
        // assert that *some* error propagated rather than a swallow.
        assert!(
            !err.to_string().is_empty(),
            "all-fail must surface a diagnostic"
        );
    }

    #[tokio::test]
    async fn overall_timeout_fires_when_no_address_responds() {
        // 192.0.2.0/24 is TEST-NET-1 (RFC 5737) — reserved for documentation,
        // never routable on the public Internet. A connect attempt drops
        // packets into the void; with a tight overall timeout we must give up
        // rather than wait the OS default.
        let addrs = vec![sock("192.0.2.1:80")];
        let start = tokio::time::Instant::now();
        let err = connect(addrs, Duration::from_millis(300))
            .await
            .expect_err("must error on overall timeout");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "overall_timeout must bound the wait; took {elapsed:?}: {err}"
        );
    }
}
