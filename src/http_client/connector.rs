use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use metrics::Histogram;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::http_client::client::ChainFault;
use crate::http_client::dns::HickoryDnsResolver;
use crate::http_client::{H1_MAX_HEADERS, H2_MAX_HEADER_LIST_SIZE};
use crate::security::SsrfGuard;

pub(crate) type ReqBody = Full<Bytes>;

/// Elapsed since `start` as whole ms, saturating at `u16::MAX` — the width the
/// result schema stores each phase in.
fn ms16(start: Instant) -> u16 {
    start.elapsed().as_millis().min(u16::MAX as u128) as u16
}

/// Everything a single fresh-connect probe needs. Borrowed from `HttpClients`
/// per check; the `tls` connector is the verify/insecure one already chosen.
pub(crate) struct ConnectParams<'a> {
    pub resolver: &'a HickoryDnsResolver,
    pub ssrf_guard: SsrfGuard,
    pub tls: &'a TlsConnector,
    pub connect_ms: &'a Histogram,
    pub tls_ms: &'a Histogram,
    pub connect_timeout: Duration,
    pub tcp_keepalive: Option<Duration>,
}

/// Per-phase wall times (ms) for one connection establishment. `tls_ms` is
/// `None` for plain HTTP. Feeds the breakdown chart's DNS/Connect/TLS bands;
/// saturates at `u16::MAX` (~65 s, far past any phase that didn't time out).
pub(crate) struct PhaseTimings {
    pub dns_ms: u16,
    pub connect_ms: u16,
    pub tls_ms: Option<u16>,
}

pub(crate) struct TimedConnection {
    pub stream: MaybeTls,
    pub timings: PhaseTimings,
    pub alpn_h2: bool,
}

/// Connect-phase failures, typed so the probe attributes each to the right
/// breakdown band / reason string instead of sniffing error text. Each variant
/// carries the phase times that *did* complete before the failure, so a TLS
/// failure can still report its DNS + TCP timings.
#[derive(Debug)]
pub(crate) enum ConnectError {
    Dns(anyhow::Error),
    NoAddrs {
        dns_ms: u16,
    },
    Connect {
        err: io::Error,
        dns_ms: u16,
    },
    Tls {
        err: io::Error,
        dns_ms: u16,
        connect_ms: u16,
    },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Dns(e) => write!(f, "dns resolution failed: {e}"),
            ConnectError::NoAddrs { .. } => write!(f, "no allowed addresses for host"),
            ConnectError::Connect { err, .. } => write!(f, "tcp connect failed: {err}"),
            ConnectError::Tls { err, .. } => write!(f, "tls handshake failed: {err}"),
        }
    }
}

impl std::error::Error for ConnectError {}

impl ConnectError {
    /// Customer-facing reason naming *why* the connect phase failed, drilled
    /// out of the typed inner error rather than the flat phase name. DNS/TCP/TLS
    /// each surface their distinct failure modes (NXDOMAIN vs no-records,
    /// refused vs unreachable, expired vs untrusted vs hostname-mismatch).
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            ConnectError::Dns(e) => dns_reason(e),
            ConnectError::NoAddrs { .. } => "address not allowed",
            ConnectError::Connect { err, .. } => tcp_reason(err),
            ConnectError::Tls { err, .. } => tls_reason(err),
        }
    }

    /// `(dns_ms, connect_ms)` for the phases that completed before the failure.
    /// DNS failures have neither; a TLS failure has both.
    pub(crate) fn partial_timings(&self) -> (Option<u16>, Option<u16>) {
        match self {
            ConnectError::Dns(_) => (None, None),
            ConnectError::NoAddrs { dns_ms } | ConnectError::Connect { dns_ms, .. } => {
                (Some(*dns_ms), None)
            }
            ConnectError::Tls {
                dns_ms, connect_ms, ..
            } => (Some(*dns_ms), Some(*connect_ms)),
        }
    }
}

/// Shared with the TCP and TLS-cert kinds through `worker::allowed_addrs`,
/// which would otherwise hand the resolver's own Display to the customer.
pub(crate) fn dns_reason(e: &anyhow::Error) -> &'static str {
    let Some(n) = e.downcast_ref::<hickory_resolver::net::NetError>() else {
        return "dns: lookup failed";
    };
    if n.is_nx_domain() {
        "dns: domain not found"
    } else if n.is_no_records_found() {
        "dns: no address records"
    } else if matches!(n, hickory_resolver::net::NetError::Timeout) {
        "dns: lookup timed out"
    } else {
        "dns: lookup failed"
    }
}

