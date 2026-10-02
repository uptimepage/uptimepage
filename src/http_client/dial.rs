use std::net::{IpAddr, SocketAddr};

use tokio::net::TcpStream;

use super::HttpClients;
use super::connector::{dns_reason, tcp_reason};

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

/// Tries to open a TCP connection to `(ip, port)` for each allowed address of
/// `host`.
pub(crate) async fn connect_via_guard(
    host: &str,
    port: u16,
    clients: &HttpClients,
) -> anyhow::Result<TcpStream> {
    let mut last_err: Option<std::io::Error> = None;
    for ip in allowed_addrs(host, clients).await? {
        match TcpStream::connect(SocketAddr::new(ip, port)).await {
            Ok(s) => return Ok(s),
            Err(e) => last_err = Some(e),
        }
    }
    // The raw `io::Error` Display carries a platform-specific errno, which no
    // error class can name and the customer should never read. Kept as the
    // source so an operator can still tell a refused port from a blocked one:
    // `context` is what `to_string` yields, the errno survives in `{:?}`.
    let err = last_err.expect("allowed_addrs yields at least one address");
    tracing::debug!(host, port, error = %err, "connect failed");
    let reason = tcp_reason(&err);
    Err(anyhow::Error::new(err).context(reason))
}
