use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache;
use sqlx::PgPool;

use crate::ad_hoc_dispatch::AdHocDispatch;
use crate::api::IdempotencyCache;
use crate::api::types::DashboardSummary;
use crate::auth::api_tokens::{
    ApiTokenLastUsedDebounce, build_debounce_cache as build_api_token_debounce,
};
use crate::auth::session::{LastUsedDebounce, build_debounce_cache};
use crate::config::AppConfig;
use crate::domain::{CheckStatus, OrgId, RegionIncidentPolicy};
use crate::email::EmailSender;
use crate::http_client::HttpClients;
use crate::http_outbound::OutboundHttpClient;
use crate::public_status::PublicSource;
use crate::quotas::{QuotaService, RateLimitService};
use crate::security::AbuseGuard;
use crate::storage::traits::RegionLatestStatus;
use crate::storage::{
    IncidentNarrationStore, MaintenanceStore, NotificationChannelStore, ResultSink, ResultsStore,
    TargetStore, TimeRange,
};
use crate::worker::WorkerPool;

/// Per-org dashboard summary snapshot. Cached for 5 seconds to absorb the
/// operator-dashboard polling cadence. Keyed by `OrgId` so a SaaS tenant's
/// dashboard never reads another tenant's last build.
pub type DashboardCache = Cache<OrgId, Arc<DashboardSummary>>;

