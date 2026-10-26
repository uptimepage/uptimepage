//! Forward proxy for one browser run. Lightpanda sends every request here, so
//! it never dials a target itself: DNS, the address policy and the mail-port
//! rule apply to redirects and script requests as they do to the start URL.
//! The engine's own IP filter cannot do this, since behind a proxy the only
//! address it ever sees is the proxy's.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::HeaderMap;
use hyper::body::{Bytes, Incoming};
use hyper::header::CONNECTION;
use hyper::http::uri::PathAndQuery;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use ipnet::IpNet;
use tokio::net::TcpListener;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::domain::check::MAIL_PORT_REFUSED;
use crate::security::{DialError, SsrfGuard, dial_guarded};

/// A failed accept (EMFILE under load) is retried after this, not fatal.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Addressed to this proxy or to the hop, never to the target.
const HOP_HEADERS: [&str; 8] = [
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
];

type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// The addresses a browser run may reach. An exemption beats both block
/// sources, as in Lightpanda's `--block-cidrs` form this config keeps.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    guard: SsrfGuard,
    block: Vec<IpNet>,
    exempt: Vec<IpNet>,
}

impl EgressPolicy {
    /// `cidrs` is comma-separated; a `-` prefix exempts a range.
    pub fn new(block_private: bool, cidrs: &str) -> Result<Self, String> {
        let mut policy = Self {
            guard: SsrfGuard::new(!block_private),
            block: Vec::new(),
            exempt: Vec::new(),
        };
        for item in cidrs.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            let (list, cidr) = match item.strip_prefix('-') {
                Some(cidr) => (&mut policy.exempt, cidr),
                None => (&mut policy.block, item),
            };
            list.push(
                cidr.parse()
                    .map_err(|_| format!("flow.block_cidrs: '{cidr}' is not a CIDR range"))?,
            );
        }
        Ok(policy)
    }

    fn allows(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.exempt.iter().any(|net| net.contains(&ip))
            || (self.guard.allow(ip) && !self.block.iter().any(|net| net.contains(&ip)))
    }
}

/// Lives as long as the run it serves; dropping it ends every connection and
/// tunnel it holds.
pub(super) struct EgressProxy {
    addr: SocketAddr,
    _stop: DropGuard,
}

impl EgressProxy {
    pub(super) async fn start(policy: Arc<EgressPolicy>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let stop = CancellationToken::new();
        tokio::spawn(
            stop.clone()
                .run_until_cancelled_owned(serve(listener, policy, stop.clone())),
        );
        Ok(Self {
            addr,
            _stop: stop.drop_guard(),
        })
    }

    pub(super) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

async fn serve(listener: TcpListener, policy: Arc<EgressPolicy>, stop: CancellationToken) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                tracing::debug!(error = %err, "flow egress proxy accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let (policy, conn_stop) = (policy.clone(), stop.clone());
        let service =
            hyper::service::service_fn(move |req| handle(req, policy.clone(), conn_stop.clone()));
        let conn = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades();
        tokio::spawn(stop.clone().run_until_cancelled_owned(conn));
    }
}

/// An error closes the browser's connection without a reply, which it reports
/// as a network failure rather than as a page the target served.
async fn handle(
    req: Request<Incoming>,
    policy: Arc<EgressPolicy>,
    stop: CancellationToken,
) -> io::Result<Response<ProxyBody>> {
    if req.method() == Method::CONNECT {
        Ok(tunnel(req, &policy, stop).await)
    } else {
        forward(req, &policy, stop).await
    }
}

/// `CONNECT host:port`, which carries every https request: the browser runs
/// TLS end to end through the bytes copied here. A refusal is a non-2xx
/// answer, which the browser already treats as a failed connect.
async fn tunnel(
    req: Request<Incoming>,
    policy: &EgressPolicy,
    stop: CancellationToken,
) -> Response<ProxyBody> {
    let Some((host, port)) = req
        .uri()
        .authority()
        .and_then(|a| Some((a.host().to_owned(), a.port_u16()?)))
    else {
        return reply(StatusCode::BAD_REQUEST, "CONNECT needs host:port");
    };
    let mut upstream = match dial_guarded(&host, port, |ip| policy.allows(ip)).await {
        Ok(stream) => stream,
        Err(err) => {
            let (status, reason) = refusal(&err);
            return reply(status, reason);
        }
    };
    tokio::spawn(stop.run_until_cancelled_owned(async move {
        if let Ok(upgraded) = hyper::upgrade::on(req).await {
            let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(upgraded), &mut upstream).await;
        }
    }));
    Response::new(text(""))
}

