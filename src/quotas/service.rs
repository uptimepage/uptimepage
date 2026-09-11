//! Plan resolution + resource-quota checks.
//!
//! The quota subject is the **account**, not the org: the plan lives on the
//! account and every count spans the account's live orgs. An extra org buys a
//! workspace, never extra capacity, and a member invited into someone else's
//! org contributes none of their own.
//!
//! `limit_for_org` is the single read path for an org's effective limits: the
//! account's plan row, with its `plan_overrides` row folded in (cached; an
//! expired override reverts to the plan default). The `check_*` methods are
//! the *friendly-error* fast path called at handler entry; the race-safe
//! guarantee lives in the store INSERTs that take the same limit number
//! (one source of truth). Every block is recorded to `quota_events`
//! fire-and-forget.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moka::future::Cache;
use serde::Deserialize;
use sqlx::{PgConnection, PgPool};

use std::collections::HashMap;

use anyhow::Context;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::domain::quota::{Plan, evidence_ttl_days, raw_ttl_days};
use crate::domain::{AccountId, OrgId, UserId};
use crate::error::{AppError, Result};
use crate::storage::{ClampedRange, TimeRange};

/// The two physical windows a written row is stamped with: how long the row
/// lives, and how long a failed flow run's page snapshot lives inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionDays {
    pub row: u16,
    pub evidence: u16,
}

/// Bulk `org_id → physical retention days`, one query, the same ceilings as
/// [`Plan::raw_window_days`] via [`raw_ttl_days`] / [`evidence_ttl_days`]. Feeds
/// the write-path TTL snapshot ([`crate::storage::org_ttl`]), which must read
/// every active org without thrashing the plan cache. Plan-level:
/// neither column carries an override or add-on today, so no override
/// folding is applied.
pub async fn retention_days_by_org(pool: &PgPool) -> Result<HashMap<Uuid, RetentionDays>> {
    let rows: Vec<(Uuid, i32, i32)> = sqlx::query_as(
        "SELECT o.id, p.raw_days, p.evidence_days \
         FROM organizations o \
         JOIN accounts a ON a.id = o.account_id \
         JOIN plans p ON p.id = a.plan_id",
    )
    .fetch_all(pool)
    .await
    .context("load retention days by org")?;
    Ok(rows
        .into_iter()
        .map(|(id, raw, evidence)| {
            (
                id,
                RetentionDays {
                    row: raw_ttl_days(raw),
                    evidence: evidence_ttl_days(evidence, raw),
                },
            )
        })
        .collect())
}

/// Cache-key tags for the usage cache. One vocabulary, equal to the
/// `plans` column names so the transparency endpoint, the UI, and any
/// future invalidation hook all name a quota the same way.
pub mod usage_keys {
    pub const TARGETS: &str = "max_targets";
    pub const MEMBERS: &str = "max_members";
    pub const PENDING_INVITATIONS: &str = "max_pending_invitations";
    /// Not a `plans` column: an abuse ceiling, equal for every plan.
    pub const INVITATION_SENDS: &str = "invitation_sends_per_window";
    pub const PUBLIC_COMPONENTS: &str = "max_public_components";
    pub const STATUS_PAGES: &str = "max_status_pages";
    pub const MAINTENANCE_WINDOWS: &str = "max_maintenance_windows";
    pub const NOTIFICATION_CHANNELS: &str = "max_notification_channels";
    pub const ESCALATION_POLICIES: &str = "max_escalation_policies";
    pub const ON_CALL_SCHEDULES: &str = "max_on_call_schedules";
    pub const FLOW_CHECKS: &str = "max_flow_checks";
    pub const ORGS: &str = "max_orgs";
}

/// The account-pooled count queries: `$1` is the account, and each counts
/// across its live orgs via [`crate::storage::accounts::live_orgs`]. Declared
/// once and shared by the atomic friendly-check path, the store-side race-safe
/// guards, *and* the usage snapshot, so the number a customer is blocked at
/// always equals the number the usage page shows (single source).
pub(crate) mod count_sql {
    use crate::storage::accounts::live_orgs;

    macro_rules! pooled {
        ($name:ident, $sql:literal) => {
            pub fn $name() -> String {
                format!($sql, orgs = live_orgs("$1"))
            }
        };
    }

