#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::header::LINK;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, Version};
use axum::routing::get;
use chrono::Utc;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uptimepage::app::AppState;
use uptimepage::config::{
    AppConfig, CheckerConfig, CircuitBreakerConfig, DnsConfig, EmailProvider, HttpClientConfig,
    SchedulerConfig, SecurityConfig, TransactionalEmailConfig,
};
use uptimepage::domain::{
    CheckSpec, ExpectedStatus, HttpCheck, HttpMethod, OrgId, PageRef, Target, WriteSource,
};
use uptimepage::email::{EmailSender, build_email_sender};
use uptimepage::http_client::{HttpClients, build_clients};
use uptimepage::http_outbound::{OutboundHttpClient, build_outbound_client};
use uptimepage::public_status::{NoopPublicSource, PublicSource, source::FeedLinks};
use uptimepage::quotas::QuotaService;
use uptimepage::storage::{
    DomainExpiryStateStore, InMemoryDomainExpiryStateStore, InMemoryIncidentNarrationStore,
    InMemoryMaintenanceStore, InMemoryNotificationChannelStore, InMemorySink,
    InMemoryStatusPageStore, InMemoryTargetStore, IncidentNarrationStore, MaintenanceStore,
    NotificationChannelStore, PgIncidentNarrationStore, PgNotificationChannelStore,
    PgStatusPageStore, PostgresTargetStore, ResultSink, ResultsStore,
};
use uptimepage::worker::domain_expiry::{DEFAULT_MAX_STALENESS, DomainExpiryRuntime};
use uptimepage::worker::host_throttle::HostThrottle;
use uptimepage::worker::rdap::RdapClient;
use uptimepage::worker::rdap_singleflight::RdapSingleflight;
use uptimepage::worker::{ResultFanout, WorkerPool};

/// Default `DomainExpiryRuntime` for test routers. Wraps an in-memory state
/// store + a permissive host throttle + a real RDAP client (never invoked
/// in tests since no DomainExpiry probe runs through this surface).
pub fn test_domain_expiry_runtime() -> Arc<DomainExpiryRuntime> {
    let outbound = build_outbound_client(uptimepage::security::SsrfGuard::relaxed_for_tests());
    let rdap_client = Arc::new(RdapClient::new(outbound));
    let state_store: Arc<dyn DomainExpiryStateStore> =
        Arc::new(InMemoryDomainExpiryStateStore::new());
    Arc::new(DomainExpiryRuntime::new(
        Arc::new(uptimepage::worker::registration::RegistrationClient::new(
            rdap_client,
        )),
        Arc::new(RdapSingleflight::with_default_ttl()),
        state_store,
        HostThrottle::permissive(),
        DEFAULT_MAX_STALENESS,
    ))
}
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

/// Outbound HTTP client + in-memory email sender shared by every test
/// `AppState` builder. The "memory" provider buffers sends so tests can
/// assert against them.
pub fn build_test_outbound_and_email() -> (OutboundHttpClient, Arc<dyn EmailSender>) {
    // Tests need to hit localhost listeners (mock email server, etc.), so the
    // SSRF guard is relaxed — production callers pass the strict config.
    let guard = uptimepage::security::SsrfGuard::relaxed_for_tests();
    let http = build_outbound_client(guard);
    let cfg = TransactionalEmailConfig {
        provider: EmailProvider::Memory,
        ..Default::default()
    };
    let sender = build_email_sender(&cfg, guard);
    (http, sender)
}

/// Fixed org id used in every `build_test_app*` helper. Tests run with
/// in-memory stores that don't enforce the FK to `organizations`, so the
/// value just needs to be stable. Live-DB integration tests must NOT reuse
/// this id — they provision their own org via `storage::create_org_with_owner`
/// so the FK on tenant tables resolves.
/// Email admission off unless a test asks for it in `mutate`: fixtures address
/// `@example.test`, which has no mail exchanger, so leaving it on would fail
/// unrelated tests and make them depend on the network to do it.
pub fn test_config(mutate: impl FnOnce(&mut AppConfig)) -> AppConfig {
    let mut cfg = AppConfig::load().expect("config");
    cfg.email_policy.enabled = false;
    mutate(&mut cfg);
    cfg
}

pub fn test_org_id() -> OrgId {
    OrgId(Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0001))
}

/// Companion to [`test_org_id`] for in-memory routers that need to thread a
/// stable owner identity through `with_session`. Same justification: the
/// in-memory stores don't enforce FK to `users`, so any stable value works.
pub fn test_user_id() -> uptimepage::domain::UserId {
    uptimepage::domain::UserId(Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0002))
}

/// Insert a live session row (expires in 1 day) for `user`, returning its
/// `id_hash`. `active_org` stamps `active_org_id` when `Some`. Tests that
/// assert on custom expiry/idle windows build the row directly instead.
pub async fn seed_session(
    pool: &PgPool,
    id_hash: &str,
    user: uptimepage::domain::UserId,
    active_org: Option<OrgId>,
) -> String {
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, active_org_id, expires_at) \
         VALUES ($1, $2, $3, now() + interval '1 day')",
    )
    .bind(id_hash)
    .bind(user.0)
    .bind(active_org.map(|o| o.0))
    .execute(pool)
    .await
    .expect("seed session");
    id_hash.to_string()
}

/// One-line helper that wraps [`build_test_app`] with an owner session bound
/// to [`test_user_id`] + [`test_org_id`]. Use this for tests that hit
/// authenticated operator endpoints; for tests that probe the unauthenticated
/// branch, use [`build_test_app`] directly without the session layer.
pub fn build_test_app_with_owner(mutate: impl FnOnce(&mut AppConfig)) -> Router {
    with_session(
        build_test_app(mutate),
        test_user_id(),
        Some(test_org_id()),
        Some("test-owner-session"),
    )
}

/// Same as [`build_test_app_with_owner`] but for the router shape that
/// mounts the operator web UI in addition to the API.
pub fn build_test_app_with_web_and_owner(mutate: impl FnOnce(&mut AppConfig)) -> Router {
    with_session(
        build_test_app_with_web(mutate),
        test_user_id(),
        Some(test_org_id()),
        Some("test-owner-session"),
    )
}