/// Detail-page live snapshot (uptime stats + recent results + last-seen
/// status). Cached for 5 seconds so the polling cadence (60s baseline +
/// overdue/manual refreshes arriving in bursts) AND repeat full-page
/// loads (browser back/forward, multi-tab) collapse to a single CH
/// round-trip per window. Keyed `(OrgId, target_id, range_key)` so a
/// tenant never reads another's snapshot and different range tabs don't
/// share cache. `Arc` keeps clones cheap when the full-page handler
/// pulls fields out for the surrounding chrome.
pub type LiveDataCache =
    Cache<(OrgId, uuid::Uuid, &'static str), Arc<crate::web::views::targets_detail::LiveData>>;

/// Operator-dashboard page snapshot: KPI strip + per-monitor rollup +
/// sparkline buckets for one (org, range) pair. Distinct from the
/// `DashboardSummary` API cache above — that one stores the JSON donut
/// payload at `/dashboard/summary`, this one stores the full V3 HTML
/// page snapshot. 5s TTL absorbs both the htmx range re-swap and the
/// auto-refresh poll. `Arc` keeps the cache-hit path a pointer bump
/// (the snapshot can grow large at 1k+ monitors). Keyed on the static
/// range key so the four tabs don't share entries.
pub type DashboardPageCache =
    Cache<(OrgId, &'static str), Arc<crate::web::views::dashboard::DashboardSnapshot>>;

/// Builder for the 5-second per-org dashboard cache. The moka `sync::Cache`
/// is cheap to clone (everything inside is `Arc`), so it lives in `AppState`
/// directly rather than behind another `Arc`.
fn build_dashboard_cache() -> DashboardCache {
    Cache::builder()
        .time_to_live(Duration::from_secs(5))
        // 1024 distinct orgs holding a ~few-KB summary is bounded enough that
        // a runaway cache won't eat the heap. Far above any realistic
        // active-org-set in one process.
        .max_capacity(1024)
        .build()
}

/// Sized for ~10k targets × 4 range presets = 40k slots upper bound.
/// Far below that in practice (only actively-viewed targets land in
/// here), but the ceiling caps memory if a crawler hits every target.
/// Each entry ~5 KB → 40k × 5 KB ≈ 200 MB worst case; a quarter of
/// that in practice. moka evicts on capacity AND on the 5s TTL.
fn build_live_data_cache() -> LiveDataCache {
    Cache::builder()
        .time_to_live(Duration::from_secs(5))
        .max_capacity(40_000)
        .build()
}

/// Per-(org, range_key) dashboard-page snapshot cache. Each entry is
/// heavier than a per-target snapshot (one row per monitor + ~60 spark
/// buckets per monitor), so cap entries lower than `LiveDataCache`:
/// 1024 orgs × 4 ranges = 4096 max. moka evicts on capacity AND on
/// the 5s TTL.
fn build_dashboard_page_cache() -> DashboardPageCache {
    Cache::builder()
        .time_to_live(Duration::from_secs(5))
        .max_capacity(4_096)
        .build()
}

/// Per-(org, window-days) incident metrics cache for `/incidents/reports`.
/// The aggregates are a few index-backed scans; a 30s TTL collapses repeated
/// loads + window flips without staleness mattering for a report view.
pub type IncidentMetricsCache = Cache<(OrgId, u32), crate::domain::IncidentMetrics>;

fn build_incident_metrics_cache() -> IncidentMetricsCache {
    Cache::builder()
        .time_to_live(Duration::from_secs(30))
        .max_capacity(4_096)
        .build()
}

/// Process-wide enabled-regions catalog: global, re-read on every chart poll.
pub type RegionCatalogCache = Cache<(), Arc<Vec<crate::storage::RegionOption>>>;

fn build_region_catalog_cache() -> RegionCatalogCache {
    Cache::builder()
        .time_to_live(Duration::from_secs(60))
        .max_capacity(1)
        .build()
}

/// Per-org region-id list, read on the 5s dashboard poll. Short TTL collapses
/// the DISTINCT-join to one query per org per window.
pub type RegionsForOrgCache = Cache<OrgId, Arc<Vec<String>>>;

fn build_regions_for_org_cache() -> RegionsForOrgCache {
    Cache::builder()
        .time_to_live(Duration::from_secs(60))
        .max_capacity(10_000)
        .build()
}

/// Open-incident count for the nav pill; short TTL bounds the count query to
/// once per org per window regardless of page volume.
pub type NavPillCache = Cache<OrgId, u32>;

fn build_nav_pill_cache() -> NavPillCache {
    Cache::builder()
        .time_to_live(Duration::from_secs(15))
        .max_capacity(10_000)
        .build()
}

/// Per-process fast-path for recently-ingested agent `batch_id`s. NOT the
/// source of truth: it is per-replica, so during a blue/green cutover a retry
/// can land on the other color and miss here. The authoritative cross-process
/// guarantee is ClickHouse block dedup (`non_replicated_deduplication_window`):
/// the agent re-sends a byte-identical block under a stable `batch_id`, which
/// the server drops regardless of which color writes it. This cache just spares
/// the common-case retry a redundant CH round-trip. TTL past the retry budget.
pub type AgentIngestDedup = Cache<uuid::Uuid, ()>;

fn build_agent_ingest_dedup() -> AgentIngestDedup {
    Cache::builder()
        .time_to_live(Duration::from_secs(300))
        .max_capacity(100_000)
        .build()
}

/// Per-agent "last_seen written recently" set, so a chatty agent doesn't UPDATE
/// its row on every pull/push.
pub type AgentSeenDebounce = Cache<uuid::Uuid, ()>;

fn build_agent_seen_debounce() -> AgentSeenDebounce {
    Cache::builder()
        .time_to_live(Duration::from_secs(30))
        .max_capacity(10_000)
        .build()
}

/// Per-dependency readiness snapshot. Both critical stores must answer for
/// the app to be "ready". Drives `/readyz` and the external heartbeat.
#[derive(Debug, Clone, Copy)]
pub struct Readiness {
    pub postgres: bool,
    pub clickhouse: bool,
}

impl Readiness {
    pub fn all_ok(&self) -> bool {
        self.postgres && self.clickhouse
    }
}

/// A dependency that doesn't answer within this is "down" — a TCP-alive but
/// hung store must not wedge `/readyz` (and the heartbeat tick) forever. Kept
/// under the deploy cutover gate's 5s `wget -T 5` so a hung store yields a
/// clean per-dependency 503 instead of racing the prober's own timeout.
const READINESS_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Ping every critical dependency concurrently — connection-level only,
/// never tenant data. Single source of truth for "ready": `/readyz` returns
/// a per-dependency 503 from it, and the dead-man's-switch heartbeat skips
/// its external ping when this is not `all_ok` (so the snitch fires on a
/// dependency outage, not just a full process death).
pub async fn probe_readiness(
    target_store: &Arc<dyn TargetStore>,
    results_store: &Arc<dyn ResultsStore>,
) -> Readiness {
    let (postgres, clickhouse) = tokio::join!(
        ping_dependency("postgres", target_store.ping()),
        ping_dependency("clickhouse", results_store.ping()),
    );
    Readiness {
        postgres,
        clickhouse,
    }
}

async fn ping_dependency<E: std::fmt::Debug>(
    name: &str,
    ping: impl std::future::Future<Output = std::result::Result<(), E>>,
) -> bool {
    match tokio::time::timeout(READINESS_PING_TIMEOUT, ping).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!(dependency = name, error = ?e, "readiness ping failed");
            false
        }
        Err(_) => {
            tracing::warn!(dependency = name, "readiness ping timed out");
            false
        }
    }
}