    pooled!(
        targets,
        "SELECT count(*) FROM targets WHERE org_id IN ({orgs})"
    );
    pooled!(
        flow,
        "SELECT count(*) FROM targets WHERE org_id IN ({orgs}) AND kind = 'flow'"
    );
    // Public components are distinct monitors curated onto any page — the cap
    // counts a monitor once no matter how many pages it sits on.
    pooled!(
        public_components,
        "SELECT count(DISTINCT target_id) FROM status_page_components WHERE org_id IN ({orgs})"
    );
    pooled!(
        status_pages,
        "SELECT count(*) FROM status_pages WHERE org_id IN ({orgs})"
    );
    pooled!(
        maintenance_windows,
        "SELECT count(*) FROM maintenance_windows WHERE org_id IN ({orgs})"
    );
    pooled!(
        notification_channels,
        "SELECT count(*) FROM notification_channels WHERE org_id IN ({orgs})"
    );
    pooled!(
        escalation_policies,
        "SELECT count(*) FROM escalation_policies WHERE org_id IN ({orgs}) AND deleted_at IS NULL"
    );
    pooled!(
        on_call_schedules,
        "SELECT count(*) FROM on_call_schedules WHERE org_id IN ({orgs}) AND deleted_at IS NULL"
    );
    // Seats are people, not memberships: one person in three of the account's
    // orgs takes one seat.
    pooled!(
        members,
        "SELECT count(DISTINCT user_id) FROM memberships WHERE org_id IN ({orgs})"
    );
    // Same "pending" predicate as `auth::invitations`, so the usage view and
    // the atomic invite-cap enforcer agree on what counts.
    pooled!(
        pending_invitations,
        "SELECT count(*) FROM invitations WHERE org_id IN ({orgs}) \
         AND accepted_at IS NULL AND declined_at IS NULL AND expires_at > now()"
    );
}