/// Public-surface routing mode. Tests parameterise over this to exercise the
/// path-based and subdomain dispatch arms; the binary itself runs SaaS-only
/// (sessions required), so both modes share the same auth model.
#[derive(Clone, Copy, Debug)]
pub enum TenancyMode {
    /// Path-based public status pages (`/status/<slug>`).
    PathBased,
    /// Per-org wildcard subdomain public status pages.
    Subdomain,
}

/// Base domain every SaaS-shaped fixture routes on. `app.` is the operator
/// surface, `mcp.` the connector, `{slug}.` a tenant page.
pub const SAAS_BASE_DOMAIN: &str = "example.test";

pub fn saas_mcp_host() -> String {
    format!("mcp.{SAAS_BASE_DOMAIN}")
}

impl TenancyMode {
    pub fn apply(self, cfg: &mut AppConfig) {
        match self {
            TenancyMode::PathBased => {
                cfg.tenancy.path_based_public_routes = true;
                cfg.tenancy.subdomain_public_routes = false;
            }
            TenancyMode::Subdomain => {
                cfg.tenancy.path_based_public_routes = false;
                cfg.tenancy.subdomain_public_routes = true;
                cfg.public_status.base_domain = SAAS_BASE_DOMAIN.into();
            }
        }
    }
}

/// Builds a test router with an InMemory store backend, applying `mutate` to
/// the loaded config before constructing `AppState`. The cancellation token is
/// freshly created and never fires — background tasks (rate-limit GC) leak
/// until the test binary exits, which is fine for short-lived tests.
pub fn build_test_app(mutate: impl FnOnce(&mut AppConfig)) -> Router {
    build_test_app_inner(mutate, false)
}

/// Like `build_test_app` but also returns the in-memory narration store so
/// tests can seed incidents directly. Maintenance routes still go through HTTP.
pub fn build_test_app_with_seedable_incidents(
    mutate: impl FnOnce(&mut AppConfig),
) -> (Router, Arc<InMemoryIncidentNarrationStore>) {
    let cfg = test_config(mutate);
    let target_store = Arc::new(InMemoryTargetStore::new());
    let sink = Arc::new(InMemorySink::new());
    let results_store: Arc<dyn ResultsStore> = sink.clone();
    let result_sink: Arc<dyn ResultSink> = sink;
    let http_clients = Arc::new(test_client());
    let (tx, _rx) = mpsc::channel(1024);
    let pool = Arc::new(WorkerPool::new(
        cfg.checker.max_concurrent_checks.max(1),
        (*http_clients).clone(),
        cfg.circuit_breaker,
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        test_domain_expiry_runtime(),
    ));
    let public_source = Arc::new(NoopPublicSource::default());
    let maintenance_store: Arc<dyn MaintenanceStore> = Arc::new(InMemoryMaintenanceStore::new());
    let narration = Arc::new(InMemoryIncidentNarrationStore::new());
    let incident_narration_store: Arc<dyn IncidentNarrationStore> = narration.clone();
    let notification_channel_store: Arc<dyn NotificationChannelStore> =
        Arc::new(InMemoryNotificationChannelStore::new());
    let quotas = Arc::new(QuotaService::new(&cfg, None));
    let state = AppState::new(
        cfg,
        None,
        target_store,
        results_store,
        result_sink,
        http_clients,
        pool,
        public_source,
        maintenance_store,
        notification_channel_store,
        Arc::new(InMemoryStatusPageStore::new()),
        incident_narration_store,
        build_test_outbound_and_email().0,
        build_test_outbound_and_email().1,
        None,
        quotas,
    );
    let router = uptimepage::build_app_router_api_only(state, CancellationToken::new());
    // Auto-attach an owner session so operator routes resolve a CurrentOrg.
    // Tests that explicitly exercise the unauthenticated branch should
    // construct the router via `build_test_app` and stamp their own session.
    let user = uptimepage::domain::UserId(Uuid::from_u128(0xA6));
    let router = with_session(
        router,
        user,
        Some(test_org_id()),
        Some("seedable-incidents"),
    );
    (router, narration)
}

/// Like `build_test_app` but accepts a custom `PublicSource` so contract tests
/// can drive the public surface deterministically without Postgres/ClickHouse.
pub fn build_test_app_with_public_source(
    mutate: impl FnOnce(&mut AppConfig),
    public_source: Arc<dyn PublicSource>,
) -> Router {
    build_test_app_with_public_source_inner(mutate, public_source, false)
}

/// Same as [`build_test_app_with_public_source`] but additionally merges
/// `web::routes()` so the HTML `/status` page is reachable.
pub fn build_test_app_with_web_and_public_source(
    mutate: impl FnOnce(&mut AppConfig),
    public_source: Arc<dyn PublicSource>,
) -> Router {
    build_test_app_with_public_source_inner(mutate, public_source, true)
}

fn build_test_app_with_public_source_inner(
    mutate: impl FnOnce(&mut AppConfig),
    public_source: Arc<dyn PublicSource>,
    with_web: bool,
) -> Router {
    let cfg = test_config(mutate);
    let target_store = Arc::new(InMemoryTargetStore::new());
    let sink = Arc::new(InMemorySink::new());
    let results_store: Arc<dyn ResultsStore> = sink.clone();
    let result_sink: Arc<dyn ResultSink> = sink;
    let http_clients = Arc::new(test_client());
    let (tx, _rx) = mpsc::channel(1024);
    let pool = Arc::new(WorkerPool::new(
        cfg.checker.max_concurrent_checks.max(1),
        (*http_clients).clone(),
        cfg.circuit_breaker,
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        test_domain_expiry_runtime(),
    ));
    let maintenance_store: Arc<dyn MaintenanceStore> = Arc::new(InMemoryMaintenanceStore::new());
    let incident_narration_store: Arc<dyn IncidentNarrationStore> =
        Arc::new(InMemoryIncidentNarrationStore::new());
    let notification_channel_store: Arc<dyn NotificationChannelStore> =
        Arc::new(InMemoryNotificationChannelStore::new());
    let quotas = Arc::new(QuotaService::new(&cfg, None));
    let state = AppState::new(
        cfg,
        None,
        target_store,
        results_store,
        result_sink,
        http_clients,
        pool,
        public_source,
        maintenance_store,
        notification_channel_store,
        Arc::new(InMemoryStatusPageStore::new()),
        incident_narration_store,
        build_test_outbound_and_email().0,
        build_test_outbound_and_email().1,
        None,
        quotas,
    );
    if with_web {
        uptimepage::build_app_router(state, CancellationToken::new())
    } else {
        uptimepage::build_app_router_api_only(state, CancellationToken::new())
    }
}

