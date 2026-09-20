use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::{Value, json};
use uptimepage::domain::{CheckStatus, DomainExpiryCheck, OrgId};
use uptimepage::http_outbound::build_outbound_client;
use uptimepage::storage::{DomainExpiryStateStore, InMemoryDomainExpiryStateStore};
use uptimepage::worker::domain_expiry::{
    DEFAULT_MAX_STALENESS, DomainExpiryRuntime, execute_domain_expiry_check,
};
use uptimepage::worker::host_throttle::HostThrottle;
use uptimepage::worker::rdap::RdapClient;
use uptimepage::worker::rdap_singleflight::RdapSingleflight;
use uptimepage::worker::registration::RegistrationClient;
use uptimepage::worker::whois::WhoisClient;
use uuid::Uuid;

mod common;

#[derive(Clone)]
struct ServerState {
    base_url: String,
    expiration: Arc<Mutex<chrono::DateTime<Utc>>>,
    registrar: Option<String>,
    fail_lookup: bool,
    lookup_delay: Duration,
    lookups: Arc<AtomicUsize>,
}

async fn handle_bootstrap(State(state): State<ServerState>) -> axum::Json<Value> {
    axum::Json(json!({
        "version": "1.0",
        "publication": "2026-01-01T00:00:00Z",
        "services": [[["example"], [format!("{}/", state.base_url)]]]
    }))
}

async fn handle_domain(
    State(state): State<ServerState>,
    Path(_domain): Path<String>,
) -> (StatusCode, axum::Json<Value>) {
    state.lookups.fetch_add(1, Ordering::SeqCst);
    if state.fail_lookup {
        return (StatusCode::NOT_FOUND, axum::Json(json!({})));
    }
    tokio::time::sleep(state.lookup_delay).await;
    let exp = *state.expiration.lock();
    let entities = match &state.registrar {
        Some(name) => json!([{
            "objectClassName": "entity",
            "roles": ["registrar"],
            "vcardArray": ["vcard", [
                ["version", {}, "text", "4.0"],
                ["fn", {}, "text", name]
            ]]
        }]),
        None => json!([]),
    };
    let body = json!({
        "events": [
            {"eventAction": "registration", "eventDate": "2020-01-01T00:00:00Z"},
            {"eventAction": "expiration", "eventDate": exp.to_rfc3339()}
        ],
        "entities": entities
    });
    (StatusCode::OK, axum::Json(body))
}

async fn spawn_rdap_fixture(
    expiration: chrono::DateTime<Utc>,
    registrar: Option<&str>,
    fail_lookup: bool,
) -> (SocketAddr, ServerState) {
    spawn_slow_rdap_fixture(expiration, registrar, fail_lookup, Duration::ZERO).await
}

async fn spawn_slow_rdap_fixture(
    expiration: chrono::DateTime<Utc>,
    registrar: Option<&str>,
    fail_lookup: bool,
    lookup_delay: Duration,
) -> (SocketAddr, ServerState) {
    // Bind before constructing state so the bootstrap response can advertise
    // the same address we'll serve from. Can't delegate to `common::spawn_router`
    // here — it binds internally, but the fixture's bootstrap payload needs the
    // addr baked into state at construction time.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = ServerState {
        base_url: format!("http://{addr}"),
        expiration: Arc::new(Mutex::new(expiration)),
        registrar: registrar.map(str::to_owned),
        fail_lookup,
        lookup_delay,
        lookups: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/bootstrap.json", get(handle_bootstrap))
        .route("/domain/{domain}", get(handle_domain))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

fn make_check(domain: &str, warn: u32, critical: u32) -> DomainExpiryCheck {
    DomainExpiryCheck {
        domain: domain.into(),
        warn_days: warn,
        critical_days: critical,
        timeout: Duration::from_secs(5),
    }
}

fn client_for(addr: SocketAddr) -> RdapClient {
    RdapClient::with_bootstrap_url(
        build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests()),
        format!("http://{addr}/bootstrap.json"),
    )
}

fn runtime_with_client(client: RdapClient) -> DomainExpiryRuntime {
    let state: Arc<dyn DomainExpiryStateStore> = Arc::new(InMemoryDomainExpiryStateStore::new());
    DomainExpiryRuntime::new(
        Arc::new(RegistrationClient::new(Arc::new(client))),
        Arc::new(RdapSingleflight::with_default_ttl()),
        state,
        HostThrottle::permissive(),
        DEFAULT_MAX_STALENESS,
    )
}