/// Shared with the TCP and TLS-cert kinds through `worker::connect_via_guard`.
/// Ping builds its own message and is not normalised through here.
pub(crate) fn tcp_reason(io: &io::Error) -> &'static str {
    match io.kind() {
        io::ErrorKind::ConnectionRefused => "connection refused",
        io::ErrorKind::HostUnreachable => "host unreachable",
        io::ErrorKind::NetworkUnreachable => "network unreachable",
        io::ErrorKind::ConnectionReset => "connection reset",
        io::ErrorKind::TimedOut => "connect timeout",
        _ => "connect",
    }
}

/// The cert arms below are unreachable on the tls_cert monitor path, which
/// installs `NoVerify` and so never raises a certificate error. Its handshake
/// failures are protocol ones instead, which is why the non-certificate arms
/// carry the weight there. Both paths share this function, and the HTTP path
/// verifies for real, so both halves earn their place.
///
/// Every string here has to classify in `domain::check_error`, or it lands in
/// `ErrorClass::Other` and recreates the errno-shaped problem this replaced.
pub(crate) fn tls_reason(io: &io::Error) -> &'static str {
    use rustls::{CertificateError, Error};
    let Some(err) = io.get_ref().and_then(|e| e.downcast_ref::<Error>()) else {
        // Not a rustls error, so the transport died mid-handshake. Connect
        // already succeeded, which is why this cannot borrow `tcp_reason`:
        // its fallback says "connect".
        return match io.kind() {
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
                "tls handshake reset"
            }
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe => {
                "tls handshake closed early"
            }
            // No arm for TimedOut: the check's own `timeout` future wins first
            // and reports "timeout", so it would never be reached.
            _ => "tls",
        };
    };
    match err {
        Error::InvalidCertificate(cert) => match cert {
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                "certificate expired"
            }
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                "certificate not yet valid"
            }
            CertificateError::Revoked => "certificate revoked",
            CertificateError::UnknownIssuer => "certificate not trusted",
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                "certificate hostname mismatch"
            }
            // `ChainFault` is ours; rustls raises `Other` for any webpki error
            // it does not map by hand, which is what `None` catches.
            CertificateError::Other(other) => match other.0.downcast_ref::<ChainFault>() {
                Some(ChainFault::Incomplete) => "certificate chain incomplete",
                Some(ChainFault::SelfSigned) => "certificate self-signed",
                None => "certificate invalid",
            },
            _ => "certificate invalid",
        },
        // The server answered the hello with a refusal.
        Error::AlertReceived(_) => "tls handshake rejected",
        // No TLS version or cipher suite in common, which is what an old
        // server looks like from here.
        Error::PeerIncompatible(_) => "tls version or cipher mismatch",
        // The bytes are not TLS records at all, most often a plaintext port.
        Error::InvalidMessage(_) => "malformed tls response",
        // Same condition tls_cert reports when the chain is empty, so it
        // reuses that wording rather than opening a second name for it.
        Error::NoCertificatesPresented => "server returned no certificate chain",
        _ => "tls",
    }
}