/// Storage-layer row for `plans`. The domain `Plan` stays `sqlx`-free
/// (per the domain/storage split); this is the only place that maps the
/// table. Field order matches the SELECT.
#[derive(sqlx::FromRow)]
struct PlanRow {
    /// The account the plan was resolved through, carried on the same row so
    /// resolution stays one query.
    account_id: Uuid,
    id: String,
    name: String,
    description: String,
    max_targets: i32,
    min_check_interval_secs: i32,
    retention_days: i32,
    raw_days: i32,
    evidence_days: i32,
    max_members: i32,
    max_pending_invitations: i32,
    max_api_tokens_per_user: i32,
    max_public_components: i32,
    max_status_pages: i32,
    max_share_links_per_monitor: i32,
    max_shared_monitors: i32,
    max_maintenance_windows: i32,
    max_notification_channels: i32,
    max_escalation_policies: i32,
    max_on_call_schedules: i32,
    max_logo_size_bytes: i32,
    max_regions: i32,
    max_orgs: i32,
    api_writes_per_minute: i32,
    api_reads_per_minute: i32,
    bulk_ops_per_minute: i32,
    test_now_per_minute: i32,
    check_now_per_minute: i32,
    custom_domain_enabled: bool,
    white_label_enabled: bool,
    sms_alerts_enabled: bool,
    incident_narration_enabled: bool,
    on_call_enabled: bool,
    max_flow_checks: i32,
    max_flow_steps: i32,
    is_listed: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<PlanRow> for Plan {
    fn from(r: PlanRow) -> Self {
        Plan {
            id: r.id,
            name: r.name,
            description: r.description,
            max_targets: r.max_targets,
            min_check_interval_secs: r.min_check_interval_secs,
            retention_days: r.retention_days,
            raw_days: r.raw_days,
            evidence_days: r.evidence_days,
            max_members: r.max_members,
            max_pending_invitations: r.max_pending_invitations,
            max_api_tokens_per_user: r.max_api_tokens_per_user,
            max_public_components: r.max_public_components,
            max_status_pages: r.max_status_pages,
            max_share_links_per_monitor: r.max_share_links_per_monitor,
            max_shared_monitors: r.max_shared_monitors,
            max_maintenance_windows: r.max_maintenance_windows,
            max_notification_channels: r.max_notification_channels,
            max_escalation_policies: r.max_escalation_policies,
            max_on_call_schedules: r.max_on_call_schedules,
            max_logo_size_bytes: r.max_logo_size_bytes,
            max_regions: r.max_regions,
            max_orgs: r.max_orgs,
            api_writes_per_minute: r.api_writes_per_minute,
            api_reads_per_minute: r.api_reads_per_minute,
            bulk_ops_per_minute: r.bulk_ops_per_minute,
            test_now_per_minute: r.test_now_per_minute,
            check_now_per_minute: r.check_now_per_minute,
            custom_domain_enabled: r.custom_domain_enabled,
            white_label_enabled: r.white_label_enabled,
            sms_alerts_enabled: r.sms_alerts_enabled,
            incident_narration_enabled: r.incident_narration_enabled,
            on_call_enabled: r.on_call_enabled,
            max_flow_checks: r.max_flow_checks,
            max_flow_steps: r.max_flow_steps,
            is_listed: r.is_listed,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// An org's quota subject: the account that owns it and that account's
/// effective plan. Resolved together because every check needs both — the cap
/// from the plan, the count from the account's pool.
#[derive(Clone)]
struct Resolved {
    account: AccountId,
    plan: Arc<Plan>,
    /// The invalidation epoch this resolution started reading under.
    epoch: u64,
}

#[derive(Clone)]
pub struct QuotaService {
    db: Option<PgPool>,
    /// Org → its account + that account's effective plan. Keyed by org because
    /// that is what request paths carry, and populated by a single
    /// `organizations ⋈ accounts ⋈ plans` query, so a steady-state cache hit is
    /// **zero** DB round-trips on the per-request hot path.
    plan_cache: Cache<OrgId, Resolved>,
    /// `(account, quota_name)` → current pooled count for the usage snapshot.
    /// TTL-only and recompute-from-DB on miss; never incremented, so a write
    /// path that forgets to adjust a counter cannot drift it (the cache
    /// contract).
    usage_cache: Cache<(AccountId, &'static str), u32>,
    /// Bumped by every account invalidation. A resolution records the value it
    /// started under, which tells a read that began before a plan change
    /// committed from one that began after, whatever order their inserts land.
    epoch: Arc<AtomicU64>,
    /// Account → epoch of its latest invalidation. Same TTL as the plan cache:
    /// past it every entry that could predate the change has expired anyway.
    invalidated: Cache<AccountId, u64>,
}

/// The account's plan plus its current pooled counts — the totals across
/// every live org it owns, which is what the caps apply to. The handler shapes
/// this into the public usage JSON; keeping the service free of the API DTO
/// keeps the count logic in one place and the wire shape in the API layer.
pub struct AccountUsage {
    pub plan: Arc<Plan>,
    /// Live orgs the account holds, against `plan.max_orgs`.
    pub orgs: i64,
    pub targets: i64,
    pub members: i64,
    pub pending_invitations: i64,
    pub public_components: i64,
    pub status_pages: i64,
    pub maintenance_windows: i64,
    pub notification_channels: i64,
}

impl QuotaService {
    pub fn new(cfg: &AppConfig, db: Option<PgPool>) -> Self {
        let plan_ttl = Duration::from_secs(cfg.quotas.plan_cache_ttl_secs.max(1));
        let usage_ttl = Duration::from_secs(cfg.quotas.usage_cache_ttl_secs.max(1));
        Self {
            db,
            plan_cache: Cache::builder()
                .time_to_live(plan_ttl)
                // Below the org count the whole fleet thrashes, since a
                // scheduler refresh touches every org in one pass.
                .max_capacity(10_000)
                .build(),
            usage_cache: Cache::builder()
                .time_to_live(usage_ttl)
                .max_capacity(4096)
                .build(),
            epoch: Arc::new(AtomicU64::new(0)),
            invalidated: Cache::builder()
                .time_to_live(plan_ttl)
                .max_capacity(10_000)
                .build(),
        }
    }

    /// Effective plan for an org — its account's plan. Without a DB (in-memory
    /// dev/test fixtures that always run single-tenant) there is no `plans`
    /// table, so quotas are not enforced — return an unlimited synthetic plan.
    pub async fn limit_for_org(&self, org: OrgId) -> Result<Arc<Plan>> {
        Ok(match self.resolve(org).await? {
            Some(r) => r.plan,
            None => Arc::new(unlimited_plan()),
        })
    }

    /// The account an org's quota is counted against. `None` only in the
    /// DB-less fixture case, where nothing is enforced.
    pub async fn account_for_org(&self, org: OrgId) -> Result<Option<AccountId>> {
        Ok(self.resolve(org).await?.map(|r| r.account))
    }

    /// Drops every cached resolution for the account, so a change lands on the
    /// next request rather than after the TTL. Call it after the write
    /// commits: a resolution in flight across the commit may insert the old
    /// plan after this runs, and only the epoch stamped here lets the lookup
    /// drop it. Tombstoned orgs are included because one can be restored
    /// inside its grace window with its old entry still live.
    pub async fn invalidate_account(&self, account: AccountId) -> Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.invalidated.insert(account, epoch).await;
        for org in crate::storage::accounts::org_ids(db, account).await? {
            self.plan_cache.invalidate(&org).await;
        }
        Ok(())
    }

    /// Org → (account, effective plan), cached. One query joins the org to its
    /// account and plan; the account-scoped override and add-on rows are folded
    /// in before the value is cached, so the TTL bounds every input equally.
    /// A miss takes a pooled connection only for as long as the load runs.
    async fn resolve(&self, org: OrgId) -> Result<Option<Resolved>> {
        let Some(db) = &self.db else {
            return Ok(None);
        };
        self.guarded(org, Source::Pool(db)).await.map(Some)
    }

    /// [`limit_for_org`] for a caller already holding a connection. A miss
    /// loads on that connection instead of waiting for a second one from the
    /// pool while the first sits idle, which is how a handful of such callers
    /// would exhaust the pool waiting on each other.
    ///
    /// [`limit_for_org`]: Self::limit_for_org
    pub async fn limit_for_org_on(&self, conn: &mut PgConnection, org: OrgId) -> Result<Arc<Plan>> {
        if self.db.is_none() {
            return Ok(Arc::new(unlimited_plan()));
        }
        Ok(self.guarded(org, Source::Conn(conn)).await?.plan)
    }

    /// The cache lookup with the invalidation guard: an entry that started
    /// reading before the account's last invalidation is discarded and loaded
    /// again, since the invalidation cannot see a read still in flight.
    async fn guarded(&self, org: OrgId, mut source: Source<'_>) -> Result<Resolved> {
        loop {
            // Read before the query so a change committed between the two is
            // caught by the epoch check, never lost to insert order.
            let started = self.epoch.load(Ordering::SeqCst);
            let resolved = match &mut source {
                Source::Pool(db) => self
                    .plan_cache
                    .try_get_with(org, async {
                        let mut conn = db.acquire().await?;
                        load(&mut conn, org, started).await
                    })
                    .await
                    .map_err(|e| lookup_error(&e))?,
                // Never joined to a load already running for this org: it may
                // be queued on the pool, and the pool may be waiting on the
                // very connection held here.
                Source::Conn(conn) => match self.plan_cache.get(&org).await {
                    Some(hit) => hit,
                    None => {
                        let loaded = load(conn, org, started)
                            .await
                            .map_err(|e| lookup_error(&e))?;
                        self.plan_cache.insert(org, loaded.clone()).await;
                        loaded
                    }
                },
            };
            match self.invalidated.get(&resolved.account).await {
                Some(at) if resolved.epoch < at => self.plan_cache.invalidate(&org).await,
                _ => return Ok(resolved),
            }
        }
    }

    /// Clamp a read range to the org's raw forensics window (raw-table reads).
    pub async fn clamp_raw(&self, org: OrgId, range: TimeRange) -> Result<ClampedRange> {
        let plan = self.limit_for_org(org).await?;
        Ok(ClampedRange::for_window(
            range,
            plan.raw_window_days(),
            chrono::Utc::now(),
        ))
    }

    /// Clamp a read range to the org's history window (rollup/chart reads).
    pub async fn clamp_history(&self, org: OrgId, range: TimeRange) -> Result<ClampedRange> {
        let plan = self.limit_for_org(org).await?;
        Ok(ClampedRange::for_window(
            range,
            plan.history_window_days(),
            chrono::Utc::now(),
        ))
    }

    /// Run one pooled count for an account. `sql` comes from [`count_sql`],
    /// where `$1` is always the account.
    async fn count(&self, sql: &str, account: AccountId) -> Result<i64> {
        let Some(db) = &self.db else { return Ok(0) };
        let n: i64 = sqlx::query_scalar(sql)
            .bind(account.0)
            .fetch_one(db)
            .await
            .map_err(|e| AppError::Other(anyhow::anyhow!("quota count: {e}")))?;
        Ok(n)
    }

    /// The shared body of every "may this account hold one more X?" check:
    /// resolve the org's account and plan, count that account's pool, and
    /// record a block when `n` more would cross the cap.
    async fn check_pooled(
        &self,
        org: OrgId,
        user: Option<UserId>,
        quota: &'static str,
        cap: impl Fn(&Plan) -> i32,
        sql: String,
        n: i64,
    ) -> Result<()> {
        let Some(r) = self.resolve(org).await? else {
            return Ok(());
        };
        let limit = i64::from(cap(&r.plan));
        let current = self.count(&sql, r.account).await?;
        if current + n > limit {
            self.record_block(org, user, quota, current, limit);
            return Err(AppError::quota_exceeded(
                quota,
                current,
                limit,
                r.plan.id.clone(),
            ));
        }
        Ok(())
    }

    /// Friendly pre-check for `n` new targets, counted across the account's
    /// orgs. The atomic guarantee is the `WHERE (count) + n <= limit` inside
    /// `TargetStore::create`/`bulk_create`, which pools the same way and is
    /// handed the same `max_targets` — this only produces the nice 422 on the
    /// common (uncontended) path.
    pub async fn check_can_create_targets(
        &self,
        org: OrgId,
        user: Option<UserId>,
        n: i64,
    ) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::TARGETS,
            |p| p.max_targets,
            count_sql::targets(),
            n,
        )
        .await
    }

    /// Friendly pre-check for `n` new flow monitors against the plan's
    /// `max_flow_checks`. Flow is heavy (a browser per check), so it carries a
    /// tighter sub-cap than the overall monitor limit. `0` is also the gate,
    /// caught earlier by `gate_flow` with a clearer message.
    pub async fn check_can_create_flow(
        &self,
        org: OrgId,
        user: Option<UserId>,
        n: i64,
    ) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::FLOW_CHECKS,
            |p| p.max_flow_checks,
            count_sql::flow(),
            n,
        )
        .await
    }

