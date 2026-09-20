use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts};
use hickory_resolver::lookup_ip::LookupIp;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::{Resolver, TokioResolver};
use metrics::{Histogram, histogram};

use crate::config::DnsConfig;
use crate::error::Result;
use crate::metric_names;

pub struct HickoryDnsResolver {
    inner: Arc<TokioResolver>,
    dns_ms: Histogram,
}

impl HickoryDnsResolver {
    pub fn new(cfg: &DnsConfig) -> Result<Self> {
        let mut opts = ResolverOpts::default();
        opts.cache_size = cfg.cache_size as u64;
        opts.positive_max_ttl = Some(Duration::from_secs(cfg.positive_ttl_secs));
        opts.negative_max_ttl = Some(Duration::from_secs(cfg.negative_ttl_secs));
        opts.attempts = 2;
        opts.timeout = Duration::from_secs(3);
        opts.try_tcp_on_error = true;

        let name_servers: Vec<NameServerConfig> = cfg
            .servers
            .iter()
            .map(|s| {
                let trimmed = s.split(':').next().unwrap_or(s);
                trimmed
                    .parse::<IpAddr>()
                    .map(NameServerConfig::udp)
                    .with_context(|| format!("invalid dns server ip: {s}"))
            })
            .collect::<anyhow::Result<_>>()?;

        let resolver_config = ResolverConfig::from_parts(None, vec![], name_servers);

        let resolver =
            Resolver::builder_with_config(resolver_config, TokioRuntimeProvider::default())
                .with_options(opts)
                .build()
                .context("failed to build hickory resolver")?;

        Ok(Self {
            inner: Arc::new(resolver),
            dns_ms: histogram!(metric_names::CHECK_DNS_MS),
        })
    }

    pub async fn resolve_addrs(&self, host: &str) -> anyhow::Result<Vec<IpAddr>> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        let start = Instant::now();
        let lookup: LookupIp = self.inner.lookup_ip(host).await?;
        self.dns_ms.record(start.elapsed().as_millis() as f64);
        Ok(lookup.iter().collect())
    }

    /// Underlying `hickory_resolver::TokioResolver`. Exposed so the DNS
    /// monitor probe can issue arbitrary-record-type queries through the
    /// same cached, shared resolver used for HTTP/TCP/TLS host lookups —
    /// no second connection pool, no second hosts file parse.
    pub fn inner(&self) -> &TokioResolver {
        &self.inner
    }
}

/// Build a one-shot resolver pointed at a single `ip` or `ip:port`
/// nameserver, with the given query timeout. Used by the DNS monitor
/// probe when the check specifies a custom resolver — bypasses the
/// shared system resolver so the probe measures *that* resolver's view.
pub fn build_single_resolver(addr: &str, timeout: Duration) -> anyhow::Result<TokioResolver> {
    let sock = parse_resolver_addr(addr)?;
    let mut udp = ConnectionConfig::udp();
    let mut tcp = ConnectionConfig::tcp();
    udp.port = sock.port();
    tcp.port = sock.port();
    let ns = NameServerConfig::new(sock.ip(), true, vec![udp, tcp]);
    let cfg = ResolverConfig::from_parts(None, vec![], vec![ns]);
    let mut opts = ResolverOpts::default();
    opts.timeout = timeout;
    opts.attempts = 1;
    opts.try_tcp_on_error = true;
    Resolver::builder_with_config(cfg, TokioRuntimeProvider::default())
        .with_options(opts)
        .build()
        .context("failed to build single-server resolver")
}

/// `"1.1.1.1"` / `"1.1.1.1:53"` / `"[::1]:53"` → `SocketAddr` (port 53
/// default). Shared with the API validator so what passes validation
/// is *exactly* what the probe will pass to the resolver.
pub fn parse_resolver_addr(s: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(sock) = s.parse::<SocketAddr>() {
        return Ok(sock);
    }
    let ip: IpAddr = s
        .parse()
        .with_context(|| format!("invalid resolver address: {s}"))?;
    Ok(SocketAddr::new(ip, 53))
}