/// Resolve → TCP connect → (TLS handshake), timing each phase. No pooling: the
/// caller drives exactly one request over the returned stream, then drops it.
pub(crate) async fn timed_connect(
    p: &ConnectParams<'_>,
    host: &str,
    port: u16,
    is_https: bool,
) -> Result<TimedConnection, ConnectError> {
    let dns_start = Instant::now();
    let addrs: Vec<SocketAddr> = p
        .resolver
        .resolve_addrs(host)
        .await
        .map_err(ConnectError::Dns)?
        .into_iter()
        .filter(|ip| p.ssrf_guard.allow(*ip))
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    let dns_ms = ms16(dns_start);
    if addrs.is_empty() {
        return Err(ConnectError::NoAddrs { dns_ms });
    }

    // Happy-Eyeballs v2 (RFC 8305): race v6/v4 with a stagger, bounded by
    // `connect_timeout`, so a broken AAAA doesn't burn the whole budget.
    let connect_start = Instant::now();
    let stream = crate::net::happy_eyeballs::connect(addrs, p.connect_timeout)
        .await
        .map_err(|err| ConnectError::Connect { err, dns_ms })?;
    let connect_ms = ms16(connect_start);
    p.connect_ms.record(connect_ms as f64);
    let _ = stream.set_nodelay(true);
    if let Some(d) = p.tcp_keepalive {
        let sock = socket2::SockRef::from(&stream);
        let ka = socket2::TcpKeepalive::new().with_time(d).with_interval(d);
        let _ = sock.set_tcp_keepalive(&ka);
    }

    if !is_https {
        return Ok(TimedConnection {
            stream: MaybeTls::Plain(stream),
            timings: PhaseTimings {
                dns_ms,
                connect_ms,
                tls_ms: None,
            },
            alpn_h2: false,
        });
    }

    let server_name = ServerName::try_from(host.to_owned()).map_err(|e| ConnectError::Tls {
        err: io::Error::other(format!("invalid server name: {e}")),
        dns_ms,
        connect_ms,
    })?;
    let tls_start = Instant::now();
    let tls_stream = p
        .tls
        .connect(server_name, stream)
        .await
        .map_err(|err| ConnectError::Tls {
            err,
            dns_ms,
            connect_ms,
        })?;
    let tls_ms = ms16(tls_start);
    p.tls_ms.record(tls_ms as f64);
    let alpn_h2 = {
        let (_io, conn) = tls_stream.get_ref();
        conn.alpn_protocol() == Some(b"h2")
    };

    Ok(TimedConnection {
        stream: MaybeTls::Tls(Box::new(tls_stream)),
        timings: PhaseTimings {
            dns_ms,
            connect_ms,
            tls_ms: Some(tls_ms),
        },
        alpn_h2,
    })
}

/// Drive one HTTP request over an established stream. h1/h2 chosen by the
/// negotiated ALPN. The connection task is spawned and aborted by [`ConnGuard`]
/// once the response body is drained — fresh-connect means it serves one
/// request and is torn down.
pub(crate) async fn handshake(
    stream: MaybeTls,
    alpn_h2: bool,
) -> hyper::Result<(Sender, ConnGuard)> {
    let io = TokioIo::new(stream);
    if alpn_h2 {
        let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .max_header_list_size(H2_MAX_HEADER_LIST_SIZE)
            .handshake(io)
            .await?;
        let guard = ConnGuard(tokio::spawn(async move {
            let _ = conn.await;
        }));
        Ok((Sender::H2(sender), guard))
    } else {
        let (sender, conn) = hyper::client::conn::http1::Builder::new()
            .max_headers(H1_MAX_HEADERS)
            .handshake(io)
            .await?;
        let guard = ConnGuard(tokio::spawn(async move {
            let _ = conn.await;
        }));
        Ok((Sender::H1(sender), guard))
    }
}

pub(crate) enum Sender {
    H1(hyper::client::conn::http1::SendRequest<ReqBody>),
    H2(hyper::client::conn::http2::SendRequest<ReqBody>),
}

impl Sender {
    pub(crate) async fn send_request(
        &mut self,
        req: Request<ReqBody>,
    ) -> hyper::Result<Response<Incoming>> {
        match self {
            Sender::H1(s) => s.send_request(req).await,
            Sender::H2(s) => s.send_request(req).await,
        }
    }
}