/// Runtime handles required by API handlers — the storage layer plus enough
/// scheduler/worker plumbing to support `test`, `check-now`, and the dashboard.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<AppConfig>,
    /// Direct Postgres handle. Required by the `CurrentOrg` extractor (and
    /// future auth helpers) which must read `organizations` / `memberships`
    /// *outside* the tenant-scoped repositories. Org-scoped data access still
    /// goes through the repositories on this state.
    ///
    /// `None` is permitted only for in-memory test fixtures. Any production
    /// code path that observes `None` here returns an internal error via
    /// `require_db()`.
    pub db: Option<PgPool>,
    pub target_store: Arc<dyn TargetStore>,
    pub results_store: Arc<dyn ResultsStore>,
    pub result_sink: Arc<dyn ResultSink>,
    /// Where an agent's flow-run telemetry lands. `None` on a control plane
    /// without ClickHouse (test fixtures), where runs are simply not recorded.
    pub flow_run_sink: Option<Arc<dyn crate::storage::traits::FlowRunSink>>,
    /// `None` without ClickHouse, where pings still move the verdict but leave
    /// no run log.
    pub heartbeat_ping_sink: Option<Arc<dyn crate::storage::traits::HeartbeatPingSink>>,
    pub http_clients: Arc<HttpClients>,
    pub worker_pool: Arc<WorkerPool>,
    pub dashboard_cache: DashboardCache,
    pub live_data_cache: LiveDataCache,
    pub dashboard_page_cache: DashboardPageCache,
    pub incident_metrics_cache: IncidentMetricsCache,
    pub region_catalog_cache: RegionCatalogCache,
    pub regions_for_org_cache: RegionsForOrgCache,
    pub nav_pill_cache: NavPillCache,
    pub idempotency: Arc<IdempotencyCache>,
    pub public_source: Arc<dyn PublicSource>,
    pub maintenance_store: Arc<dyn MaintenanceStore>,
    pub notification_channel_store: Arc<dyn NotificationChannelStore>,
    pub status_page_store: Arc<dyn crate::storage::StatusPageStore>,
    /// Per-status-page assets (logo now; background/favicon/css later). Built
    /// from `db` so `AppState::new`'s signature stays unchanged: a Pg store
    /// when tenancy is live, an in-memory one for no-DB fixtures.
    pub page_asset_store: Arc<dyn crate::storage::PageAssetStore>,
    /// Per-monitor share links (`/m/{token}`). Built from `db` so
    /// `AppState::new`'s signature stays unchanged: a Pg store when tenancy is
    /// live, an in-memory one for no-DB fixtures.
    pub monitor_share_store: Arc<dyn crate::storage::MonitorShareStore>,
    /// Heartbeat-monitor ping tokens + last-ping persistence (`/ping/{token}`).
    pub heartbeat_store: Arc<dyn crate::storage::HeartbeatStore>,
    /// In-memory heartbeat anchors + ping rate state, taken from the worker
    /// pool's executor so the two can never diverge.
    pub heartbeat_runtime: Arc<crate::worker::heartbeat::HeartbeatRuntime>,
    /// Reusable org variables + the secret credential store. Secret values are
    /// sealed with the KEK and resolved into monitor request fields worker-side.
    /// Built from `db` so `AppState::new`'s signature stays unchanged.
    pub variable_store: Arc<dyn crate::storage::VariableStore>,
    /// Single-use Telegram link codes. Built from `db` so `AppState::new`'s
    /// signature stays unchanged.
    pub channel_link_code_store: Arc<dyn crate::storage::ChannelLinkCodeStore>,
    /// Process-wide central-bot send budget. `new()` builds a fresh one for
    /// fixtures; main replaces it with the instance the escalation engine
    /// shares — two instances would double the bot's rate budget.
    pub telegram_send_budget: Arc<crate::telegram::TelegramSendBudget>,
    pub incident_narration_store: Arc<dyn IncidentNarrationStore>,
    /// Operational incident lifecycle (acknowledge/assign/resolve/reopen +
    /// internal timeline). Built from `db` so `AppState::new`'s signature stays
    /// unchanged: a Pg store when tenancy is live, in-memory for no-DB fixtures.
    pub incident_ops_store: Arc<dyn crate::storage::IncidentOpsStore>,
    /// Escalation-policy config (owner CRUD + monitor/org binding). Built from
    /// `db` like [`Self::incident_ops_store`] so the constructor signature is
    /// unchanged.
    pub escalation_policy_store: Arc<dyn crate::storage::EscalationPolicyStore>,
    /// On-call schedule config (owner CRUD + the who-is-on-call resolver).
    pub on_call_store: Arc<dyn crate::storage::OnCallStore>,
    /// Per-member contact channels paged when a user/schedule target resolves.
    pub contact_store: Arc<dyn crate::storage::ContactStore>,
    /// Per-incident retrospective documents (one per incident).
    pub postmortem_store: Arc<dyn crate::storage::PostmortemStore>,
    pub silence_store: Arc<dyn crate::storage::SilenceStore>,
    /// Debounce cache for `sessions.last_used_at` writes — see
    /// `auth::session::touch_last_used_debounced`.
    pub session_debounce: Arc<LastUsedDebounce>,
    /// Debounce cache for `api_tokens.last_used_at` — same shape as
    /// `session_debounce` so the Bearer middleware can lazily refresh
    /// without N writes-per-second per token.
    pub api_token_debounce: Arc<ApiTokenLastUsedDebounce>,
    /// Shared outbound HTTPS client used by transactional email and every
    /// user-supplied destination. Not the per-target check client.
    pub outbound_http: OutboundHttpClient,
    /// OAuth token exchange only. Skips the SSRF guard, since the origin is
    /// operator config rather than user input — see
    /// [`SsrfGuard::operator_configured_target`]. Never reuse it for a
    /// destination that came from a request.
    pub oauth_http: OutboundHttpClient,
    /// Transactional email sender (invitations, magic-link). Provider selected
    /// by `email.provider`.
    pub email_sender: Arc<dyn EmailSender>,
    /// Plan resolution + resource-quota checks. Built from `cfg` + `db` so
    /// `AppState::new`'s signature (and every caller) stays unchanged.
    pub quotas: Arc<QuotaService>,
    /// Per-org / per-user request rate limiter. The idle-entry janitor is
    /// spawned in `build_router` against the shutdown token.
    pub rate_limits: Arc<RateLimitService>,
    /// Compiled URL-pattern + domain deny-list. Built once from `cfg.abuse`;
    /// `main` validates the patterns/YAML first so this build is total.
    pub abuse: Arc<AbuseGuard>,
    /// Starts empty, and an empty set means "no opinion", never "block
    /// everything". `main` fills it from Postgres before serving.
    pub email_policy: Arc<crate::security::EmailPolicy>,
    /// Escalation-engine signal channel. `Some` only when paging is enabled;
    /// lifecycle handlers (declare/resolve/reopen) nudge the engine through it.
    pub incident_signal_tx: Option<tokio::sync::mpsc::Sender<crate::escalation::IncidentSignal>>,
    /// KEK cipher for decrypting check credentials — needed by the agent
    /// config-pull API, which serves decrypted params to region agents.
    pub cipher: Option<Arc<crate::security::Cipher>>,
    /// Dedup of recently-ingested agent result `batch_id`s.
    pub agent_ingest_dedup: AgentIngestDedup,
    /// Debounce for agent `last_seen_at` writes — at most one UPDATE per agent
    /// per TTL, mirroring the api-token last-used debounce.
    pub agent_seen_debounce: AgentSeenDebounce,
    /// In-memory dispatch for interactive checks (test / check-now): hands a
    /// check to an agent currently holding a long-poll and routes the result
    /// back to the waiting request.
    pub ad_hoc: Arc<AdHocDispatch>,
    /// Process shutdown signal. `Some` in `main`; lets the agent long-poll
    /// (`/api/agent/dispatch`) return immediately on shutdown instead of
    /// blocking graceful drain for the full hold window.
    pub shutdown: Option<tokio_util::sync::CancellationToken>,
    /// Keys the public unsubscribe HMAC. Persisted and independent of
    /// `fingerprint_salt` so rotating that salt can't void mailed links.
    pub subscription_unsubscribe_secret: String,
    /// Keys the one-click stop link mailed to alert-channel recipients.
    /// Separate from the subscriber secret so the two authorities don't overlap.
    pub alert_channel_stop_secret: String,
    /// Keys the acknowledge link pushed to phones. Its own authority: that
    /// link silences an incident, the stop link retires a channel.
    pub incident_ack_secret: String,
}