/// Same as [`build_test_app`] but additionally merges `web::routes()` so the
/// HTML UI is reachable. Mirrors the composition in `src/main.rs`.
pub fn build_test_app_with_web(mutate: impl FnOnce(&mut AppConfig)) -> Router {
    build_test_app_inner(mutate, true)
}

fn build_test_app_inner(mutate: impl FnOnce(&mut AppConfig), with_web: bool) -> Router {
    let state = build_test_app_state(mutate);
    if with_web {
        uptimepage::build_app_router(state, CancellationToken::new())
    } else {
        uptimepage::build_app_router_api_only(state, CancellationToken::new())
    }
}

/// Build a router backed by InMemory tenant stores but with a real Postgres
/// pool wired into `AppState.db`. Org-management routes need the pool. The
/// `mutate` hook flips any knobs the caller wants. Returns the router plus
/// a freshly-provisioned `OrgId` (unique slug) so tests can stamp a session
/// on that org without colliding with parallel runs.
pub async fn build_test_app_with_pg(
    pool: PgPool,
    mutate: impl FnOnce(&mut AppConfig),
) -> (Router, OrgId) {
    let (app, org, _state) = build_test_app_with_pg_state(pool, mutate).await;
    (app, org)
}

/// Pin the account's monitor cap through the same account-scoped override the
/// writers read, so a quota test does not have to create dozens of rows.
pub async fn set_account_targets_cap(pool: &PgPool, user: uptimepage::domain::UserId, cap: i32) {
    sqlx::query(
        "UPDATE plan_overrides po \
            SET override_json = po.override_json || jsonb_build_object('max_targets', $2::int) \
           FROM accounts a \
          WHERE a.id = po.account_id AND a.owner_user_id = $1",
    )
    .bind(user.0)
    .bind(cap)
    .execute(pool)
    .await
    .expect("set_account_targets_cap");
}

/// Insert `n` monitors straight into `org`, bypassing the cap under test.
pub async fn seed_targets(pool: &PgPool, org: OrgId, n: i32) {
    for i in 0..n {
        sqlx::query(
            "INSERT INTO targets (org_id, name, check_spec, interval_secs, enabled) \
             VALUES ($1, $2, '{\"type\":\"http\",\"url\":\"https://example.test\"}'::jsonb, 300, false)",
        )
        .bind(org.0)
        .bind(format!("seeded-{i}-{}", Uuid::now_v7()))
        .execute(pool)
        .await
        .expect("seed target");
    }
}

/// The org's account plan, resolved the way the handlers do. `restore_org`
/// takes it because a restore has to re-earn every pooled cap, not just the
/// org slot.
pub async fn plan_for(pool: &PgPool, org: OrgId) -> uptimepage::domain::quota::Plan {
    let cfg = test_config(|_| {});
    let quotas = uptimepage::quotas::QuotaService::new(&cfg, Some(pool.clone()));
    (*quotas.limit_for_org(org).await.expect("resolve plan")).clone()
}

/// Org allowance [`make_user`] grants every fixture user. Well past what any
/// test needs, so a fixture never trips the cap it is not testing.
pub const FIXTURE_MAX_ORGS: i32 = 25;

/// Open `user`'s account if they have none yet and pin how many orgs it may
/// hold, through the same account-scoped override the writers read. The free
/// plan allows one org, so any test that wants several for one user says so
/// here rather than depending on a plan's number.
pub async fn allow_orgs(pool: &PgPool, user: uptimepage::domain::UserId, max_orgs: i32) {
    sqlx::query(
        "WITH a AS ( \
             INSERT INTO accounts (owner_user_id) VALUES ($1) \
             ON CONFLICT (owner_user_id) WHERE owner_user_id IS NOT NULL \
             DO UPDATE SET owner_user_id = EXCLUDED.owner_user_id \
             RETURNING id) \
         INSERT INTO plan_overrides (account_id, override_json, reason) \
         SELECT a.id, jsonb_build_object('max_orgs', $2::int), 'test fixture' FROM a \
         ON CONFLICT (account_id) DO UPDATE SET override_json = EXCLUDED.override_json",
    )
    .bind(user.0)
    .bind(max_orgs)
    .execute(pool)
    .await
    .expect("allow_orgs");
}

/// Live orgs the user's account holds — the count the org cap is measured
/// against, and what the console shows as "n of m".
pub async fn live_orgs(pool: &PgPool, user: uptimepage::domain::UserId) -> u32 {
    let (used, _cap) = uptimepage::storage::accounts::org_allowance_for_user(pool, user)
        .await
        .expect("org_allowance_for_user");
    u32::try_from(used).unwrap()
}