/// An absolute-form `http://` request, sent on in origin form over a fresh
/// connection. Anything else (`ws://` arrives this way) is refused.
async fn forward(
    mut req: Request<Incoming>,
    policy: &EgressPolicy,
    stop: CancellationToken,
) -> io::Result<Response<ProxyBody>> {
    let uri = req.uri().clone();
    if uri.scheme_str() != Some("http") {
        return Err(io::Error::other("only http:// requests are forwarded"));
    }
    let host = uri
        .host()
        .ok_or_else(|| io::Error::other("request has no host"))?;
    let upstream = dial_guarded(host, uri.port_u16().unwrap_or(80), |ip| policy.allows(ip))
        .await
        .map_err(|err| io::Error::other(refusal(&err).1))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(upstream))
        .await
        .map_err(io::Error::other)?;
    tokio::spawn(stop.run_until_cancelled_owned(conn));

    let path = uri
        .path_and_query()
        .cloned()
        .unwrap_or_else(|| PathAndQuery::from_static("/"));
    *req.uri_mut() = Uri::from(path);
    strip_hop_headers(req.headers_mut());
    let mut resp = sender.send_request(req).await.map_err(io::Error::other)?;
    strip_hop_headers(resp.headers_mut());
    Ok(resp.map(BodyExt::boxed))
}

/// RFC 9110 §7.6.1: the fixed hop headers, plus any the `Connection` header
/// names, describe one hop and are not forwarded.
fn strip_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    for name in HOP_HEADERS
        .iter()
        .copied()
        .chain(named.iter().map(String::as_str))
    {
        headers.remove(name);
    }
}

fn refusal(err: &DialError) -> (StatusCode, &'static str) {
    match err {
        DialError::MailPort => (StatusCode::FORBIDDEN, MAIL_PORT_REFUSED),
        DialError::Blocked => (StatusCode::FORBIDDEN, "address not allowed"),
        DialError::Resolve(_) => (StatusCode::BAD_GATEWAY, "dns resolution failed"),
        DialError::Connect(_) => (StatusCode::BAD_GATEWAY, "connect failed"),
    }
}

fn reply(status: StatusCode, reason: &'static str) -> Response<ProxyBody> {
    let mut resp = Response::new(text(reason));
    *resp.status_mut() = status;
    resp
}