async fn classify_one(
    addr: SocketAddr,
    check: DomainExpiryCheck,
) -> uptimepage::domain::CheckResult {
    let runtime = runtime_with_client(client_for(addr));
    execute_domain_expiry_check(
        Uuid::now_v7(),
        Uuid::now_v7(),
        &check,
        &runtime,
        &common::test_client(),
    )
    .await
}

/// Minimal port-43 server: reads the query line, writes `body`, closes.
async fn spawn_whois_fixture(body: String) -> SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut scratch = [0u8; 256];
                let _ = sock.read(&mut scratch).await;
                let _ = sock.write_all(body.as_bytes()).await;
            });
        }
    });
    addr
}

/// End-to-end fallback: RDAP has no server for `.co`, so the coordinator must
/// query WHOIS and classify from the parsed registry answer. Guards the whole
/// transport + parse + fallback path in CI, which the ignored live test cannot.
#[tokio::test]
async fn domain_expiry_falls_back_to_whois_when_rdap_lacks_tld() {
    // Bootstrap advertises only `.example`, so a `.co` RDAP lookup is
    // TldUnsupported and the coordinator drops to WHOIS.
    let (rdap_addr, _) =
        spawn_rdap_fixture(Utc::now() + chrono::Duration::days(200), None, false).await;

    let expiry = Utc::now() + chrono::Duration::days(20);
    let body = format!(
        "Domain Name: FOO.CO\r\nRegistry Expiry Date: {}\r\nRegistrar: Test Registrar\r\n",
        expiry.to_rfc3339()
    );
    let whois_addr = spawn_whois_fixture(body).await;

    let state: Arc<dyn DomainExpiryStateStore> = Arc::new(InMemoryDomainExpiryStateStore::new());
    let runtime = DomainExpiryRuntime::new(
        Arc::new(RegistrationClient::with_whois(
            Arc::new(client_for(rdap_addr)),
            WhoisClient::with_addr_override(whois_addr.ip().to_string(), whois_addr.port()),
        )),
        Arc::new(RdapSingleflight::with_default_ttl()),
        state,
        HostThrottle::permissive(),
        DEFAULT_MAX_STALENESS,
    );

    let r = execute_domain_expiry_check(
        Uuid::now_v7(),
        Uuid::now_v7(),
        &make_check("foo.co", 30, 7),
        &runtime,
        &common::test_client(),
    )
    .await;

    assert_eq!(r.status, CheckStatus::Degraded);
    let details: Value =
        serde_json::from_str(r.error.as_deref().expect("degraded carries details")).unwrap();
    assert_eq!(details["registrar"], "Test Registrar");
    assert!(details["days_remaining"].as_i64().unwrap() < 30);
}

#[tokio::test]
async fn domain_expiry_up_when_far_from_expiry() {
    let (addr, _) = spawn_rdap_fixture(
        Utc::now() + chrono::Duration::days(120),
        Some("Acme"),
        false,
    )
    .await;
    let r = classify_one(addr, make_check("foo.example", 30, 7)).await;
    assert_eq!(r.status, CheckStatus::Up);
    assert!(r.error.is_none(), "fresh Up emits no error annotation");
}

#[tokio::test]
async fn domain_expiry_degraded_under_warn_days() {
    let (addr, _) =
        spawn_rdap_fixture(Utc::now() + chrono::Duration::days(20), Some("Acme"), false).await;
    let r = classify_one(addr, make_check("foo.example", 30, 7)).await;
    assert_eq!(r.status, CheckStatus::Degraded);
    let body: Value =
        serde_json::from_str(r.error.as_deref().expect("degraded carries details")).unwrap();
    assert_eq!(body["registrar"], "Acme");
    assert!(body["days_remaining"].as_i64().unwrap() < 30);
}

#[tokio::test]
async fn domain_expiry_down_under_critical_days() {
    let (addr, _) = spawn_rdap_fixture(Utc::now() + chrono::Duration::days(3), None, false).await;
    let r = classify_one(addr, make_check("foo.example", 30, 7)).await;
    assert_eq!(r.status, CheckStatus::Down);
}

#[tokio::test]
async fn domain_expiry_down_when_expired() {
    let (addr, _) = spawn_rdap_fixture(Utc::now() - chrono::Duration::days(1), None, false).await;
    let r = classify_one(addr, make_check("foo.example", 30, 7)).await;
    assert_eq!(r.status, CheckStatus::Down);
    let body: Value =
        serde_json::from_str(r.error.as_deref().expect("down carries details")).unwrap();
    assert!(body["days_remaining"].as_i64().unwrap() < 0);
}

