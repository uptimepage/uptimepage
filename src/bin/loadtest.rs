use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use uptimepage::config::{CheckerConfig, DnsConfig, HttpClientConfig, SecurityConfig};
use uptimepage::domain::{ExpectedStatus, HttpCheck, HttpMethod};
use uptimepage::http_client::build_clients;
use uptimepage::worker::execute_http_check;
use url::Url;
use uuid::Uuid;

const LATENCY_BATCH: usize = 1024;

#[derive(Debug)]
struct Args {
    concurrency: usize,
    duration_secs: u64,
    timeout_ms: u64,
    mock_ports: usize,
    ramp_secs: u64,
}

impl Args {
    fn parse() -> Self {
        let concurrency = env_parse("CONCURRENCY", 50_000usize).max(1);
        let duration_secs = env_parse("DURATION_SECS", 30u64);
        let timeout_ms = env_parse("TIMEOUT_MS", 5_000u64);
        let mock_ports = env_parse("MOCK_PORTS", 16usize).max(1);
        let ramp_secs = env_parse("RAMP_SECS", 2u64);
        Self {
            concurrency,
            duration_secs,
            timeout_ms,
            mock_ports,
            ramp_secs,
        }
    }
}

fn env_parse<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    println!("loadtest config: {args:?}");

    let mock_addrs = spawn_mock_servers(args.mock_ports).await;
    println!("mock servers ({}): {mock_addrs:?}", mock_addrs.len());

    let http_cfg = HttpClientConfig {
        tcp_keepalive_secs: 60,
        user_agent: "Uptimepage-LoadTest/1.0".into(),
    };
    let checker_cfg = CheckerConfig {
        max_concurrent_checks: args.concurrency,
        default_timeout_ms: args.timeout_ms,
        connect_timeout_ms: 2_000,
        default_check_interval_secs: 60,
        per_host_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
        rdap_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
    };
    // No external resolvers — the mock is on loopback; hickory should never be queried.
    let dns_cfg = DnsConfig {
        cache_size: 1024,
        positive_ttl_secs: 300,
        negative_ttl_secs: 60,
        servers: vec![],
    };
    let security_cfg = SecurityConfig {
        allow_private_targets: true,
        credentials_kek_base64: secrecy::SecretString::from(String::new()),
        trusted_proxies: vec![],
        disclosure: Default::default(),
    };
    let clients =
        build_clients(&http_cfg, &checker_cfg, &dns_cfg, &security_cfg).expect("build clients");

    let checks: Vec<Arc<HttpCheck>> = mock_addrs
        .iter()
        .map(|addr| {
            let url = Url::parse(&format!("http://{addr}/ok")).expect("parse url");
            Arc::new(HttpCheck {
                url,
                method: HttpMethod::Get,
                timeout: Duration::from_millis(args.timeout_ms),
                follow_redirects: false,
                max_redirects: 0,
                expected_status: ExpectedStatus::Exact(200),
                expected_body_contains: None,
                headers: HashMap::new(),
                body: None,
                verify_tls: true,
                basic_auth: None,
                bearer_token: None,
                ip_version: uptimepage::domain::IpVersion::Any,
            })
        })
        .collect();

    let target_id = Uuid::now_v7();
    let total = Arc::new(AtomicU64::new(0));
    let success = Arc::new(AtomicU64::new(0));
    let error_counts: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
    let latencies: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let start = Instant::now();
    let deadline = start + Duration::from_secs(args.duration_secs);

    let ramp = Duration::from_secs(args.ramp_secs);
    let stagger_ns = (ramp.as_nanos() / args.concurrency.max(1) as u128) as u64;
    let mut set: JoinSet<()> = JoinSet::new();
    for i in 0..args.concurrency {
        let clients = clients.clone();
        let check = checks[i % checks.len()].clone();
        let total = total.clone();
        let success = success.clone();
        let error_counts = error_counts.clone();
        let latencies = latencies.clone();
        let start_delay = Duration::from_nanos(stagger_ns.saturating_mul(i as u64));
        set.spawn(async move {
            if !start_delay.is_zero() {
                sleep(start_delay).await;
            }
            let mut local_latencies: Vec<u32> = Vec::with_capacity(LATENCY_BATCH);
            let mut local_errors: HashMap<String, u64> = HashMap::new();
            while Instant::now() < deadline {
                let r = execute_http_check(target_id, uuid::Uuid::nil(), &check, &clients).await;
                total.fetch_add(1, Ordering::Relaxed);
                if matches!(
                    r.status,
                    uptimepage::domain::CheckStatus::Up | uptimepage::domain::CheckStatus::Degraded
                ) {
                    success.fetch_add(1, Ordering::Relaxed);
                } else if let Some(err) = r.error.as_deref() {
                    *local_errors.entry(err.to_string()).or_insert(0) += 1;
                }
                local_latencies.push(r.duration_ms);
                if local_latencies.len() == LATENCY_BATCH {
                    latencies.lock().extend(local_latencies.drain(..));
                }
            }
            if !local_latencies.is_empty() {
                latencies.lock().extend(local_latencies);
            }
            if !local_errors.is_empty() {
                let mut shared = error_counts.lock();
                for (k, v) in local_errors {
                    *shared.entry(k).or_insert(0) += v;
                }
            }
        });
    }

    let progress_stop = CancellationToken::new();
    let progress = tokio::spawn({
        let total = total.clone();
        let stop = progress_stop.clone();
        async move {
            let mut last = 0u64;
            let mut last_t = Instant::now();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = sleep(Duration::from_secs(5)) => {}
                }
                let now = total.load(Ordering::Relaxed);
                let dt = last_t.elapsed().as_secs_f64();
                let rps = ((now - last) as f64 / dt) as u64;
                println!("[+{:>4}s] total={now} rps={rps}", start.elapsed().as_secs());
                last = now;
                last_t = Instant::now();
            }
        }
    });

    set.join_all().await;
    let elapsed = start.elapsed().as_secs_f64();
    progress_stop.cancel();
    let _ = progress.await;

    let total = total.load(Ordering::Relaxed);
    let success = success.load(Ordering::Relaxed);
    let mut samples = std::mem::take(&mut *latencies.lock());

    let rps = total as f64 / elapsed;
    let mut p = |q: f64| -> u32 {
        if samples.is_empty() {
            return 0;
        }
        let idx = ((samples.len() as f64) * q).clamp(0.0, (samples.len() - 1) as f64) as usize;
        let (_, nth, _) = samples.select_nth_unstable(idx);
        *nth
    };
    let p50 = p(0.50);
    let p95 = p(0.95);
    let p99 = p(0.99);

    let error_counts = std::mem::take(&mut *error_counts.lock());
    println!("---");
    println!("total checks: {total}");
    println!(
        "success: {success} ({:.2}%)",
        success as f64 * 100.0 / total.max(1) as f64
    );
    println!("errors: {}", total - success);
    let mut kinds: Vec<_> = error_counts.into_iter().collect();
    kinds.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    for (kind, count) in kinds.iter().take(10) {
        println!("  {kind}: {count}");
    }
    println!("elapsed s: {elapsed:.2}");
    println!("rps: {rps:.0}");
    println!("p50 ms: {p50}");
    println!("p95 ms: {p95}");
    println!("p99 ms: {p99}");
}

async fn spawn_mock_servers(n: usize) -> Vec<SocketAddr> {
    let app = Router::new().route("/ok", get(|| async { "ok" }));
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("local_addr");
        let app = app.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        out.push(addr);
    }
    out
}