    /// Region-assignment cap. `requested` is the size of the region set the
    /// caller wants for one monitor (assignment replaces the set, so this is the
    /// whole count, not an increment). Audits a block like every other cap.
    pub async fn check_region_assignment(
        &self,
        org: OrgId,
        user: Option<UserId>,
        requested: i64,
    ) -> Result<()> {
        let plan = self.limit_for_org(org).await?;
        let limit = i64::from(plan.max_regions);
        if requested > limit {
            self.record_block(org, user, "max_regions", requested, limit);
            return Err(AppError::quota_exceeded(
                "max_regions",
                requested,
                limit,
                plan.id.clone(),
            ));
        }
        Ok(())
    }

    pub async fn check_can_create_maintenance_window(
        &self,
        org: OrgId,
        user: Option<UserId>,
    ) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::MAINTENANCE_WINDOWS,
            |p| p.max_maintenance_windows,
            count_sql::maintenance_windows(),
            1,
        )
        .await
    }

    /// Friendly pre-check for one new status page. The race-safe guarantee is
    /// the count-subquery + advisory lock inside `StatusPageStore::create`,
    /// handed the same `max_status_pages` cap.
    pub async fn check_can_create_status_page(
        &self,
        org: OrgId,
        user: Option<UserId>,
    ) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::STATUS_PAGES,
            |p| p.max_status_pages,
            count_sql::status_pages(),
            1,
        )
        .await
    }

    /// Friendly pre-check for one new notification channel. The race-safe
    /// guarantee is the count-subquery + advisory lock inside
    /// `NotificationChannelStore::create`, handed the same
    /// `max_notification_channels`; this only produces the nice 422 on the
    /// common (uncontended) path.
    pub async fn check_can_create_notification_channel(
        &self,
        org: OrgId,
        user: Option<UserId>,
    ) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::NOTIFICATION_CHANNELS,
            |p| p.max_notification_channels,
            count_sql::notification_channels(),
            1,
        )
        .await
    }

    /// Friendly pre-check for one new escalation policy. The race-safe
    /// guarantee is the count-subquery + advisory lock inside
    /// `EscalationPolicyStore::create`, handed the same `max_escalation_policies`.
    pub async fn check_can_create_escalation_policy(
        &self,
        org: OrgId,
        user: Option<UserId>,
    ) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::ESCALATION_POLICIES,
            |p| p.max_escalation_policies,
            count_sql::escalation_policies(),
            1,
        )
        .await
    }

    /// Friendly pre-check for one new on-call schedule. The race-safe guarantee
    /// is the count-subquery + advisory lock inside `OnCallStore::create`,
    /// handed the same `max_on_call_schedules`.
    pub async fn check_can_create_on_call_schedule(
        &self,
        org: OrgId,
        user: Option<UserId>,
    ) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::ON_CALL_SCHEDULES,
            |p| p.max_on_call_schedules,
            count_sql::on_call_schedules(),
            1,
        )
        .await
    }

    /// Friendly pre-check for adding one member, so an over-cap invitation
    /// accept fails with a clean 422 before the token is consumed. Seats are
    /// pooled *people*: the same person in two of the account's orgs takes one
    /// seat, and re-adding them to a second org costs nothing. The race-safe
    /// guarantee is the advisory-locked count inside `orgs::add_member`, handed
    /// the same `max_members`.
    pub async fn check_can_add_member(&self, org: OrgId, user: Option<UserId>) -> Result<()> {
        self.check_pooled(
            org,
            user,
            usage_keys::MEMBERS,
            |p| p.max_members,
            count_sql::members(),
            1,
        )
        .await
    }

    /// Read-through the usage cache for one `(account, quota_name)` count.
    /// TTL-only and recompute-from-DB on miss — never an increment, so a path
    /// that forgets to adjust a counter cannot drift it (the cache contract).
    /// Two racing misses recompute the same idempotent `COUNT(*)`; harmless.
    async fn cached_count<F>(
        &self,
        account: AccountId,
        key: &'static str,
        compute: F,
    ) -> Result<i64>
    where
        F: Future<Output = Result<i64>>,
    {
        if let Some(v) = self.usage_cache.get(&(account, key)).await {
            return Ok(i64::from(v));
        }
        let n = compute.await?.max(0);
        let stored = u32::try_from(n).unwrap_or(u32::MAX);
        self.usage_cache.insert((account, key), stored).await;
        Ok(i64::from(stored))
    }

    /// Plan + current pooled counts for the usage endpoint / UI. The numbers
    /// are the account's totals across its live orgs — the same pool the caps
    /// are enforced against, so what a customer reads here explains what they
    /// are blocked at. Counts go through the 10 s usage cache. Without a DB
    /// (in-memory fixtures) the counts are zero against the synthetic
    /// unlimited plan.
    pub async fn account_usage(&self, org: OrgId) -> Result<AccountUsage> {
        let Some(r) = self.resolve(org).await? else {
            return Ok(AccountUsage {
                plan: Arc::new(unlimited_plan()),
                orgs: 0,
                targets: 0,
                members: 0,
                pending_invitations: 0,
                public_components: 0,
                status_pages: 0,
                maintenance_windows: 0,
                notification_channels: 0,
            });
        };
        let Some(db) = &self.db else {
            return Ok(AccountUsage {
                plan: r.plan,
                orgs: 0,
                targets: 0,
                members: 0,
                pending_invitations: 0,
                public_components: 0,
                status_pages: 0,
                maintenance_windows: 0,
                notification_channels: 0,
            });
        };
        let account = r.account;
        let orgs = self
            .cached_count(account, usage_keys::ORGS, async {
                crate::storage::accounts::live_org_count(db, account).await
            })
            .await?;
        let targets = self
            .cached_count(
                account,
                usage_keys::TARGETS,
                self.count(&count_sql::targets(), account),
            )
            .await?;
        let members = self
            .cached_count(
                account,
                usage_keys::MEMBERS,
                self.count(&count_sql::members(), account),
            )
            .await?;
        let pending_invitations = self
            .cached_count(
                account,
                usage_keys::PENDING_INVITATIONS,
                self.count(&count_sql::pending_invitations(), account),
            )
            .await?;
        let public_components = self
            .cached_count(
                account,
                usage_keys::PUBLIC_COMPONENTS,
                self.count(&count_sql::public_components(), account),
            )
            .await?;
        let status_pages = self
            .cached_count(
                account,
                usage_keys::STATUS_PAGES,
                self.count(&count_sql::status_pages(), account),
            )
            .await?;
        let maintenance_windows = self
            .cached_count(
                account,
                usage_keys::MAINTENANCE_WINDOWS,
                self.count(&count_sql::maintenance_windows(), account),
            )
            .await?;
        let notification_channels = self
            .cached_count(
                account,
                usage_keys::NOTIFICATION_CHANNELS,
                self.count(&count_sql::notification_channels(), account),
            )
            .await?;
        Ok(AccountUsage {
            plan: r.plan,
            orgs,
            targets,
            members,
            pending_invitations,
            public_components,
            status_pages,
            maintenance_windows,
            notification_channels,
        })
    }

    /// Append a `quota_exceeded` audit row. Fire-and-forget: a failed insert
    /// must never turn a clean 422 into a 500.
    pub fn record_block(
        &self,
        org: OrgId,
        user: Option<UserId>,
        quota_name: &'static str,
        current: i64,
        limit: i64,
    ) {
        record_quota_event(
            self.db.clone(),
            Some(org),
            user,
            "quota_exceeded",
            Some(quota_name),
            serde_json::json!({ "current": current, "limit": limit }),
            None,
        );
    }
}