/// Run unconditionally at boot after config parse. Encodes the per-org
/// public-surface and cookie-scope invariants in code so a misconfig is
/// loud and immediate, not a silent runtime data leak. The two functions it
/// calls are kept separate so cookie-scope can be exercised in isolation by
/// tests.
pub fn assert_per_org_status_config(cfg: &AppConfig) {
    if cfg.tenancy.subdomain_public_routes {
        let bd = cfg.public_status.base_domain.as_str();
        if bd.is_empty() || !bd.contains('.') {
            panic!(
                "public_status.base_domain = {bd:?} is empty or missing a dot; \
                 subdomain routing cannot work safely"
            );
        }
    }
    assert_cookie_scope_safe(cfg);
}

/// Refuses to boot when the MCP OAuth server is enabled but its identity URIs
/// are missing or not HTTPS. Without this the AS would mint tokens whose
/// audience is the empty/wrong resource and the resource server would then
/// reject them — a silently-broken connector. Both URIs are also the OAuth
/// `issuer` / `resource` identifiers, which MUST be absolute HTTPS in
/// production. Loopback HTTP is allowed for local development.
pub fn assert_mcp_oauth_config(cfg: &AppConfig) {
    if !cfg.mcp.oauth_enabled {
        return;
    }
    let check = |label: &str, raw: &str| {
        let url = url::Url::parse(raw).unwrap_or_else(|_| {
            panic!(
                "{label} must be a valid absolute URL when mcp.oauth_enabled = true (got {raw:?})"
            )
        });
        if url.scheme() != "https" && !crate::oauth::is_loopback_http(&url) {
            panic!(
                "{label} must be https (or http on loopback for dev) when \
                 mcp.oauth_enabled = true (got {raw:?})"
            );
        }
    };
    if cfg.mcp.resource_uri.trim().is_empty() {
        panic!("mcp.oauth_enabled = true requires mcp.resource_uri to be set");
    }
    if cfg.auth.public_base_url.trim().is_empty() {
        panic!(
            "mcp.oauth_enabled = true requires auth.public_base_url (the OAuth issuer) to be set"
        );
    }
    check("mcp.resource_uri", &cfg.mcp.resource_uri);
    check("auth.public_base_url", &cfg.auth.public_base_url);
}