/// [`build_test_app_with_pg`] plus a clone of the router's `AppState`. Shares
/// every `Arc`, so a test can seed state the router then reads.
pub async fn build_test_app_with_pg_state(
    pool: PgPool,
    mutate: impl FnOnce(&mut AppConfig),
) -> (Router, OrgId, AppState) {
    let cfg = test_config(mutate);
    let slug = format!("test-{}", uuid::Uuid::now_v7().simple());
    let slug = &slug[..slug.len().min(30)];
    let (org_uuid,): (uuid::Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts DEFAULT VALUES RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Test Org', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(&pool)
    .await
    .expect("insert test org");
    let provisioned_org = OrgId(org_uuid);
    let target_store = Arc::new(InMemoryTargetStore::new());
    let sink = Arc::new(InMemorySink::new());
    let results_store: Arc<dyn ResultsStore> = sink.clone();
    let result_sink: Arc<dyn ResultSink> = sink;
    let http_clients = Arc::new(test_client());
    let (tx, _rx) = mpsc::channel(1024);
    let pool_arc = Arc::new(WorkerPool::new(
        cfg.checker.max_concurrent_checks.max(1),
        (*http_clients).clone(),
        cfg.circuit_breaker,
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        test_domain_expiry_runtime(),
    ));
    let public_source = Arc::new(NoopPublicSource::default());
    let maintenance_store: Arc<dyn MaintenanceStore> = Arc::new(InMemoryMaintenanceStore::new());
    let incident_narration_store: Arc<dyn IncidentNarrationStore> =
        Arc::new(InMemoryIncidentNarrationStore::new());
    let notification_channel_store: Arc<dyn NotificationChannelStore> =
        Arc::new(InMemoryNotificationChannelStore::new());
    let quotas = Arc::new(QuotaService::new(&cfg, Some(pool.clone())));
    let state = AppState::new(
        cfg,
        Some(pool),
        target_store,
        results_store,
        result_sink,
        http_clients,
        pool_arc,
        public_source,
        maintenance_store,
        notification_channel_store,
        Arc::new(InMemoryStatusPageStore::new()),
        incident_narration_store,
        build_test_outbound_and_email().0,
        build_test_outbound_and_email().1,
        None,
        quotas,
    );
    let app = uptimepage::build_app_router(state.clone(), CancellationToken::new());
    (app, provisioned_org, state)
}

/// Like [`build_test_app_with_pg`] but the target store is the real
/// `PostgresTargetStore` bound to a **freshly inserted org** (unique slug,
/// on an account whose `plan_id` is pinned to `free`) with an owner user attached as
/// a session on the returned router. Quota counts (`QuotaService`) and the
/// store's atomic count-in-INSERT both read the same `targets` table, and
/// the unique org isolates parallel quota tests from each other. Used by
/// the quota integration suite, which must exercise the production path.
///
/// Tests that need a different identity should call
/// [`build_test_app_with_pg_store_anon`] and stamp their own session.
pub async fn build_test_app_with_pg_store(
    pool: PgPool,
    mutate: impl FnOnce(&mut AppConfig),
) -> (Router, OrgId) {
    build_test_app_with_pg_store_tweaked(pool, mutate, |state| state).await
}

/// As [`build_test_app_with_pg_store`], with a hook on the assembled state
/// for the few tests that swap in a double (a fake billing provider).
pub async fn build_test_app_with_pg_store_tweaked(
    pool: PgPool,
    mutate: impl FnOnce(&mut AppConfig),
    tweak: impl FnOnce(AppState) -> AppState,
) -> (Router, OrgId) {
    let (app, org) = build_test_app_with_pg_store_anon_tweaked(pool.clone(), mutate, tweak).await;
    let owner = make_user(&pool, "owner").await;
    sqlx::query("INSERT INTO memberships (user_id, org_id, role) VALUES ($1, $2, 'owner')")
        .bind(owner.0)
        .bind(org.0)
        .execute(&pool)
        .await
        .expect("seed owner membership");
    // Signup hangs the org off the owner's own account; the anon builder had
    // nobody to hang it off yet, so repoint it now. Both accounts are on the
    // free plan, so no cap moves. Without this the org's account is ownerless
    // and `/me/usage` reports the owner holding zero orgs.
    sqlx::query(
        "UPDATE organizations SET account_id = a.id FROM accounts a \
         WHERE organizations.id = $1 AND a.owner_user_id = $2",
    )
    .bind(org.0)
    .bind(owner.0)
    .execute(&pool)
    .await
    .expect("point the seeded org at the owner's account");
    let app = with_session(app, owner, Some(org), Some("default-owner-session"));
    (app, org)
}

/// Same as [`build_test_app_with_pg_store`] but the returned router has no
/// session layer attached. Use this when the test owns the auth identity —
/// e.g. asserting 401 on anonymous requests or stamping a stranger session.
pub async fn build_test_app_with_pg_store_anon(
    pool: PgPool,
    mutate: impl FnOnce(&mut AppConfig),
) -> (Router, OrgId) {
    build_test_app_with_pg_store_anon_tweaked(pool, mutate, |state| state).await
}

pub async fn build_test_app_with_pg_store_anon_tweaked(
    pool: PgPool,
    mutate: impl FnOnce(&mut AppConfig),
    tweak: impl FnOnce(AppState) -> AppState,
) -> (Router, OrgId) {
    let cfg = test_config(mutate);
    let slug = format!("qt{}", uuid::Uuid::now_v7().simple());
    let slug = &slug[..slug.len().min(30)];
    let (org_uuid,): (uuid::Uuid,) = sqlx::query_as(
        "WITH a AS (INSERT INTO accounts (plan_id) VALUES ('free') RETURNING id) \
         INSERT INTO organizations (slug, name, account_id) \
         SELECT $1, 'Quota Test', a.id FROM a RETURNING id",
    )
    .bind(slug)
    .fetch_one(&pool)
    .await
    .expect("insert quota-test org");
    let provisioned_org = OrgId(org_uuid);
    let app = assemble_pg_router_tweaked(pool, cfg, tweak);
    (app, provisioned_org)
}

/// SaaS router backed by the **real** `PostgresTargetStore` (no ambient org —
/// every query is scoped by the request's `CurrentOrg`) plus an in-memory
/// results sink. Unlike [`build_test_app_with_pg_store`] this provisions no
/// org: the caller creates as many orgs as the test needs and drives requests
/// with per-org `session_layer`s. This is the harness for cross-tenant IDOR
/// regression — two orgs, one target each, one shared store.
pub async fn build_saas_router_with_pg_targets(pool: PgPool) -> Router {
    build_saas_router_with_pg_cfg(pool, |_| {}).await
}

/// As [`build_saas_router_with_pg_targets`], but lets the caller tweak the
/// config first before the status-page store + logo routes are wired.
pub async fn build_saas_router_with_pg_cfg(
    pool: PgPool,
    mutate: impl FnOnce(&mut AppConfig),
) -> Router {
    let mut cfg = test_config(|_| {});
    cfg.tenancy.path_based_public_routes = false;
    cfg.tenancy.subdomain_public_routes = true;
    // Pinned rather than inherited: an ambient `UPTIMEPAGE__` override would
    // otherwise decide whether the assertion below panics.
    cfg.public_status.base_domain = SAAS_BASE_DOMAIN.into();
    cfg.auth.session.cookie_domain = String::new();
    mutate(&mut cfg);
    assemble_pg_router(pool, cfg)
}

/// Shared tail of the PG-target-store router builders: real
/// `PostgresTargetStore` + `PgIncidentNarrationStore` + in-memory results,
/// wired into the API + web router. The incident store has to be the Postgres
/// one for tenancy tests to mean anything: the in-memory stand-in looks rows up
/// by id alone. Callers own the tenancy prelude that precedes this.
fn assemble_pg_router(pool: PgPool, cfg: AppConfig) -> Router {
    assemble_pg_router_tweaked(pool, cfg, |state| state)
}

fn assemble_pg_router_tweaked(
    pool: PgPool,
    cfg: AppConfig,
    tweak: impl FnOnce(AppState) -> AppState,
) -> Router {
    // Subdomain routing without a base domain cannot boot, and 404s every
    // request that carries a `Host`. Fail here, not as a mystery 404.
    cfg.assert_per_org_status();
    let target_store = Arc::new(PostgresTargetStore::from_pool(pool.clone(), None));
    let sink = Arc::new(InMemorySink::new());
    let results_store: Arc<dyn ResultsStore> = sink.clone();
    let result_sink: Arc<dyn ResultSink> = sink;
    let http_clients = Arc::new(test_client());
    let (tx, _rx) = mpsc::channel(1024);
    let pool_arc = Arc::new(WorkerPool::new(
        cfg.checker.max_concurrent_checks.max(1),
        (*http_clients).clone(),
        cfg.circuit_breaker,
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        test_domain_expiry_runtime(),
    ));
    let public_source = Arc::new(NoopPublicSource::default());
    let maintenance_store: Arc<dyn MaintenanceStore> = Arc::new(InMemoryMaintenanceStore::new());
    let incident_narration_store: Arc<dyn IncidentNarrationStore> =
        Arc::new(PgIncidentNarrationStore::new(pool.clone()));
    // Postgres for the same reason as the incident store: the in-memory
    // stand-in answers by id alone, so a channel tenancy test against it proves
    // nothing.
    let notification_channel_store: Arc<dyn NotificationChannelStore> =
        Arc::new(PgNotificationChannelStore::new(pool.clone(), None));
    let status_page_store = Arc::new(PgStatusPageStore::new(pool.clone()));
    let quotas = Arc::new(QuotaService::new(&cfg, Some(pool.clone())));
    let state = AppState::new(
        cfg,
        Some(pool),
        target_store,
        results_store,
        result_sink,
        http_clients,
        pool_arc,
        public_source,
        maintenance_store,
        notification_channel_store,
        status_page_store,
        incident_narration_store,
        build_test_outbound_and_email().0,
        build_test_outbound_and_email().1,
        None,
        quotas,
    );
    uptimepage::build_app_router(tweak(state), CancellationToken::new())
}

/// Layer that stamps the provided `Session` onto every request's extensions.
/// `Session::from_request_parts` reads from the extensions when present, so
/// tests can drive authenticated routes without the real auth backend.
pub fn session_layer(
    session: uptimepage::request::Session,
) -> axum::Extension<uptimepage::request::Session> {
    axum::Extension(session)
}

/// Insert a fresh user with a unique email and return its id. `prefix` only
/// disambiguates the email in shared-DB logs — uniqueness comes from the
/// `Uuid::now_v7()` suffix, so concurrent suites never collide.
///
/// The user's account is opened with room for several orgs: fixtures that
/// happen to need a second org are not testing the org cap, and the free
/// plan's real allowance is one. Tests that *do* exercise the cap set their
/// own number with [`allow_orgs`], or drop to the plan's with
/// [`plan_default_orgs`].
pub async fn make_user(pool: &PgPool, prefix: &str) -> uptimepage::domain::UserId {
    let email = format!("{prefix}-{}@test.example", Uuid::now_v7());
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO users (email, terms_version, privacy_version) \
         VALUES ($1, 'v1', 'v1') RETURNING id",
    )
    .bind(&email)
    .fetch_one(pool)
    .await
    .expect("insert user");
    let user = uptimepage::domain::UserId(id);
    allow_orgs(pool, user, FIXTURE_MAX_ORGS).await;
    user
}