/// Append a best-effort row to `quota_events`.
///
/// Fire-and-forget by design: the rate-limit reject path is already-hot
/// and already-degraded; a failing INSERT here must never escalate into
/// the user's response. Callers therefore get no return value and the
/// only failure handling is a `warn!` log line.
///
/// **Durability contract:** `quota_events` is the high-volume
/// observability stream — rate-limit hits, quota blocks, abuse rejects.
/// Under DB pressure (the exact condition rate-limit blocks happen
/// most often) some rows can be lost. Readers must treat the table as a
/// best-effort sample, never as an authoritative audit trail.
///
/// Compliance-grade audit (GDPR DSR, SOC2) goes to `org_audit_log` via
/// [`crate::storage::orgs::record_audit_tx`] — a different table with a
/// different durability contract, by design.
pub fn record_quota_event(
    db: Option<PgPool>,
    org: Option<OrgId>,
    user: Option<UserId>,
    event: &'static str,
    quota_name: Option<&'static str>,
    details: serde_json::Value,
    ip_hash: Option<String>,
) {
    let Some(db) = db else { return };
    tokio::spawn(async move {
        let res = sqlx::query(
            "INSERT INTO quota_events (org_id, user_id, event, quota_name, details, ip_hash) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(org.map(|o| o.0))
        .bind(user.map(|u| u.0))
        .bind(event)
        .bind(quota_name)
        .bind(details)
        .bind(ip_hash)
        .execute(&db)
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "quota_events insert failed (non-fatal)");
        }
    });
}