/// Aborts the spawned connection task on drop. Must outlive response-body
/// collection — the body streams from this task.
pub(crate) struct ConnGuard(tokio::task::JoinHandle<()>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// Box<TlsStream<_>> and TcpStream are both Unpin, so Pin::new(&mut ...) works
// directly on either arm.
pub(crate) enum MaybeTls {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

macro_rules! delegate_io {
    ($self:ident.$method:ident($($arg:expr),*)) => {
        match &mut $self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).$method($($arg),*),
            MaybeTls::Tls(s) => Pin::new(s).$method($($arg),*),
        }
    };
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate_io!(self.poll_read(cx, buf))
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate_io!(self.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate_io!(self.poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate_io!(self.poll_shutdown(cx))
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            MaybeTls::Plain(s) => s.is_write_vectored(),
            MaybeTls::Tls(s) => s.is_write_vectored(),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        delegate_io!(self.poll_write_vectored(cx, bufs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(kind: io::ErrorKind) -> ConnectError {
        ConnectError::Connect {
            err: io::Error::new(kind, "x"),
            dns_ms: 0,
        }
    }

    fn tls_cert(cert: rustls::CertificateError) -> ConnectError {
        ConnectError::Tls {
            err: io::Error::other(rustls::Error::InvalidCertificate(cert)),
            dns_ms: 0,
            connect_ms: 0,
        }
    }

    #[test]
    fn tcp_reasons_split_by_io_kind() {
        assert_eq!(
            tcp(io::ErrorKind::ConnectionRefused).reason(),
            "connection refused"
        );
        assert_eq!(
            tcp(io::ErrorKind::HostUnreachable).reason(),
            "host unreachable"
        );
        assert_eq!(
            tcp(io::ErrorKind::NetworkUnreachable).reason(),
            "network unreachable"
        );
        assert_eq!(
            tcp(io::ErrorKind::ConnectionReset).reason(),
            "connection reset"
        );
        assert_eq!(tcp(io::ErrorKind::TimedOut).reason(), "connect timeout");
        assert_eq!(tcp(io::ErrorKind::BrokenPipe).reason(), "connect");
    }

    #[test]
    fn tls_reasons_split_by_certificate_error() {
        use rustls::CertificateError as C;
        assert_eq!(tls_cert(C::Expired).reason(), "certificate expired");
        assert_eq!(
            tls_cert(C::NotValidYet).reason(),
            "certificate not yet valid"
        );
        assert_eq!(tls_cert(C::Revoked).reason(), "certificate revoked");
        assert_eq!(
            tls_cert(C::UnknownIssuer).reason(),
            "certificate not trusted"
        );
        assert_eq!(
            tls_cert(C::NotValidForName).reason(),
            "certificate hostname mismatch"
        );
        assert_eq!(tls_cert(C::BadEncoding).reason(), "certificate invalid");
    }

    /// webpki calls all three `UnknownIssuer`.
    #[test]
    fn chain_faults_split_untrusted_into_actionable_reasons() {
        use std::sync::Arc;

        use rustls::{CertificateError as C, OtherError};

        assert_eq!(
            tls_cert(C::Other(OtherError(Arc::new(ChainFault::Incomplete)))).reason(),
            "certificate chain incomplete"
        );
        assert_eq!(
            tls_cert(C::Other(OtherError(Arc::new(ChainFault::SelfSigned)))).reason(),
            "certificate self-signed"
        );
        // An `Other` raised anywhere else keeps the flat name.
        assert_eq!(
            tls_cert(C::Other(OtherError(Arc::new(io::Error::other("x"))))).reason(),
            "certificate invalid"
        );
    }

    fn tls_err(err: rustls::Error) -> ConnectError {
        ConnectError::Tls {
            err: io::Error::other(err),
            dns_ms: 0,
            connect_ms: 0,
        }
    }

    fn tls_io(kind: io::ErrorKind) -> ConnectError {
        ConnectError::Tls {
            err: io::Error::new(kind, "x"),
            dns_ms: 0,
            connect_ms: 0,
        }
    }

    /// The tls_cert monitor installs `NoVerify`, so every certificate arm is
    /// dead on that path and these are the failures it actually produces.
    /// Before they were named, all of them reached the customer as "tls".
    #[test]
    fn tls_protocol_errors_are_named_not_lumped_together() {
        use rustls::Error as E;
        assert_eq!(
            tls_err(E::AlertReceived(rustls::AlertDescription::HandshakeFailure)).reason(),
            "tls handshake rejected"
        );
        assert_eq!(
            tls_err(E::PeerIncompatible(
                rustls::PeerIncompatible::NoCipherSuitesInCommon
            ))
            .reason(),
            "tls version or cipher mismatch"
        );
        assert_eq!(
            tls_err(E::InvalidMessage(rustls::InvalidMessage::InvalidCcs)).reason(),
            "malformed tls response"
        );
        // Reuses the wording tls_cert already emits for an empty chain.
        assert_eq!(
            tls_err(E::NoCertificatesPresented).reason(),
            "server returned no certificate chain"
        );
        // A rustls error with no arm of its own still has to stay generic.
        assert_eq!(tls_err(E::DecryptError).reason(), "tls");
    }

    /// A transport death mid-handshake is not a rustls error at all. Connect
    /// has already succeeded here, so `tcp_reason` would be wrong: its
    /// fallback says "connect".
    #[test]
    fn tls_transport_failures_name_the_handshake_phase() {
        assert_eq!(
            tls_io(io::ErrorKind::ConnectionReset).reason(),
            "tls handshake reset"
        );
        assert_eq!(
            tls_io(io::ErrorKind::ConnectionAborted).reason(),
            "tls handshake reset"
        );
        assert_eq!(
            tls_io(io::ErrorKind::UnexpectedEof).reason(),
            "tls handshake closed early"
        );
        assert_eq!(
            tls_io(io::ErrorKind::BrokenPipe).reason(),
            "tls handshake closed early"
        );
        assert_eq!(tls_io(io::ErrorKind::Other).reason(), "tls");
        // Deliberately generic: the check's own timeout future reports
        // "timeout" long before an io TimedOut could surface here, so naming
        // it would be a dead arm.
        assert_eq!(tls_io(io::ErrorKind::TimedOut).reason(), "tls");
    }

    /// An unclassified reason lands in `ErrorClass::Other`, which is what the
    /// errno leak did before it was normalised. Sampling from the emitter
    /// rather than from the match arm is the point: a renamed string fails
    /// here instead of silently falling through in production.
    #[test]
    fn every_tls_reason_this_emits_is_classified() {
        use crate::domain::check_error::{ErrorClass, classify_check_error, humanize_check_error};
        use rustls::Error as E;

        let emitted = [
            tls_err(E::InvalidCertificate(rustls::CertificateError::Expired)).reason(),
            tls_err(E::AlertReceived(rustls::AlertDescription::HandshakeFailure)).reason(),
            tls_err(E::PeerIncompatible(
                rustls::PeerIncompatible::NoCipherSuitesInCommon,
            ))
            .reason(),
            tls_err(E::InvalidMessage(rustls::InvalidMessage::InvalidCcs)).reason(),
            tls_err(E::NoCertificatesPresented).reason(),
            tls_err(E::DecryptError).reason(),
            tls_cert(rustls::CertificateError::Other(rustls::OtherError(
                std::sync::Arc::new(ChainFault::Incomplete),
            )))
            .reason(),
            tls_cert(rustls::CertificateError::Other(rustls::OtherError(
                std::sync::Arc::new(ChainFault::SelfSigned),
            )))
            .reason(),
            tls_io(io::ErrorKind::ConnectionReset).reason(),
            tls_io(io::ErrorKind::UnexpectedEof).reason(),
            tls_io(io::ErrorKind::Other).reason(),
        ];
        for reason in emitted {
            assert_ne!(
                classify_check_error(reason),
                ErrorClass::Other,
                "{reason:?} is emitted but unclassified"
            );
            // Several reasons are already plain English and pass through
            // unchanged, which is fine. What is not fine is shipping the
            // protocol spelled in lowercase to a reader.
            let shown = humanize_check_error(reason);
            assert!(
                !shown.contains("tls"),
                "{reason:?} reaches the reader as {shown:?}, with tls uncapitalised"
            );
        }
    }

    #[test]
    fn no_addrs_is_address_not_allowed() {
        assert_eq!(
            ConnectError::NoAddrs { dns_ms: 0 }.reason(),
            "address not allowed"
        );
    }

    #[test]
    fn partial_timings_carry_completed_phases() {
        // DNS failure: nothing completed.
        assert_eq!(
            ConnectError::Dns(anyhow::anyhow!("x")).partial_timings(),
            (None, None)
        );
        // TCP failure: DNS completed, connect did not.
        let tcp = ConnectError::Connect {
            err: io::Error::other("x"),
            dns_ms: 12,
        };
        assert_eq!(tcp.partial_timings(), (Some(12), None));
        // TLS failure: DNS + TCP completed.
        let tls = ConnectError::Tls {
            err: io::Error::other(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Expired,
            )),
            dns_ms: 12,
            connect_ms: 45,
        };
        assert_eq!(tls.partial_timings(), (Some(12), Some(45)));
    }

    #[test]
    fn dns_timeout_and_unknown_split() {
        let t = ConnectError::Dns(anyhow::Error::new(hickory_resolver::net::NetError::Timeout));
        assert_eq!(t.reason(), "dns: lookup timed out");
        let other = ConnectError::Dns(anyhow::anyhow!("plain message"));
        assert_eq!(other.reason(), "dns: lookup failed");
    }
}