/// Drop the fixture's org allowance so the user's account is measured against
/// its plan again.
pub async fn plan_default_orgs(pool: &PgPool, user: uptimepage::domain::UserId) {
    sqlx::query(
        "DELETE FROM plan_overrides po USING accounts a \
         WHERE a.id = po.account_id AND a.owner_user_id = $1",
    )
    .bind(user.0)
    .execute(pool)
    .await
    .expect("plan_default_orgs");
}

/// `{prefix}-{8 hex}` — the tail of a v4 (pure-random) UUID, so two slugs
/// minted in the same millisecond can't collide (v7's leading bytes are the
/// timestamp). Stays within the 30-char slug limit for the short test prefixes.
pub fn unique_slug(prefix: &str) -> String {
    let id = Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &id[id.len() - 8..])
}

/// A deterministic 32-byte AES-256-GCM cipher for sealing secrets at rest in
/// PG-backed tests. The fixed key makes the at-rest envelope reproducible;
/// it never touches a real KEK.
pub fn test_cipher() -> Arc<uptimepage::security::Cipher> {
    use base64::Engine as _;
    let kek = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    Arc::new(uptimepage::security::Cipher::from_base64(&kek).unwrap())
}

/// Stamp a session onto `router`. `org` becomes `active_org_id` (the
/// `CurrentOrg` extractor verifies `user` is an active member of it);
/// `session_id` sets the session id. Supersedes the per-file `as_member` /
/// `app_with_session` / `with_session` copies.
/// Email the injected session carries. The nav identity renders the session
/// email, not a DB lookup, so assertions on identity must expect this.
pub fn session_email(user: uptimepage::domain::UserId) -> String {
    format!("u-{}@test.example", user.0)
}