fn lookup_error(e: &sqlx::Error) -> AppError {
    match e {
        sqlx::Error::RowNotFound => {
            AppError::not_found(crate::api::error::codes::ORG_NOT_FOUND, "org not found")
        }
        _ => AppError::Other(anyhow::anyhow!("limit_for_org: {e}")),
    }
}

/// Where a cache miss reads from: a connection taken from the pool for the
/// duration of the load, or one the caller already holds.
enum Source<'a> {
    Pool(&'a PgPool),
    Conn(&'a mut PgConnection),
}

/// One org's account and effective plan, read on `conn`: the plan row with
/// the account's override and add-ons folded in.
async fn load(conn: &mut PgConnection, org: OrgId, started: u64) -> Result<Resolved, sqlx::Error> {
    let p: PlanRow = sqlx::query_as(
        "SELECT o.account_id, \
         p.id, p.name, p.description, p.max_targets, \
         p.min_check_interval_secs, p.retention_days, p.raw_days, p.evidence_days, \
         p.max_members, \
         p.max_pending_invitations, p.max_api_tokens_per_user, \
         p.max_public_components, p.max_status_pages, \
         p.max_share_links_per_monitor, p.max_shared_monitors, \
         p.max_maintenance_windows, \
         p.max_notification_channels, p.max_escalation_policies, \
         p.max_on_call_schedules, \
         p.max_logo_size_bytes, p.max_regions, p.max_orgs, \
         p.api_writes_per_minute, \
         p.api_reads_per_minute, p.bulk_ops_per_minute, \
         p.test_now_per_minute, p.check_now_per_minute, \
         p.custom_domain_enabled, p.white_label_enabled, \
         p.sms_alerts_enabled, p.incident_narration_enabled, \
         p.on_call_enabled, p.max_flow_checks, p.max_flow_steps, \
         p.is_listed, p.created_at, p.updated_at \
         FROM organizations o \
         JOIN accounts a ON a.id = o.account_id \
         JOIN plans p ON p.id = a.plan_id \
         WHERE o.id = $1",
    )
    .bind(org.0)
    .fetch_one(&mut *conn)
    .await?;
    let account = AccountId(p.account_id);
    let mut plan: Plan = p.into();
    // Per-account exception (beta customers, friends-of-the-project): a
    // present, unexpired plan_overrides row replaces the named caps. Folded
    // into the cached value, so the TTL bounds an override edit/expiry exactly
    // as it bounds a plans-table edit (same staleness contract).
    if let Some(ov) = plan_override(&mut *conn, account).await? {
        plan = apply_overrides(&plan, &ov);
    }
    // Billed add-ons stack on the resolved plan/override base.
    plan = apply_addons(&plan, &account_addons(&mut *conn, account).await?);
    // Creation assigns a region regardless, so zero is unhonourable.
    plan.max_regions = plan.max_regions.max(1);
    // An account always holds the org being resolved.
    plan.max_orgs = plan.max_orgs.max(1);
    Ok(Resolved {
        account,
        plan: Arc::new(plan),
        epoch: started,
    })
}

/// The cap fields a limit override may set. Deserialized from a
/// `plan_overrides.override_json` row; also the merge input for the
/// self-host config knob (via `From`), so `apply_overrides` is the one
/// merge for both. Unknown keys are ignored (not `deny_unknown_fields`): a
/// future cap added to `Plan` but not yet here, or a typo, must only fail to
/// apply that one key — never reject the whole row and silently revert the org
/// to plan defaults. Deliberately has no `enabled` flag — whether an override
/// applies is the caller's decision (a present unexpired row; or the self-host
/// gate), never a property of the cap bag itself.
///
/// The field list also names the keys the operator write path accepts, so a
/// cap cannot be writable without being readable or the other way round.
macro_rules! override_caps {
    ($($field:ident),* $(,)?) => {
        #[derive(Debug, Default, Deserialize)]
        #[serde(default)]
        pub(crate) struct PlanOverrides {
            $(pub(crate) $field: Option<u32>,)*
        }

        impl PlanOverrides {
            pub(crate) const KEYS: &'static [&'static str] = &[$(stringify!($field)),*];
        }
    };
}

