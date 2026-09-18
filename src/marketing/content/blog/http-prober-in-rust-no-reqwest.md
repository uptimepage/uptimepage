+++
title = "Why I dropped reqwest for hyper in my Rust uptime prober"
date = "2026-07-07"
updated = "2026-09-18"
slug = "http-prober-in-rust-no-reqwest"
excerpt = "I swapped reqwest for raw hyper to probe uptime in Rust. Four things it taught me, each one a detail a normal HTTP client hides from you."
tags = ["rust", "hyper", "monitoring", "devops"]
draft = false
+++

> **TL;DR.** An uptime prober is a strange HTTP client, so I swapped reqwest for raw hyper. It needs the opposite of what a normal client gives you: no connection pool, so the cold path gets measured instead of hidden; every phase timed, even when connect fails; a hard SSRF block on the URL the user typed; and exact failure reasons instead of one flat error. Four tricks below, each worth copying even if you never build a monitor.

I built an uptime checker in Rust. Like most people, I used [reqwest](https://github.com/seanmonstar/reqwest) first. Then I dropped it and moved down to raw [hyper](https://github.com/hyperium/hyper).

Not because reqwest is slow. It is great, and for app code you should use it. But a prober is a strange kind of HTTP client. It makes one request, and its whole job is to measure that request and report exactly what happened. A normal client is built to do the opposite: make requests fast and hide the messy details. Every trick below is a messy detail I needed to keep.

Here are four things building it taught me. Each one is a habit worth copying, even if you never write a monitor.

## 1. A connection pool lies to a monitor

The first thing reqwest gives you is a connection pool, and it is the first thing I had to remove.

A pool keeps TCP connections open and reuses them. The second request to a host skips DNS, skips the TCP handshake, skips TLS, and comes back much faster. For app code that is a free speed boost. For a monitor it is a lie. If I reuse a warm connection, the "connect time" I report is near zero, because I did not connect, I borrowed. A user watching a slow TLS handshake slowly rise in one region would see nothing, because the pool hid the handshake.

So the connector makes exactly one request per connection, then drops it:

```rust
/// No pooling: the caller drives exactly one request over the returned
/// stream, then drops it.
pub(crate) async fn timed_connect(
    p: &ConnectParams<'_>,
    host: &str,
    port: u16,
    is_https: bool,
) -> Result<TimedConnection, ConnectError> {
```

Every check pays the full cost of a fresh connection, because the full cost is the thing I am trying to measure. The lesson is general: if you are timing a request, a warm pool is measuring the wrong thing. Reuse is a feature that erases exactly the numbers a monitor exists to show.

## 2. Put the timings inside the error

Splitting a request into DNS, TCP, TLS, and time to first byte is easy on the happy path. The interesting question is what happens when connect fails.

The naive design loses everything on failure. You get an `Err` and no numbers, so a hung TLS handshake looks the same as a dead DNS server. Both are just "failed." That is the worst moment to have no data.

The fix is small, and I now use it everywhere: make the error carry the phases that finished before the one that broke.

```rust
pub(crate) enum ConnectError {
    Dns(anyhow::Error),
    NoAddrs { dns_ms: u16 },
    Connect { err: io::Error, dns_ms: u16 },
    Tls { err: io::Error, dns_ms: u16, connect_ms: u16 },
}
```

A TLS failure still reports how long DNS and TCP took. So when a cert handshake hangs, the chart still shows DNS was 12ms, connect was 45ms, and then TLS took the rest. The shape of the error tells you what broke. You are never left guessing which layer stalled, even on the path where everything went wrong. reqwest gives you one flat `Error` at the very end. Here the error type holds the answer.

```text
one probe, one fresh connection, each phase on its own clock:

  DNS         TCP connect      TLS handshake     request + TTFB
  +-----------+----------------+-----------------+---------------->
   12ms        45ms             (hung)            never reached

  TLS failed. The error still carries dns_ms=12, connect_ms=45,
  so you know exactly which layer stalled, not just "it failed".
```

## 3. The SSRF check goes after DNS, not before

This is the one most people building a fetch-any-URL feature get wrong, and it is the scary one.

A checker fetches whatever URL the user typed. That is the feature. It is also a classic [SSRF](https://owasp.org/www-community/attacks/Server_Side_Request_Forgery) engine. Someone signs up, points a check at `http://169.254.169.254/latest/meta-data/`, and now your prober is reading cloud credentials from inside your own network and showing them the reply. Point it at `http://10.0.0.5:6379` and it is a port scanner for your private subnet.

The trap is thinking you can block this by checking the hostname. You cannot. The classic bypass is DNS rebinding: the name looks public and passes your check, but it resolves to `127.0.0.1`. The block has to happen after resolution, on the real IP you are about to dial:

```rust
let addrs: Vec<SocketAddr> = p
    .resolver
    .resolve_addrs(host)
    .await
    .map_err(ConnectError::Dns)?
    .into_iter()
    .filter(|ip| p.ssrf_guard.allow(*ip))   // block loopback, private, link-local, metadata
    .map(|ip| SocketAddr::new(ip, port))
    .collect();
```

Resolve first, filter every resolved IP, then connect only to what survives. And because each redirect hop reconnects through this same guarded path, a `Location:` header pointing at `169.254.169.254` gets rejected exactly like a directly typed one. The safety sits in the single place all connections pass through, so there is nothing to forget on the redirect path. If you ever fetch a user-supplied URL server-side, this is the check that matters, and the hostname version is not it. The [HTTP header and redirect checker](/tools/http-header-checker) is this connector pointed at whatever URL you paste: every hop guarded the same way, and the chain printed rather than followed silently. The [website security checker](/tools/website-security-checker) adds no probe of its own: it calls this one and the certificate reader, then grades the answers in the browser.

## 4. Stop grepping your error strings

When a check goes red the user gets one line, and that line has to be right. "Something went wrong" trains people to ignore alerts. "Certificate expired" tells them what to fix in five seconds.

The tempting way to produce that line is to match on error message text. It is a trap: those strings change between library versions and read differently on every OS. The message is not an API. So instead the connector downcasts to the real error and reads the reason straight out of it:

```rust
fn tls_reason(io: &io::Error) -> &'static str {
    use rustls::CertificateError;
    let Some(rustls::Error::InvalidCertificate(cert)) =
        io.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>())
    else {
        return "tls";
    };
    match cert {
        CertificateError::Expired => "certificate expired",
        CertificateError::UnknownIssuer => "certificate not trusted",
        CertificateError::NotValidForName => "certificate hostname mismatch",
        CertificateError::Revoked => "certificate revoked",
        _ => "certificate invalid",
    }
}
```

An expired cert, an untrusted issuer, and a hostname mismatch are three different problems with three different fixes, and the user should never have to guess which one they have. The `io::ErrorKind` and the rustls error already carry the truth. The trick is to refuse to flatten it into a string and then re-parse the string you just made.

## So, reqwest or not?

For your program? Almost certainly reqwest. Going down to hyper means you own redirects, decompression with a size cap, timeout budgeting per hop, and the request-target rules for HTTP/1 versus HTTP/2. That is real code with real edge cases, and every one is a place reqwest would have been correct for free. If I needed to fetch a config file at startup, writing this connector would be a mistake.

It is worth it here for one reason: measuring and guarding the request is the product, not a detail of it. When the request itself is the thing you sell, you want to own every millisecond and every IP it touches.

Uptimepage is open source, AGPL-3.0, and the whole probe path is in the repo: [github.com/uptimepage/uptimepage](https://github.com/uptimepage/uptimepage). The rows this probe writes land in ClickHouse, and [why I store them there](/blog/postgres-vs-clickhouse-uptime-monitor) is its own post. If you want the wider build story, one binary and two databases, [that is a separate post](/blog/building-an-uptime-monitor-in-rust). Or just [start free on the hosted tier](/) and point a check at something.