pub fn with_session(
    router: Router,
    user: uptimepage::domain::UserId,
    org: Option<OrgId>,
    session_id: Option<&str>,
) -> Router {
    router.layer(session_layer(uptimepage::request::Session {
        user: Some(uptimepage::request::User {
            id: user,
            email: session_email(user),
        }),
        active_org_id: org,
        session_id_hash: session_id.map(str::to_owned),
    }))
}

/// Shared `AppState` builder used by the router helpers above and by tests
/// that need to exercise an extractor directly without going through HTTP.
/// In-memory stores, no Postgres pool (`db: None`); callers that require a
/// pool must build their own state.
pub fn build_test_app_state(mutate: impl FnOnce(&mut AppConfig)) -> AppState {
    build_test_app_state_with_email(mutate, build_test_outbound_and_email().1)
}

/// [`build_test_app_state`] with a caller-supplied email sender, so a test can
/// hold the concrete [`InMemoryEmailSender`] and assert on what was mailed.
pub fn build_test_app_state_with_email(
    mutate: impl FnOnce(&mut AppConfig),
    email_sender: Arc<dyn EmailSender>,
) -> AppState {
    let cfg = test_config(mutate);
    let target_store = Arc::new(InMemoryTargetStore::new());
    let sink = Arc::new(InMemorySink::new());
    let results_store: Arc<dyn ResultsStore> = sink.clone();
    let result_sink: Arc<dyn ResultSink> = sink;
    let http_clients = Arc::new(test_client());
    let (tx, _rx) = mpsc::channel(1024);
    let pool = Arc::new(WorkerPool::new(
        cfg.checker.max_concurrent_checks.max(1),
        (*http_clients).clone(),
        cfg.circuit_breaker,
        ResultFanout::new(tx),
        uptimepage::worker::host_throttle::HostThrottle::permissive(),
        test_domain_expiry_runtime(),
    ));
    let public_source = Arc::new(NoopPublicSource::default());
    let maintenance_store: Arc<dyn MaintenanceStore> = Arc::new(InMemoryMaintenanceStore::new());
    let incident_narration_store: Arc<dyn IncidentNarrationStore> =
        Arc::new(InMemoryIncidentNarrationStore::new());
    let notification_channel_store: Arc<dyn NotificationChannelStore> =
        Arc::new(InMemoryNotificationChannelStore::new());
    let quotas = Arc::new(QuotaService::new(&cfg, None));
    AppState::new(
        cfg,
        None,
        target_store,
        results_store,
        result_sink,
        http_clients,
        pool,
        public_source,
        maintenance_store,
        notification_channel_store,
        Arc::new(InMemoryStatusPageStore::new()),
        incident_narration_store,
        build_test_outbound_and_email().0,
        email_sender,
        None,
        quotas,
    )
}

/// Owner-session router paired with the in-memory sender its handlers write
/// to, for tests that assert on the content of a mail an endpoint sent.
pub fn build_test_app_with_owner_and_email(
    mutate: impl FnOnce(&mut AppConfig),
) -> (Router, Arc<uptimepage::email::InMemoryEmailSender>) {
    let mem = Arc::new(uptimepage::email::InMemoryEmailSender::new());
    let state = build_test_app_state_with_email(mutate, mem.clone());
    let router = uptimepage::build_app_router(state, CancellationToken::new());
    (
        with_session(
            router,
            test_user_id(),
            Some(test_org_id()),
            Some("test-owner-session"),
        ),
        mem,
    )
}

/// Builds a JSON request with `Content-Type: application/json`. Panics on
/// serialization failure — tests pass `serde_json::Value` literals so the
/// only way this fails is a typo in the test fixture.
pub fn json_request(method: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&body).expect("serialize JSON body"),
        ))
        .expect("build request")
}

/// Decodes the response body as JSON. Panics on non-JSON payloads.
pub async fn body_json(resp: axum::http::Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .expect("collect body");
    serde_json::from_slice(&bytes).expect("valid json")
}

/// The process-wide metrics recorder. One per process, so every test in a
/// binary that reads a metric shares it and must assert on deltas.
pub fn metrics_handle() -> &'static metrics_exporter_prometheus::PrometheusHandle {
    static H: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    H.get_or_init(|| {
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .expect("install prometheus test recorder")
    })
}

/// The exact value of an unlabelled series in a render, `None` while the
/// series has not been touched.
pub fn metric_value(rendered: &str, name: &str) -> Option<f64> {
    let prefix = format!("{name} ");
    rendered
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|rest| rest.trim().parse().ok())
}

pub const PRELOAD_HINT: &str = "<https://cdn.example/a.js>; rel=preload; as=script";

pub fn link_header(bytes: usize) -> HeaderMap {
    let link = vec![PRELOAD_HINT; bytes / PRELOAD_HINT.len() + 1].join(", ");
    let mut headers = HeaderMap::new();
    headers.insert(LINK, HeaderValue::from_str(&link).unwrap());
    headers
}

pub fn link_lines(lines: usize) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for _ in 0..lines {
        headers.append(LINK, HeaderValue::from_static(PRELOAD_HINT));
    }
    headers
}

/// `only` gates the 200 so a test fails, not passes, when the protocol under
/// test is no longer the one negotiated.
pub fn router_with(headers: HeaderMap, only: Version) -> Router {
    Router::new().route(
        "/",
        get(move |version: Version| {
            let headers = headers.clone();
            async move {
                let status = if version == only {
                    StatusCode::OK
                } else {
                    StatusCode::HTTP_VERSION_NOT_SUPPORTED
                };
                (status, headers, "ok")
            }
        }),
    )
}