/// Refuses to boot when `auth.session.cookie_domain` overlaps the per-org
/// status subdomain. Without this, a single config edit on the operator
/// host can leak `_sm_session` to every tenant's status page.
pub fn assert_cookie_scope_safe(cfg: &AppConfig) {
    let cookie_domain = cfg.auth.session.cookie_domain.as_str();
    if cookie_domain.is_empty() {
        return;
    }
    if !cfg.tenancy.subdomain_public_routes {
        return;
    }
    let base = cfg.public_status.base_domain.as_str();
    let cd = cookie_domain.trim_start_matches('.');
    if base == cd || base.ends_with(&format!(".{cd}")) {
        panic!(
            "auth.session.cookie_domain={cookie_domain:?} overlaps the \
             status-page wildcard *.{base}. Operator session cookies would \
             leak to every tenant's status page. Either unset cookie_domain, \
             or move the status surface to a different parent zone."
        );
    }
}

/// How far back the region fold reads. Long enough for every check interval to
/// have reported, short enough that a region dropped from a monitor stops
/// voting quickly.
const FOLD_LOOKBACK_HOURS: i64 = 24;

/// Fold inputs for [`AppState::folded_status`]. Heartbeats are inbound-only, so
/// a quorum over probe regions describes nothing.
pub fn folded_status_policies(
    targets: &[crate::domain::Target],
) -> impl Iterator<Item = (uuid::Uuid, RegionIncidentPolicy)> + '_ {
    targets
        .iter()
        .filter(|t| !t.check.is_passive())
        .map(|t| (t.id, t.region_policy))
}