override_caps!(
    max_targets,
    min_check_interval_secs,
    retention_days,
    max_members,
    max_pending_invitations,
    max_api_tokens_per_user,
    max_public_components,
    max_status_pages,
    max_share_links_per_monitor,
    max_shared_monitors,
    max_maintenance_windows,
    max_notification_channels,
    max_escalation_policies,
    max_on_call_schedules,
    max_regions,
    max_logo_size_bytes,
    max_orgs,
);

fn apply_overrides(base: &Plan, ov: &PlanOverrides) -> Plan {
    let mut p = base.clone();
    let take = |v: Option<u32>, cur: i32| {
        v.map(|x| i32::try_from(x).unwrap_or(i32::MAX))
            .unwrap_or(cur)
    };
    p.max_targets = take(ov.max_targets, p.max_targets);
    p.min_check_interval_secs = take(ov.min_check_interval_secs, p.min_check_interval_secs);
    p.retention_days = take(ov.retention_days, p.retention_days);
    p.max_members = take(ov.max_members, p.max_members);
    p.max_pending_invitations = take(ov.max_pending_invitations, p.max_pending_invitations);
    p.max_api_tokens_per_user = take(ov.max_api_tokens_per_user, p.max_api_tokens_per_user);
    p.max_public_components = take(ov.max_public_components, p.max_public_components);
    p.max_status_pages = take(ov.max_status_pages, p.max_status_pages);
    p.max_share_links_per_monitor = take(
        ov.max_share_links_per_monitor,
        p.max_share_links_per_monitor,
    );
    p.max_shared_monitors = take(ov.max_shared_monitors, p.max_shared_monitors);
    p.max_maintenance_windows = take(ov.max_maintenance_windows, p.max_maintenance_windows);
    p.max_notification_channels = take(ov.max_notification_channels, p.max_notification_channels);
    p.max_escalation_policies = take(ov.max_escalation_policies, p.max_escalation_policies);
    p.max_on_call_schedules = take(ov.max_on_call_schedules, p.max_on_call_schedules);
    p.max_regions = take(ov.max_regions, p.max_regions);
    p.max_logo_size_bytes = take(ov.max_logo_size_bytes, p.max_logo_size_bytes);
    p.max_orgs = take(ov.max_orgs, p.max_orgs);
    p
}

/// Active per-account limit override, if any. Expired rows are filtered in SQL,
/// so an override past its `expires_at` reverts to the plan default. A
/// *query* error propagates (so the caller's cache does not memoize a
/// degraded plan for the whole TTL on a transient DB blip — a healthy
/// override must not be dropped by unrelated infra trouble). Only the
/// benign cases are `Ok(None)`: no row, or a malformed `override_json`
/// (logged) — a bad admin row must never take an org's limits down with it.
async fn plan_override(
    conn: &mut PgConnection,
    account: AccountId,
) -> Result<Option<PlanOverrides>, sqlx::Error> {
    let json: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT override_json FROM plan_overrides \
         WHERE account_id = $1 AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(account.0)
    .fetch_optional(conn)
    .await?;
    let Some(json) = json else { return Ok(None) };
    match serde_json::from_value::<PlanOverrides>(json) {
        Ok(ov) => Ok(Some(ov)),
        Err(e) => {
            tracing::warn!(error = %e, "plan_overrides override_json invalid; ignoring");
            Ok(None)
        }
    }
}