fn text(body: &'static str) -> ProxyBody {
    Full::new(Bytes::from_static(body.as_bytes()))
        .map_err(|never| match never {})
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn an_exemption_beats_both_block_sources() {
        let policy =
            EgressPolicy::new(true, "203.0.113.0/24,-10.0.0.42/32,-203.0.113.7/32").unwrap();
        assert!(!policy.allows(ip("10.0.0.1")));
        assert!(policy.allows(ip("10.0.0.42")));
        assert!(!policy.allows(ip("203.0.113.1")));
        assert!(policy.allows(ip("203.0.113.7")));
        assert!(policy.allows(ip("93.184.216.34")));
    }

    #[test]
    fn a_mapped_v6_address_meets_the_v4_ranges() {
        let policy = EgressPolicy::new(false, "198.51.100.0/24").unwrap();
        assert!(!policy.allows(ip("::ffff:198.51.100.9")));
    }

    #[test]
    fn off_blocks_nothing_but_the_listed_ranges() {
        let policy = EgressPolicy::new(false, "").unwrap();
        assert!(policy.allows(ip("127.0.0.1")));
        assert!(policy.allows(ip("::1")));
    }

    #[test]
    fn a_bad_range_is_named() {
        let err = EgressPolicy::new(true, "10.0.0.0/8, nope").unwrap_err();
        assert!(err.contains("'nope'"), "{err}");
    }

    async fn proxy(block_private: bool) -> EgressProxy {
        EgressProxy::start(Arc::new(EgressPolicy::new(block_private, "").unwrap()))
            .await
            .unwrap()
    }

    /// One raw request through the proxy, the whole reply read back.
    async fn exchange(proxy: &EgressProxy, request: &str) -> String {
        let mut conn = TcpStream::connect(proxy.addr).await.unwrap();
        conn.write_all(request.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf))
            .await
            .expect("proxy answers")
            .unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    async fn listener() -> (u16, TcpListener) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        (l.local_addr().unwrap().port(), l)
    }

    #[tokio::test]
    async fn a_mail_port_is_refused_before_anything_is_dialled() {
        let p = proxy(false).await;
        for port in crate::domain::check::MAIL_PORTS {
            let tunnel = exchange(
                &p,
                &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
            )
            .await;
            assert!(tunnel.starts_with("HTTP/1.1 403"), "{port}: {tunnel}");
            assert!(tunnel.contains(MAIL_PORT_REFUSED), "{tunnel}");

            let post = exchange(
                &p,
                &format!(
                    "POST http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                     Content-Length: 6\r\n\r\nHELO x"
                ),
            )
            .await;
            assert_eq!(post, "", "{port}: a refused plain request gets no reply");
        }
    }

    #[tokio::test]
    async fn a_private_address_is_refused_when_the_policy_blocks_it() {
        let (port, _l) = listener().await;
        let p = proxy(true).await;
        let tunnel = exchange(
            &p,
            &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        )
        .await;
        assert!(tunnel.starts_with("HTTP/1.1 403"), "{tunnel}");
        assert!(tunnel.contains("address not allowed"), "{tunnel}");

        let plain = exchange(
            &p,
            &format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        )
        .await;
        assert_eq!(plain, "");
    }

    // A reply would be loaded as the page; a closed connection is the network
    // failure the browser saw before the proxy existed.
    #[tokio::test]
    async fn a_dead_plain_origin_closes_the_connection_instead_of_serving_a_page() {
        let (port, l) = listener().await;
        drop(l);
        let reply = exchange(
            &proxy(false).await,
            &format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        )
        .await;
        assert_eq!(reply, "");
    }

    #[tokio::test]
    async fn a_plain_request_goes_on_in_origin_form() {
        let (port, l) = listener().await;
        let seen = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 2048];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(
                b"HTTP/1.1 200 OK\r\nProxy-Authenticate: Basic\r\nContent-Length: 2\r\n\r\nok",
            )
            .await
            .unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        let reply = exchange(
            &proxy(false).await,
            &format!(
                "GET http://127.0.0.1:{port}/a?b=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
                 Proxy-Connection: keep-alive\r\nConnection: keep-alive, X-Hop\r\nX-Hop: 1\r\n\r\n"
            ),
        )
        .await;
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.ends_with("ok"), "{reply}");
        assert!(
            !reply.to_ascii_lowercase().contains("proxy-authenticate"),
            "{reply}"
        );
        let upstream = seen.await.unwrap();
        assert!(
            upstream.starts_with("GET /a?b=1 HTTP/1.1\r\n"),
            "{upstream}"
        );
        let upstream = upstream.to_ascii_lowercase();
        assert!(!upstream.contains("proxy-connection"), "{upstream}");
        assert!(!upstream.contains("x-hop"), "{upstream}");
    }

    #[tokio::test]
    async fn a_websocket_through_the_proxy_is_refused() {
        let reply = exchange(
            &proxy(false).await,
            "PUT ws://127.0.0.1:9/ws HTTP/1.1\r\nHost: 127.0.0.1:9\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        assert_eq!(reply, "");
    }

    #[tokio::test]
    async fn dropping_the_proxy_ends_its_open_tunnels() {
        let (port, l) = listener().await;
        let p = proxy(false).await;
        let mut conn = TcpStream::connect(p.addr).await.unwrap();
        conn.write_all(
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let (mut upstream, _) = l.accept().await.unwrap();
        let mut buf = vec![0u8; 256];
        let n = conn.read(&mut buf).await.unwrap();
        assert!(buf[..n].starts_with(b"HTTP/1.1 200"));

        conn.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        upstream.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");

        drop(p);
        let closed = tokio::time::timeout(Duration::from_secs(5), upstream.read(&mut buf))
            .await
            .expect("tunnel closes once the proxy is dropped");
        assert!(matches!(closed, Ok(0) | Err(_)));
    }
}
