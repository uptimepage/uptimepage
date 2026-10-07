use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::net::TcpStream;

use super::HttpClients;
use super::connector::{dns_reason, tcp_reason};
use crate::net::happy_eyeballs;

/// Resolves `host` and keeps only addresses the shared SSRF guard allows.
/// Errors when resolution fails or nothing survives the filter, so callers
/// always receive at least one address.
pub(crate) async fn allowed_addrs(
    host: &str,
    clients: &HttpClients,
) -> anyhow::Result<Vec<IpAddr>> {
    let guard = clients.ssrf_guard();
    // The resolver's own Display names the query it failed, brace-printed
    // struct and all. No error class can name that, so it would reach the
    // customer verbatim and land in `ErrorClass::Other`.
    let resolved = clients.resolver().resolve_addrs(host).await.map_err(|e| {
        let reason = dns_reason(&e);
        e.context(reason)
    })?;
    let addrs: Vec<IpAddr> = resolved.into_iter().filter(|ip| guard.allow(*ip)).collect();
    if addrs.is_empty() {
        anyhow::bail!("no allowed addresses for {host}");
    }
    Ok(addrs)
}

/// Races a TCP connection to the allowed addresses of `host`, v6 and v4
/// interleaved, giving up after `budget`.
pub(crate) async fn connect_via_guard(
    host: &str,
    port: u16,
    clients: &HttpClients,
    budget: Duration,
) -> anyhow::Result<TcpStream> {
    let addrs = allowed_addrs(host, clients)
        .await?
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    happy_eyeballs::connect(addrs, budget).await.map_err(|err| {
        // The raw `io::Error` Display carries a platform-specific errno, which no
        // error class can name and the customer should never read. Kept as the
        // source so an operator can still tell a refused port from a blocked one:
        // `context` is what `to_string` yields, the errno survives in `{:?}`.
        tracing::debug!(host, port, error = %err, "connect failed");
        let reason = tcp_reason(&err);
        anyhow::Error::new(err).context(reason)
    })
}
