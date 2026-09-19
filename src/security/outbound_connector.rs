//! HTTP connector for non-check outbound traffic (notification webhooks,
//! RDAP) that resolves DNS first, drops every IP the [`SsrfGuard`] blocks,
//! and only then opens the TCP connection. Mirrors the SSRF defence the
//! check-path connector applies, without the per-probe DNS/TLS/TTFB
//! histograms and pool-tracking — those would poison the check metrics
//! when emitted from non-check paths.
//!
//! Wrap with `hyper_rustls::HttpsConnectorBuilder::wrap_connector` to add
//! TLS; the guard fires before TCP open, so a webhook URL pointing at
//! `127.0.0.1`, `10.0.0.0/8`, link-local, ULA, cloud-metadata, or 6to4 /
//! NAT64 with embedded private v4 is rejected regardless of scheme.
//!
//! DNS-rebinding safe: the filter runs on the *resolved* addresses, so a
//! hostname that initially returned a public IP can't be swapped to a
//! private IP between channel-create validation and notifier dispatch —
//! the rebinder's reply still has to clear the guard at connect time.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tower::Service;

use crate::net::happy_eyeballs;
use crate::security::{SsrfError, SsrfGuard};

/// Overall budget for the connect race. Bounds the happy-eyeballs sweep
/// across every resolved address — without it, a webhook pointing at a
/// non-routable public IP wedges per-attempt for the OS default (~75 s on
/// Linux, minutes on macOS) before the hyper client gives up and retries.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct SsrfHttpConnector {
    guard: SsrfGuard,
}

impl SsrfHttpConnector {
    pub fn new(guard: SsrfGuard) -> Self {
        Self { guard }
    }
}

/// Connected `TcpStream` wrapped so it implements [`Connection`] — required
/// by `hyper_rustls::HttpsConnectorBuilder::wrap_connector` and by
/// `hyper_util::client::legacy::Client` when this connector is used directly
/// for plain HTTP. Pure delegation; no extra state.
#[derive(Debug)]
pub struct SsrfStream {
    inner: TcpStream,
}

impl Connection for SsrfStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl AsyncRead for SsrfStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for SsrfStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Service<Uri> for SsrfHttpConnector {
    type Response = TokioIo<SsrfStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let guard = self.guard;
        Box::pin(async move {
            // `Uri::host()` returns IPv6 literals with their surrounding
            // brackets (e.g. `[fc00::1]`); `lookup_host` expects the bracket-
            // less form. Share `domain::check::unbracket` with the abuse / SSRF
            // hostname checks so all three normalise identically.
            let host = crate::domain::check::unbracket(
                dst.host()
                    .ok_or_else(|| io::Error::other("missing host in URI"))?,
            )
            .to_string();
            let scheme = dst.scheme_str().unwrap_or("http");
            let port = dst
                .port_u16()
                .unwrap_or(if scheme == "https" { 443 } else { 80 });

            // tokio's libc-backed lookup_host honours /etc/hosts, IDNA, and
            // AAAA records. We feed the resolved IPs through the shared
            // `SsrfGuard::filter` so the connector's policy outcome stays in
            // lock-step with the check-path's filter on the same set of
            // blocked ranges (loopback / RFC1918 / link-local / ULA /
            // metadata / 6to4 / NAT64 with embedded private v4).
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
                .await?
                .collect();
            let ips: Vec<std::net::IpAddr> = resolved.iter().map(|sa| sa.ip()).collect();
            let allowed = guard.filter(&host, ips).map_err(|e| match e {
                SsrfError::AllBlocked { .. } => io::Error::other(format!(
                    "refusing to connect to '{host}': every resolved address is in a \
                     blocked range (loopback / private / link-local / metadata). \
                     Set security.allow_private_targets=true to permit private \
                     destinations in development."
                )),
                SsrfError::BlockedAddress(ip) => {
                    io::Error::other(format!("address {ip} is in a blocked range"))
                }
            })?;
            let addrs: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|sa| allowed.contains(&sa.ip()))
                .collect();

            // Happy-Eyeballs v2 race (RFC 8305): try v6 and v4 in parallel
            // with a 250 ms stagger so a single broken AAAA on a long-tail
            // webhook target doesn't cost the full `CONNECT_TIMEOUT`.
            let stream = happy_eyeballs::connect(addrs, CONNECT_TIMEOUT).await?;
            Ok(TokioIo::new(SsrfStream { inner: stream }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_loopback_when_guard_strict() {
        let mut svc = SsrfHttpConnector::new(SsrfGuard::strict());
        let dst: Uri = "http://127.0.0.1:9".parse().unwrap();
        let err = svc.call(dst).await.expect_err("loopback must be blocked");
        assert!(
            err.to_string().contains("blocked"),
            "expected SSRF block, got: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_link_local_when_guard_strict() {
        let mut svc = SsrfHttpConnector::new(SsrfGuard::strict());
        // 169.254.169.254 — AWS / Hetzner cloud metadata. Connect must never
        // be attempted regardless of whether anything answers there.
        let dst: Uri = "http://169.254.169.254/".parse().unwrap();
        let err = svc
            .call(dst)
            .await
            .expect_err("metadata IP must be blocked");
        assert!(err.to_string().contains("blocked"));
    }

    #[tokio::test]
    async fn rejects_ipv6_ula_when_guard_strict() {
        let mut svc = SsrfHttpConnector::new(SsrfGuard::strict());
        // fc00::/7 — unique-local, RFC 4193.
        let dst: Uri = "http://[fc00::1]/".parse().unwrap();
        let err = svc.call(dst).await.expect_err("ULA must be blocked");
        assert!(err.to_string().contains("blocked"));
    }

    #[tokio::test]
    async fn rejects_missing_host_uri() {
        let mut svc = SsrfHttpConnector::new(SsrfGuard::strict());
        // `/foo` is a path-only URI — no host component.
        let dst: Uri = "/foo".parse().unwrap();
        let err = svc.call(dst).await.expect_err("host-less URI must error");
        assert!(err.to_string().contains("missing host"));
    }

    #[tokio::test]
    async fn allows_loopback_when_guard_relaxed() {
        // Bind an ephemeral listener; the connector must reach it through
        // the same loopback IP the strict guard rejects.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut svc = SsrfHttpConnector::new(SsrfGuard::relaxed_for_tests());
        let dst: Uri = format!("http://127.0.0.1:{port}/").parse().unwrap();
        let connect = svc.call(dst);
        let accept = listener.accept();
        let (conn, accepted) = tokio::join!(connect, accept);
        conn.expect("connect must succeed when guard is relaxed");
        accepted.expect("listener must accept the connect");
    }
}