impl AppState {
    /// Quorum-folded current status per monitor. Best-effort: a missing entry
    /// leaves the caller on the raw `last_status` rather than failing the read.
    pub async fn folded_status(
        &self,
        org: OrgId,
        range: TimeRange,
        policies: impl IntoIterator<Item = (uuid::Uuid, RegionIncidentPolicy)>,
    ) -> HashMap<uuid::Uuid, CheckStatus> {
        // Current status needs the tail of the caller's window, not all of it: a
        // region dropped from a monitor mid-window must stop voting rather than
        // freeze its last verdict into a 90-day argMax.
        let recent = TimeRange {
            from: range
                .from
                .max(range.to - chrono::Duration::hours(FOLD_LOOKBACK_HOURS)),
            to: range.to,
        };
        // Only regions that reported inside the window are in the denominator,
        // which is the rule the incident writer votes by. A region that stops
        // delivering drops out on its own; agent liveness is not consulted,
        // since the control plane's own region has no `agents` row to be live in.
        let rows = match self
            .results_store
            .latest_status_by_region(org, recent)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "region statuses unavailable, showing last result");
                return HashMap::new();
            }
        };
        let grouped = RegionLatestStatus::group(rows);
        policies
            .into_iter()
            .filter_map(|(id, policy)| {
                let statuses = grouped.get(&id)?;
                Some((id, policy.fold_regions(statuses.iter().copied())?))
            })
            .collect()
    }

    /// Borrow the Postgres pool, or return an internal error. Centralises
    /// the "tenancy enabled but db is None" cloak so every handler doesn't
    /// rewrite the same anyhow string.
    pub fn require_db(&self) -> crate::error::Result<&PgPool> {
        self.db.as_ref().ok_or_else(|| {
            crate::error::AppError::Other(anyhow::anyhow!(
                "tenancy enabled but AppState.db is None"
            ))
        })
    }

    /// `signup_policy` was validated at boot, so the fallback is unreachable;
    /// `Flag` is the reading that neither blocks nor forgets if it ever fires.
    pub async fn admit_email(&self, email: &str) -> crate::security::Admission {
        let policy = self
            .cfg
            .email_policy
            .signup_policy()
            .unwrap_or(crate::config::SignupPolicy::Flag);
        self.email_policy
            .admit(email, self.http_clients.resolver(), policy)
            .await
    }

    /// For an address we would send mail *to*. Independent of `signup_policy`:
    /// an address that takes no mail bounces, and bounce rate is a
    /// sender-reputation number every tenant shares.
    /// No DNS. For paths where the verdict cannot refuse anyway, and for
    /// unauthenticated forms, where a caller could otherwise drive uncached
    /// lookups through the resolver the monitoring workers share.
    pub fn listed_disposable(&self, email: &str) -> Option<crate::security::EmailRisk> {
        self.email_policy
            .disposable_domain(email)
            .map(|_| crate::security::EmailRisk::Disposable)
    }

    pub async fn undeliverable_email(
        &self,
        email: &str,
        surface: &'static str,
    ) -> Option<crate::security::EmailRisk> {
        let risk = self
            .email_policy
            .assess(email, self.http_clients.resolver())
            .await;
        if let Some(risk) = risk {
            crate::security::email_policy::record(surface, "refused", risk);
        }
        risk
    }

    /// Enabled-regions catalog, cached so polled readers skip the `regions` query.
    pub async fn regions_detailed(
        &self,
    ) -> crate::error::Result<Vec<crate::storage::RegionOption>> {
        if let Some(regions) = self.region_catalog_cache.get(&()) {
            return Ok((*regions).clone());
        }
        let regions = self.target_store.available_regions_detailed().await?;
        self.region_catalog_cache
            .insert((), Arc::new(regions.clone()));
        Ok(regions)
    }

    /// Per-org region-id list, cached so the polled dashboard skips the
    /// DISTINCT-join on every tick.
    pub async fn regions_for_org(&self, org: OrgId) -> crate::error::Result<Vec<String>> {
        if let Some(regions) = self.regions_for_org_cache.get(&org) {
            return Ok((*regions).clone());
        }
        let regions = self.target_store.regions_for_org(org).await?;
        self.regions_for_org_cache
            .insert(org, Arc::new(regions.clone()));
        Ok(regions)
    }

    /// Cached open-incident count for the nav pill. A store error yields 0 so a
    /// blip never breaks every page's chrome.
    pub async fn open_incident_count(&self, org: OrgId) -> u32 {
        if let Some(n) = self.nav_pill_cache.get(&org) {
            return n;
        }
        let n = self
            .incident_narration_store
            .count_active(org)
            .await
            .unwrap_or(0);
        self.nav_pill_cache.insert(org, n);
        n
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: AppConfig,
        db: Option<PgPool>,
        target_store: Arc<dyn TargetStore>,
        results_store: Arc<dyn ResultsStore>,
        result_sink: Arc<dyn ResultSink>,
        http_clients: Arc<HttpClients>,
        worker_pool: Arc<WorkerPool>,
        public_source: Arc<dyn PublicSource>,
        maintenance_store: Arc<dyn MaintenanceStore>,
        notification_channel_store: Arc<dyn NotificationChannelStore>,
        status_page_store: Arc<dyn crate::storage::StatusPageStore>,
        incident_narration_store: Arc<dyn IncidentNarrationStore>,
        outbound_http: OutboundHttpClient,
        email_sender: Arc<dyn EmailSender>,
        cipher: Option<Arc<crate::security::Cipher>>,
        quotas: Arc<QuotaService>,
    ) -> Self {
        let monitor_share_store: Arc<dyn crate::storage::MonitorShareStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgMonitorShareStore::new(
                pool,
                cipher.clone(),
            )),
            None => Arc::new(crate::storage::InMemoryMonitorShareStore::new()),
        };
        let heartbeat_store: Arc<dyn crate::storage::HeartbeatStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgHeartbeatStore::new(pool, cipher.clone())),
            None => Arc::new(crate::storage::InMemoryHeartbeatStore::new()),
        };
        let heartbeat_runtime = worker_pool.heartbeat_runtime();
        let variable_store: Arc<dyn crate::storage::VariableStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgVariableStore::new(pool, cipher.clone())),
            None => Arc::new(crate::storage::InMemoryVariableStore::new()),
        };
        let page_asset_store: Arc<dyn crate::storage::PageAssetStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgPageAssetStore::new(pool)),
            None => Arc::new(crate::storage::InMemoryPageAssetStore::new()),
        };
        let channel_link_code_store: Arc<dyn crate::storage::ChannelLinkCodeStore> =
            match db.clone() {
                Some(pool) => Arc::new(crate::storage::PgChannelLinkCodeStore::new(pool)),
                None => Arc::new(crate::storage::InMemoryChannelLinkCodeStore::new()),
            };
        let incident_ops_store: Arc<dyn crate::storage::IncidentOpsStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgIncidentOpsStore::new(pool)),
            None => Arc::new(crate::storage::InMemoryIncidentOpsStore::new()),
        };
        let escalation_policy_store: Arc<dyn crate::storage::EscalationPolicyStore> =
            match db.clone() {
                Some(pool) => Arc::new(crate::storage::PgEscalationPolicyStore::new(pool)),
                None => Arc::new(crate::storage::InMemoryEscalationPolicyStore::new()),
            };
        let on_call_store: Arc<dyn crate::storage::OnCallStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgOnCallStore::new(pool)),
            None => Arc::new(crate::storage::InMemoryOnCallStore::new()),
        };
        let contact_store: Arc<dyn crate::storage::ContactStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgContactStore::new(pool)),
            None => Arc::new(crate::storage::InMemoryContactStore::new()),
        };
        let postmortem_store: Arc<dyn crate::storage::PostmortemStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgPostmortemStore::new(pool)),
            None => Arc::new(crate::storage::InMemoryPostmortemStore::new()),
        };
        let silence_store: Arc<dyn crate::storage::SilenceStore> = match db.clone() {
            Some(pool) => Arc::new(crate::storage::PgSilenceStore::new(pool)),
            None => Arc::new(crate::storage::InMemorySilenceStore::new()),
        };
        let rate_limits = Arc::new(RateLimitService::new());
        let abuse = Arc::new(AbuseGuard::from_config(&cfg.abuse));
        let email_policy = Arc::new(crate::security::EmailPolicy::from_config(&cfg.email_policy));
        Self {
            cfg: Arc::new(cfg),
            db,
            target_store,
            results_store,
            result_sink,
            flow_run_sink: None,
            heartbeat_ping_sink: None,
            http_clients,
            worker_pool,
            dashboard_cache: build_dashboard_cache(),
            live_data_cache: build_live_data_cache(),
            dashboard_page_cache: build_dashboard_page_cache(),
            incident_metrics_cache: build_incident_metrics_cache(),
            region_catalog_cache: build_region_catalog_cache(),
            regions_for_org_cache: build_regions_for_org_cache(),
            nav_pill_cache: build_nav_pill_cache(),
            idempotency: Arc::new(IdempotencyCache::new()),
            public_source,
            maintenance_store,
            notification_channel_store,
            status_page_store,
            page_asset_store,
            monitor_share_store,
            heartbeat_store,
            heartbeat_runtime,
            variable_store,
            channel_link_code_store,
            telegram_send_budget: Arc::new(crate::telegram::TelegramSendBudget::new()),
            incident_narration_store,
            incident_ops_store,
            escalation_policy_store,
            on_call_store,
            contact_store,
            postmortem_store,
            silence_store,
            session_debounce: Arc::new(build_debounce_cache()),
            api_token_debounce: Arc::new(build_api_token_debounce()),
            outbound_http,
            oauth_http: crate::http_outbound::build_outbound_client(
                crate::security::SsrfGuard::operator_configured_target(),
            ),
            email_sender,
            quotas,
            rate_limits,
            abuse,
            email_policy,
            incident_signal_tx: None,
            cipher,
            agent_ingest_dedup: build_agent_ingest_dedup(),
            agent_seen_debounce: build_agent_seen_debounce(),
            ad_hoc: Arc::new(AdHocDispatch::new()),
            shutdown: None,
            subscription_unsubscribe_secret: String::new(),
            alert_channel_stop_secret: String::new(),
            incident_ack_secret: String::new(),
        }
    }

    /// Set the persisted secret that keys public unsubscribe links.
    pub fn with_subscription_unsubscribe_secret(mut self, secret: String) -> Self {
        self.subscription_unsubscribe_secret = secret;
        self
    }

    /// Set the persisted secret that keys alert-channel one-click stop links.
    pub fn with_alert_channel_stop_secret(mut self, secret: String) -> Self {
        self.alert_channel_stop_secret = secret;
        self
    }

    /// Set the persisted secret that keys incident acknowledge links.
    pub fn with_incident_ack_secret(mut self, secret: String) -> Self {
        self.incident_ack_secret = secret;
        self
    }

    pub fn with_flow_run_sink(
        mut self,
        sink: Arc<dyn crate::storage::traits::FlowRunSink>,
    ) -> Self {
        self.flow_run_sink = Some(sink);
        self
    }

    pub fn with_heartbeat_ping_sink(
        mut self,
        sink: Arc<dyn crate::storage::traits::HeartbeatPingSink>,
    ) -> Self {
        self.heartbeat_ping_sink = Some(sink);
        self
    }

    /// Wire the process shutdown token so held agent long-polls unblock on
    /// shutdown instead of stalling graceful drain for the hold window.
    pub fn with_shutdown(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.shutdown = Some(token);
        self
    }

    /// Share the central-bot send budget with the escalation engine — both
    /// sides must meter against the same instance.
    pub fn with_telegram_send_budget(
        mut self,
        budget: Arc<crate::telegram::TelegramSendBudget>,
    ) -> Self {
        self.telegram_send_budget = budget;
        self
    }

    /// Attach the escalation-engine signal channel so lifecycle handlers can
    /// page manual incidents. No-op wiring when paging is disabled.
    pub fn with_incident_signals(
        mut self,
        tx: tokio::sync::mpsc::Sender<crate::escalation::IncidentSignal>,
    ) -> Self {
        self.incident_signal_tx = Some(tx);
        self
    }

    /// Nudge the escalation engine that an incident changed. Best-effort and
    /// non-blocking: drops (logged) when paging is disabled, the engine has
    /// shut down, or the channel is saturated — a lifecycle request must never
    /// block on paging throughput.
    pub fn signal_incident(
        &self,
        org: OrgId,
        incident_id: uuid::Uuid,
        reason: crate::domain::NotificationReason,
    ) {
        if let Some(tx) = &self.incident_signal_tx
            && let Err(err) = tx.try_send(crate::escalation::IncidentSignal {
                org,
                incident_id,
                reason,
            })
        {
            metrics::counter!(
                crate::observability::metrics::names::ALERTS_DROPPED,
                "reason" => reason.as_db_str()
            )
            .increment(1);
            tracing::warn!(%org, %incident_id, error = %err, "incident paging signal dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        Readiness, assert_cookie_scope_safe, assert_mcp_oauth_config, assert_per_org_status_config,
        probe_readiness,
    };
    use crate::config::AppConfig;
    use crate::storage::{InMemorySink, InMemoryTargetStore, ResultsStore, TargetStore};

    #[tokio::test]
    async fn probe_readiness_reports_reachable_stores_as_up() {
        let ts: Arc<dyn TargetStore> = Arc::new(InMemoryTargetStore::new());
        let rs: Arc<dyn ResultsStore> = Arc::new(InMemorySink::new());
        let r = probe_readiness(&ts, &rs).await;
        assert!(r.all_ok());
        assert!(r.postgres && r.clickhouse);
    }

    #[test]
    fn all_ok_requires_every_dependency() {
        assert!(
            Readiness {
                postgres: true,
                clickhouse: true
            }
            .all_ok()
        );
        assert!(
            !Readiness {
                postgres: true,
                clickhouse: false
            }
            .all_ok()
        );
        assert!(
            !Readiness {
                postgres: false,
                clickhouse: true
            }
            .all_ok()
        );
    }

    /// Run `f` with the default panic hook muted (so the expected-panic
    /// cases don't spam the log with backtraces) and assert it unwound with
    /// a message containing `expect`. Matching the message stops a test
    /// passing because it tripped a *different* boot assertion than intended.
    fn assert_panics(expect: &str, f: impl FnOnce() + std::panic::UnwindSafe) {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(f);
        std::panic::set_hook(prev);
        let payload = outcome.expect_err("expected a boot-refusing panic");
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        assert!(
            msg.contains(expect),
            "panicked, but on the wrong assertion: got {msg:?}, expected it to contain {expect:?}"
        );
    }

    /// A valid SaaS-subdomain baseline: subdomain routes on, path-based off,
    /// a two-label base domain, host-only cookies. Every field the assertions
    /// read is set explicitly so env/toml overrides can't make the tests
    /// non-deterministic. Each test then flips exactly the field under test
    /// off this safe starting point.
    fn saas_subdomain_cfg() -> AppConfig {
        let mut cfg = AppConfig::load().expect("config");
        cfg.tenancy.subdomain_public_routes = true;
        cfg.tenancy.path_based_public_routes = false;
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = String::new();
        cfg
    }

    #[test]
    fn valid_saas_subdomain_config_passes() {
        assert_per_org_status_config(&saas_subdomain_cfg());
    }

    #[test]
    fn empty_base_domain_with_subdomain_routes_panics() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = String::new();
        assert_panics("empty or missing a dot", move || {
            assert_per_org_status_config(&cfg)
        });
    }

    #[test]
    fn single_label_base_domain_panics() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "local".into();
        assert_panics("empty or missing a dot", move || {
            assert_per_org_status_config(&cfg)
        });
    }

    #[test]
    fn cookie_domain_overlapping_status_wildcard_panics() {
        // `.example.com` is also sent to `*.example.com`, so the operator
        // session would ride along to every tenant's page.
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = ".example.com".into();
        assert_panics("overlaps the", move || assert_cookie_scope_safe(&cfg));
    }

    #[test]
    fn cookie_domain_equal_to_base_panics() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = "example.com".into();
        assert_panics("overlaps the", move || assert_cookie_scope_safe(&cfg));
    }

    #[test]
    fn host_only_cookie_is_always_safe() {
        // Empty cookie_domain ⇒ browser scopes to the exact host; no overlap
        // is possible even with an otherwise dangerous base domain.
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = String::new();
        assert_cookie_scope_safe(&cfg);
    }

    #[test]
    fn disjoint_cookie_domain_is_safe() {
        let mut cfg = saas_subdomain_cfg();
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = ".other-zone.net".into();
        assert_cookie_scope_safe(&cfg);
    }

    #[test]
    fn cookie_scope_unchecked_when_subdomain_routes_off() {
        // No public subdomains exist, so an overlapping cookie_domain has no
        // cross-tenant surface to leak onto.
        let mut cfg = saas_subdomain_cfg();
        cfg.tenancy.subdomain_public_routes = false;
        cfg.public_status.base_domain = "example.com".into();
        cfg.auth.session.cookie_domain = ".example.com".into();
        assert_cookie_scope_safe(&cfg);
    }

    fn oauth_on_cfg() -> AppConfig {
        let mut cfg = AppConfig::load().expect("config");
        cfg.mcp.oauth_enabled = true;
        cfg.mcp.resource_uri = "https://mcp.example.com/mcp".into();
        cfg.auth.public_base_url = "https://app.example.com".into();
        cfg
    }

    #[test]
    fn oauth_config_valid_https_passes() {
        assert_mcp_oauth_config(&oauth_on_cfg());
    }

    #[test]
    fn oauth_disabled_skips_all_checks() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.oauth_enabled = false;
        cfg.mcp.resource_uri = String::new();
        cfg.auth.public_base_url = String::new();
        assert_mcp_oauth_config(&cfg);
    }

    #[test]
    fn oauth_on_with_empty_resource_panics() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.resource_uri = String::new();
        assert_panics("requires mcp.resource_uri", move || {
            assert_mcp_oauth_config(&cfg)
        });
    }

    #[test]
    fn oauth_on_with_non_https_resource_panics() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.resource_uri = "http://mcp.example.com/mcp".into();
        assert_panics("must be https", move || assert_mcp_oauth_config(&cfg));
    }

    #[test]
    fn oauth_on_with_loopback_http_issuer_passes() {
        let mut cfg = oauth_on_cfg();
        cfg.mcp.resource_uri = "http://localhost:9000/mcp".into();
        cfg.auth.public_base_url = "http://localhost:8080".into();
        assert_mcp_oauth_config(&cfg);
    }
}
