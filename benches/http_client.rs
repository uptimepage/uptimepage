use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::future::join_all;
use tokio::runtime::{Builder, Runtime};
use uptimepage::config::{CheckerConfig, DnsConfig, HttpClientConfig, SecurityConfig};
use uptimepage::domain::{ExpectedStatus, HttpCheck, HttpMethod};
use uptimepage::http_client::{HttpClients, build_clients};
use uptimepage::worker::execute_http_check;
use url::Url;
use uuid::Uuid;

#[derive(Copy, Clone, Debug)]
enum Cores {
    /// Server + client share one OS thread (current_thread runtime).
    One,
    /// Server on its own OS thread, client on the benchmark thread. 2 OS threads total.
    Two,
}

impl Cores {
    fn tag(self) -> &'static str {
        match self {
            Self::One => "1c",
            Self::Two => "2c",
        }
    }
}

struct Fixture {
    client_rt: Runtime,
    clients: HttpClients,
    check: HttpCheck,
    // Hoisted out of the iter — target_id is fixed per-target in prod,
    // so paying Uuid::now_v7's getentropy syscall per call would skew the bench.
    target_id: Uuid,
}

fn fixture_one_core() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let addr = rt.block_on(spawn_mock());
        Fixture {
            check: default_check(addr),
            clients: build_test_clients(),
            client_rt: rt,
            target_id: Uuid::now_v7(),
        }
    })
}

fn fixture_two_cores() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let addr = spawn_mock_on_own_thread();
        let client_rt = Builder::new_current_thread().enable_all().build().unwrap();
        Fixture {
            check: default_check(addr),
            clients: build_test_clients(),
            client_rt,
            target_id: Uuid::now_v7(),
        }
    })
}

fn fixture(cores: Cores) -> &'static Fixture {
    match cores {
        Cores::One => fixture_one_core(),
        Cores::Two => fixture_two_cores(),
    }
}

fn spawn_mock_on_own_thread() -> SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("bench-server".into())
        .spawn(move || {
            let rt = Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let addr = spawn_mock().await;
                tx.send(addr).unwrap();
                std::future::pending::<()>().await;
            });
        })
        .unwrap();
    rx.recv().unwrap()
}

async fn spawn_mock() -> SocketAddr {
    let app = Router::new().route("/", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn default_check(addr: SocketAddr) -> HttpCheck {
    HttpCheck {
        url: Url::parse(&format!("http://{addr}/")).unwrap(),
        method: HttpMethod::Get,
        timeout: Duration::from_secs(5),
        follow_redirects: false,
        max_redirects: 0,
        expected_status: ExpectedStatus::Exact(200),
        expected_body_contains: None,
        headers: HashMap::new(),
        body: None,
        verify_tls: false,
        basic_auth: None,
        bearer_token: None,
        ip_version: uptimepage::domain::IpVersion::Any,
    }
}

fn build_test_clients() -> HttpClients {
    let http_cfg = HttpClientConfig {
        tcp_keepalive_secs: 60,
        user_agent: "Uptimepage/bench".into(),
    };
    let checker_cfg = CheckerConfig {
        max_concurrent_checks: 100_000,
        default_timeout_ms: 5_000,
        connect_timeout_ms: 2_000,
        default_check_interval_secs: 60,
        per_host_max_inflight: usize::MAX,
        rdap_max_inflight: usize::MAX,
    };
    let dns_cfg = DnsConfig {
        cache_size: 1024,
        positive_ttl_secs: 30,
        negative_ttl_secs: 5,
        servers: vec!["1.1.1.1".into()],
    };
    let security_cfg = SecurityConfig {
        allow_private_targets: true,
        credentials_kek_base64: secrecy::SecretString::from(String::new()),
        trusted_proxies: Vec::new(),
        disclosure: Default::default(),
    };
    build_clients(&http_cfg, &checker_cfg, &dns_cfg, &security_cfg).unwrap()
}

fn bench_single(c: &mut Criterion) {
    let mut group = c.benchmark_group("http_check_single");
    group.throughput(Throughput::Elements(1));
    for cores in [Cores::One, Cores::Two] {
        let f = fixture(cores);
        group.bench_function(cores.tag(), |b| {
            b.to_async(&f.client_rt).iter(|| async {
                execute_http_check(f.target_id, uuid::Uuid::nil(), &f.check, &f.clients).await
            });
        });
    }
    group.finish();
}

fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("http_check_throughput");
    group.sample_size(10);
    for cores in [Cores::One, Cores::Two] {
        let f = fixture(cores);
        for &concurrency in &[100usize, 1_000, 10_000, 50_000] {
            group.throughput(Throughput::Elements(concurrency as u64));
            group.measurement_time(Duration::from_secs(if concurrency >= 10_000 {
                45
            } else {
                10
            }));
            let id = BenchmarkId::new(cores.tag(), format!("c_{concurrency}"));
            group.bench_with_input(id, &concurrency, |b, &n| {
                b.to_async(&f.client_rt).iter(|| async {
                    let futs = (0..n).map(|_| {
                        execute_http_check(f.target_id, uuid::Uuid::nil(), &f.check, &f.clients)
                    });
                    join_all(futs).await
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_single, bench_throughput);
criterion_main!(benches);