pub async fn spawn_router(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

pub async fn spawn_self_signed_tls_router(router: Router) -> SocketAddr {
    use axum_server::tls_rustls::RustlsConfig;
    use rcgen::generate_simple_self_signed;

    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = generate_simple_self_signed(vec!["localhost".into()]).expect("gen cert");
    let cfg = RustlsConfig::from_pem(
        cert.cert.pem().into_bytes(),
        cert.signing_key.serialize_pem().into_bytes(),
    )
    .await
    .expect("rustls config");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, cfg)
            .expect("rustls server")
            .serve(router.into_make_service())
            .await
            .expect("serve");
    });

    addr
}

/// Serves a CA-issued leaf and nothing else, the shape a server takes when
/// its operator installs the certificate without the intermediate.
pub async fn spawn_truncated_chain_tls_router(router: Router) -> SocketAddr {
    use axum_server::tls_rustls::RustlsConfig;
    use rcgen::{
        BasicConstraints, CertificateParams, CustomExtension, DnType, IsCa, Issuer, KeyPair,
    };

    let _ = rustls::crypto::ring::default_provider().install_default();

    let ca_key = KeyPair::generate().expect("ca key");
    let mut ca_params = CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Test Issuing CA");
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let key = KeyPair::generate().expect("leaf key");
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).expect("leaf params");
    params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    params
        .custom_extensions
        .push(CustomExtension::from_oid_content(
            &[1, 3, 6, 1, 5, 5, 7, 1, 1],
            aia_ca_issuers_der("http://ca.test/issuer.crt"),
        ));
    let leaf = params.signed_by(&key, &issuer).expect("leaf cert");

    let cfg = RustlsConfig::from_pem(leaf.pem().into_bytes(), key.serialize_pem().into_bytes())
        .await
        .expect("rustls config");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, cfg)
            .expect("rustls server")
            .serve(router.into_make_service())
            .await
            .expect("serve");
    });

    addr
}

/// `AuthorityInfoAccessSyntax` with one id-ad-caIssuers URI. Hand-rolled
/// because rcgen models no AIA.
fn aia_ca_issuers_der(uri: &str) -> Vec<u8> {
    const CA_ISSUERS: [u8; 10] = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x02];
    let mut desc = CA_ISSUERS.to_vec();
    desc.push(0x86);
    desc.push(u8::try_from(uri.len()).expect("short test uri"));
    desc.extend_from_slice(uri.as_bytes());

    let mut inner = vec![0x30, u8::try_from(desc.len()).expect("short desc")];
    inner.extend_from_slice(&desc);
    let mut out = vec![0x30, u8::try_from(inner.len()).expect("short aia")];
    out.extend_from_slice(&inner);
    out
}

pub fn test_client() -> HttpClients {
    build_clients_with(default_dns()).unwrap()
}

pub fn test_client_with_failing_dns() -> HttpClients {
    build_clients_with(DnsConfig {
        servers: vec!["127.0.0.1:9".into()],
        ..default_dns()
    })
    .unwrap()
}

fn default_dns() -> DnsConfig {
    DnsConfig {
        cache_size: 1024,
        positive_ttl_secs: 30,
        negative_ttl_secs: 5,
        servers: vec!["1.1.1.1".into()],
    }
}

fn build_clients_with(dns_cfg: DnsConfig) -> uptimepage::error::Result<HttpClients> {
    let http_cfg = HttpClientConfig {
        tcp_keepalive_secs: 30,
        user_agent: "Uptimepage/test".into(),
    };
    let checker_cfg = CheckerConfig {
        max_concurrent_checks: 100,
        default_timeout_ms: 5_000,
        connect_timeout_ms: 2_000,
        default_check_interval_secs: 60,
        per_host_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
        rdap_max_inflight: tokio::sync::Semaphore::MAX_PERMITS,
    };
    let security_cfg = SecurityConfig {
        allow_private_targets: true,
        credentials_kek_base64: secrecy::SecretString::from(String::new()),
        trusted_proxies: vec![],
    };
    build_clients(&http_cfg, &checker_cfg, &dns_cfg, &security_cfg)
}

pub fn default_http_check(url: Url, expected: ExpectedStatus) -> HttpCheck {
    HttpCheck {
        url,
        method: HttpMethod::Get,
        timeout: Duration::from_secs(3),
        follow_redirects: false,
        max_redirects: 0,
        expected_status: expected,
        expected_body_contains: None,
        headers: HashMap::new(),
        body: None,
        verify_tls: true,
        basic_auth: None,
        bearer_token: None,
    }
}

pub fn http_target(addr: SocketAddr, path: &str, interval_ms: u64) -> Target {
    let url = Url::parse(&format!("http://{addr}{path}")).unwrap();
    Target {
        id: Uuid::now_v7(),
        name: "test".into(),
        check: CheckSpec::Http(default_http_check(url, ExpectedStatus::Exact(200))),
        interval: Duration::from_millis(interval_ms),
        enabled: true,
        tags: vec![],
        alerts: uptimepage::domain::TargetAlerts::default(),
        region_policy: Default::default(),
        alert_confirmations: 2,
        notify_recovery: true,
        renotify_interval_secs: 3600,
        group_name: None,
        owner_user_id: None,
        write_source: WriteSource::Ui,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        plan_hold_at: None,
    }
}

pub fn breaker_cfg() -> CircuitBreakerConfig {
    CircuitBreakerConfig {
        failure_threshold: 2,
        success_threshold: 1,
        open_duration_secs: 30,
        half_open_max_calls: 1,
    }
}

pub fn scheduler_cfg(refresh_secs: u64) -> SchedulerConfig {
    SchedulerConfig {
        enabled: true,
        target_refresh_interval_secs: refresh_secs,
        region: "default".to_string(),
        default_region: String::new(),
    }
}