/// Additive, billed capacity on top of the base plan (Stripe quantity items).
/// Unlike `PlanOverrides` (which replaces a cap), add-ons stack on the resolved
/// plan/override. Count caps only. Summed per type from `account_addons`.
#[derive(Debug, Default)]
struct Addons {
    extra_targets: i64,
    extra_status_pages: i64,
    extra_members: i64,
    extra_shared_monitors: i64,
    extra_notification_channels: i64,
}

fn apply_addons(base: &Plan, a: &Addons) -> Plan {
    let mut p = base.clone();
    // Saturate at i32::MAX; clamp negatives to 0 — a bad row must never shrink a cap.
    let add = |cur: i32, extra: i64| {
        i32::try_from(i64::from(cur).saturating_add(extra.max(0))).unwrap_or(i32::MAX)
    };
    p.max_targets = add(p.max_targets, a.extra_targets);
    p.max_status_pages = add(p.max_status_pages, a.extra_status_pages);
    p.max_members = add(p.max_members, a.extra_members);
    p.max_shared_monitors = add(p.max_shared_monitors, a.extra_shared_monitors);
    p.max_notification_channels = add(p.max_notification_channels, a.extra_notification_channels);
    p
}

/// Add-on quantities for an account, summed by type. Like `plan_override`: a
/// query error propagates (never memoize a degraded plan on a transient blip);
/// empty is `Ok(default)`. Unknown type (CHECK makes it impossible) is logged +
/// ignored.
async fn account_addons(
    conn: &mut PgConnection,
    account: AccountId,
) -> Result<Addons, sqlx::Error> {
    // PK (account_id, addon_type) → at most one row per type, no aggregation.
    let rows: Vec<(String, i32)> =
        sqlx::query_as("SELECT addon_type, quantity FROM account_addons WHERE account_id = $1")
            .bind(account.0)
            .fetch_all(conn)
            .await?;
    let mut a = Addons::default();
    for (kind, qty) in rows {
        let qty = i64::from(qty);
        match kind.as_str() {
            "extra_targets" => a.extra_targets = qty,
            "extra_status_pages" => a.extra_status_pages = qty,
            "extra_members" => a.extra_members = qty,
            "extra_shared_monitors" => a.extra_shared_monitors = qty,
            "extra_notification_channels" => a.extra_notification_channels = qty,
            other => tracing::warn!(addon_type = other, "unknown account_addons type; ignoring"),
        }
    }
    Ok(a)
}

/// Unlimited plan used when there is no `plans` table to consult (DB-less
/// in-memory test/dev fixtures). Every cap is `i32::MAX`.
pub(crate) fn unlimited_plan() -> Plan {
    let now = chrono::Utc::now();
    Plan {
        id: "self-host".into(),
        name: "Self-host".into(),
        description: "Unlimited (no plans table)".into(),
        max_targets: i32::MAX,
        min_check_interval_secs: 1,
        retention_days: i32::MAX,
        raw_days: i32::MAX,
        evidence_days: i32::MAX,
        max_members: i32::MAX,
        max_pending_invitations: i32::MAX,
        max_api_tokens_per_user: i32::MAX,
        max_public_components: i32::MAX,
        max_status_pages: i32::MAX,
        max_share_links_per_monitor: i32::MAX,
        max_shared_monitors: i32::MAX,
        max_maintenance_windows: i32::MAX,
        max_notification_channels: i32::MAX,
        max_escalation_policies: i32::MAX,
        max_on_call_schedules: i32::MAX,
        max_logo_size_bytes: i32::MAX,
        max_regions: i32::MAX,
        max_orgs: i32::MAX,
        api_writes_per_minute: i32::MAX,
        api_reads_per_minute: i32::MAX,
        bulk_ops_per_minute: i32::MAX,
        test_now_per_minute: i32::MAX,
        check_now_per_minute: i32::MAX,
        custom_domain_enabled: false,
        white_label_enabled: false,
        sms_alerts_enabled: true,
        incident_narration_enabled: true,
        on_call_enabled: true,
        // A flow needs an engine behind it, so grant no cap the fixture cannot run.
        max_flow_checks: 0,
        max_flow_steps: 30,
        is_listed: false,
        created_at: now,
        updated_at: now,
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;

    #[test]
    fn unknown_keys_are_ignored_and_new_caps_apply() {
        // Regression: deny_unknown_fields made any override naming a not-yet-
        // listed cap (max_regions/...) fail to parse, dropping the WHOLE
        // override and silently reverting the org to plan defaults.
        let json = serde_json::json!({
            "max_regions": 7,
            "max_escalation_policies": 3,
            "totally_unknown_future_key": 99,
        });
        let ov: PlanOverrides =
            serde_json::from_value(json).expect("unknown keys must be ignored, not rejected");
        assert_eq!(ov.max_regions, Some(7));
        assert_eq!(ov.max_escalation_policies, Some(3));

        let base = unlimited_plan();
        let merged = apply_overrides(&base, &ov);
        assert_eq!(merged.max_regions, 7);
        assert_eq!(merged.max_escalation_policies, 3);
        assert_eq!(
            merged.max_targets, base.max_targets,
            "untouched cap unchanged"
        );
    }
}