#[tokio::test]
async fn domain_expiry_errors_on_unknown_tld() {
    let (addr, _) = spawn_rdap_fixture(Utc::now() + chrono::Duration::days(100), None, false).await;
    let r = classify_one(addr, make_check("foo.unknowntld", 30, 7)).await;
    assert_eq!(r.status, CheckStatus::Error);
    let err = r.error.as_deref().unwrap();
    assert!(
        err.contains("no registration lookup available") && err.contains("unknowntld"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn domain_expiry_errors_on_rdap_404() {
    let (addr, _) = spawn_rdap_fixture(Utc::now() + chrono::Duration::days(100), None, true).await;
    let r = classify_one(addr, make_check("foo.example", 30, 7)).await;
    assert_eq!(r.status, CheckStatus::Error);
    assert!(r.error.as_deref().unwrap().contains("404"));
}

/// Sticky-on-failure: when the registry returns 404 but a recent last-good
/// exists in the state store, the executor surfaces the cached verdict +
/// `served_stale` annotation instead of an Error.
#[tokio::test]
async fn domain_expiry_serves_last_good_on_rdap_failure() {
    let (addr, _) = spawn_rdap_fixture(Utc::now() + chrono::Duration::days(100), None, true).await;

    let state: Arc<dyn DomainExpiryStateStore> = Arc::new(InMemoryDomainExpiryStateStore::new());
    let target = Uuid::now_v7();
    let org = OrgId(Uuid::now_v7());
    state
        .upsert_success(
            org,
            target,
            "foo.example",
            Utc::now() + chrono::Duration::days(60),
            Some("Acme"),
        )
        .await
        .unwrap();

    let runtime = DomainExpiryRuntime::new(
        Arc::new(RegistrationClient::new(Arc::new(client_for(addr)))),
        Arc::new(RdapSingleflight::with_default_ttl()),
        state,
        HostThrottle::permissive(),
        DEFAULT_MAX_STALENESS,
    );

    let r = execute_domain_expiry_check(
        target,
        org.0,
        &make_check("foo.example", 30, 7),
        &runtime,
        &common::test_client(),
    )
    .await;

    assert_eq!(r.status, CheckStatus::Up, "60-day cached answer is Up");
    // Up cached verdicts must not carry the served_stale annotation —
    // operators watch the stale-served counter instead. Customer-facing
    // surfaces would otherwise display the internal annotation text.
    assert!(
        r.error.is_none(),
        "Up cached verdict carries no error annotation, got: {:?}",
        r.error
    );
}

/// A same-TLD burst queues on the one registry slot instead of failing.
/// Checks that run out of time while queued never reach the registry.
#[tokio::test]
async fn domain_expiry_same_tld_burst_queues_on_the_registry_slot() {
    let (addr, server) = spawn_slow_rdap_fixture(
        Utc::now() + chrono::Duration::days(120),
        None,
        false,
        Duration::from_millis(300),
    )
    .await;
    let state: Arc<dyn DomainExpiryStateStore> = Arc::new(InMemoryDomainExpiryStateStore::new());
    let runtime = Arc::new(DomainExpiryRuntime::new(
        Arc::new(RegistrationClient::new(Arc::new(client_for(addr)))),
        Arc::new(RdapSingleflight::with_default_ttl()),
        state,
        Arc::new(HostThrottle::new(2, 1)),
        DEFAULT_MAX_STALENESS,
    ));

    // Two checks with room to wait their turn, two whose deadline passes
    // while the first lookup still holds the slot.
    let checks = [
        ("a.example", Duration::from_secs(5)),
        ("b.example", Duration::from_secs(5)),
        ("c.example", Duration::from_millis(200)),
        ("d.example", Duration::from_millis(200)),
    ];
    let runs = checks.map(|(domain, timeout)| {
        let runtime = runtime.clone();
        let mut check = make_check(domain, 30, 7);
        check.timeout = timeout;
        tokio::spawn(async move {
            execute_domain_expiry_check(
                Uuid::now_v7(),
                Uuid::now_v7(),
                &check,
                &runtime,
                &common::test_client(),
            )
            .await
        })
    });
    let mut results = Vec::new();
    for run in runs {
        results.push(run.await.unwrap());
    }

    assert_eq!(results[0].status, CheckStatus::Up, "{:?}", results[0].error);
    assert_eq!(results[1].status, CheckStatus::Up, "{:?}", results[1].error);
    for r in &results[2..] {
        assert_eq!(r.status, CheckStatus::Error);
        assert_eq!(r.error.as_deref(), Some("rdap timeout"));
    }
    assert_eq!(
        server.lookups.load(Ordering::SeqCst),
        2,
        "checks that expired in the queue must not reach the registry"
    );
}