/// `PublicSource` whose every method returns `PublicAppError::Unavailable`.
/// Useful for asserting the 503 path on public endpoints without standing up
/// a real aggregator.
pub struct UnavailablePublicSource;

#[async_trait::async_trait]
impl PublicSource for UnavailablePublicSource {
    async fn page(
        &self,
        _page: PageRef,
    ) -> Result<Arc<uptimepage::domain::PublicStatusPage>, uptimepage::error::public::PublicAppError>
    {
        Err(uptimepage::error::public::PublicAppError::Unavailable)
    }
    async fn component_history(
        &self,
        _page: PageRef,
        _id: Uuid,
        _days: u32,
    ) -> Result<
        uptimepage::domain::ComponentHistoryResponse,
        uptimepage::error::public::PublicAppError,
    > {
        Err(uptimepage::error::public::PublicAppError::Unavailable)
    }
    async fn list_incidents(
        &self,
        _page: PageRef,
        _q: uptimepage::public_status::IncidentListQuery,
    ) -> Result<
        uptimepage::pagination::CursorPage<uptimepage::domain::PublicIncident>,
        uptimepage::error::public::PublicAppError,
    > {
        Err(uptimepage::error::public::PublicAppError::Unavailable)
    }
    async fn incident_by_id(
        &self,
        _page: PageRef,
        _id: Uuid,
    ) -> Result<uptimepage::domain::PublicIncident, uptimepage::error::public::PublicAppError> {
        Err(uptimepage::error::public::PublicAppError::Unavailable)
    }
    async fn maintenance(
        &self,
        _page: PageRef,
    ) -> Result<uptimepage::domain::PublicMaintenanceList, uptimepage::error::public::PublicAppError>
    {
        Err(uptimepage::error::public::PublicAppError::Unavailable)
    }
    async fn incidents_rss(
        &self,
        _page: PageRef,
        _links: FeedLinks<'_>,
    ) -> Result<String, uptimepage::error::public::PublicAppError> {
        Err(uptimepage::error::public::PublicAppError::Unavailable)
    }
}

// ── Live-store helpers (Postgres + ClickHouse) ──────────────────────────────
//
// Both return `None` when the corresponding env var is unset, so callers can
// gate `#[ignore]` integration tests cleanly:
//
//     let Some(pool) = common::pg_pool_from_env().await else { return };
//
// Migrations run **at most once per test binary**, guarded by an async
// `Mutex<bool>` — two `#[tokio::test]` cases in the same binary call these
// helpers concurrently, and the ClickHouse migration 002 drops + recreates
// `check_results` non-idempotently, so an unguarded second call races with
// the first's `CREATE MATERIALIZED VIEW … FROM check_results` and fails with
// `UNKNOWN_TABLE`. Tests share the dev database; use fresh UUIDs per test to
// avoid cross-test interference.

static PG_MIGRATED: Mutex<bool> = Mutex::const_new(false);
static CH_MIGRATED: Mutex<bool> = Mutex::const_new(false);

pub async fn pg_pool_from_env() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connect to postgres");
    let mut guard = PG_MIGRATED.lock().await;
    if !*guard {
        sqlx::migrate!("./migrations/postgres")
            .run(&pool)
            .await
            .expect("run pg migrations");
        *guard = true;
    }
    Some(pool)
}

pub async fn ch_client_from_env() -> Option<clickhouse::Client> {
    let url = std::env::var("CLICKHOUSE_URL").ok()?;
    let user = std::env::var("CLICKHOUSE_USER").unwrap_or_else(|_| "monitor".into());
    let password = std::env::var("CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "monitor".into());
    let database = std::env::var("CLICKHOUSE_DATABASE").unwrap_or_else(|_| "monitor".into());
    let client = clickhouse::Client::default()
        .with_url(&url)
        .with_database(&database)
        .with_user(&user)
        .with_password(&password);
    let mut guard = CH_MIGRATED.lock().await;
    if !*guard {
        uptimepage::storage::migrate(&client)
            .await
            .expect("run ch migrations");
        *guard = true;
    }
    Some(client)
}

// Per-test throwaway database: `CREATE DATABASE <prefix>_<uuid>`, migrations
// applied by the caller, dropped at the end. The isolation model used by the
// auth / GDPR suites (distinct from the shared-DB `pg_pool_from_env`). Returns
// `None` when `DATABASE_URL` is unset so `#[ignore]` tests no-op cleanly.
pub async fn fresh_test_db(prefix: &str) -> Option<(String, String)> {
    use sqlx::{Connection, Executor};
    let raw = std::env::var("DATABASE_URL").ok()?;
    let mut url = Url::parse(&raw).expect("DATABASE_URL must be a valid URL");
    let test_db = format!("{prefix}_{}", Uuid::now_v7().simple());
    url.set_path("/postgres");
    let mut conn = sqlx::PgConnection::connect(url.as_str())
        .await
        .expect("fresh_test_db: connect to admin DB");
    conn.execute(format!("CREATE DATABASE {test_db}").as_str())
        .await
        .expect("fresh_test_db: CREATE DATABASE");
    let mut new_url = url.clone();
    new_url.set_path(&format!("/{test_db}"));
    Some((new_url.to_string(), test_db))
}

pub async fn drop_test_db(test_db: &str) {
    use sqlx::{Connection, Executor};
    let Ok(raw) = std::env::var("DATABASE_URL") else {
        return;
    };
    let mut url = Url::parse(&raw).expect("DATABASE_URL must be a valid URL");
    url.set_path("/postgres");
    if let Ok(mut conn) = sqlx::PgConnection::connect(url.as_str()).await {
        let _ = conn
            .execute(
                format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{test_db}' AND pid <> pg_backend_pid()"
                )
                .as_str(),
            )
            .await;
        let _ = conn
            .execute(format!("DROP DATABASE IF EXISTS {test_db}").as_str())
            .await;
    }
}

pub async fn open_test_pool(db_url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect(db_url)
        .await
        .expect("open_test_pool: connect to test DB")
}
