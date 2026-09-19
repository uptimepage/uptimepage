//! Cross-tenant data access. The only escape hatch out of the org-scoped
//! repositories. Every constructor records a static `reason` so an audit grep
//! over the codebase enumerates every cross-tenant entry point.
//!
//! Rules (also enforced by `scripts/sg-rules/admin_repo_static_reason.yml`):
//!  * `AdminRepo::new` requires `&'static str` — dynamic strings would defeat
//!    the audit, since a future caller could format an attacker-controlled
//!    value into the reason field.
//!  * Methods on this type intentionally do **not** filter by `org_id`. The
//!    type itself is the audit signal; readers see `AdminRepo` and know the
//!    query crosses tenant boundaries on purpose.
//!  * No method here returns per-org user-facing data. Only aggregates and
//!    background-job inputs (scheduler enumeration, purge queue, etc.).
//!
//! Audit caveat: the grep `rg 'AdminRepo::new\('` enumerates direct call
//! sites only. A `use AdminRepo as Repo` (or trait-impl rename via macros)
//! would dodge it. Code review must reject such renames; the ast-grep rule
//! has the same blind spot.

use anyhow::Context;
use async_trait::async_trait;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::domain::{OrgId, Target};
use crate::error::Result;
use crate::quotas::effective::RegionCaps;
use crate::security::Cipher;
use crate::storage::postgres::{TargetRow, decode_target_row};

/// Source of every enabled target across every organisation, for the
/// scheduler's global registry. Each target is paired with its owning
/// [`OrgId`] so the scheduler→worker→alert path can resolve channels
/// tenant-scoped (a target only ever reaches its own org's channels).
/// Implemented by [`AdminRepo`] (production) and by
/// [`crate::storage::InMemoryTargetStore`] (single-org test fixtures).
#[async_trait]
pub trait EnabledTargetSource: Send + Sync {
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>>;
}

#[async_trait]
impl EnabledTargetSource for AdminRepo {
    async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        AdminRepo::list_all_enabled_targets(self).await
    }
}

/// Keyset cursor over `(org_id, target_id)` ascending. `None` means "start
/// from the beginning"; subsequent pages pass the last row's pair to skip
/// past it on the next read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicTargetCursor {
    pub org_id: OrgId,
    pub target_id: Uuid,
}

impl PublicTargetCursor {
    pub fn after(org_id: OrgId, target_id: Uuid) -> Self {
        Self { org_id, target_id }
    }
}

/// Paginated cross-tenant walk over every enabled target in *live*
/// organisations — the set the incident writer watches. Separate from
/// [`EnabledTargetSource`] because the access pattern differs: the scheduler
/// builds a single in-memory registry (full snapshot, infrequent), whereas the
/// incident writer streams every 30 s and must keep page-sized memory +
/// bounded SQL load at 10k+ orgs. Whether a target is also a public status-page
/// component is decided at incident-insert time, not here.
#[async_trait]
pub trait EnabledTargetStream: Send + Sync {
    /// Next page of `(org_id, target)` strictly after `after`, up to `limit`
    /// rows, ordered by `(org_id, target_id)` ascending. An empty result
    /// signals the walk is done.
    async fn next_enabled_target_page(
        &self,
        after: Option<PublicTargetCursor>,
        limit: usize,
    ) -> Result<Vec<(OrgId, Target)>>;
}

#[async_trait]
impl EnabledTargetStream for AdminRepo {
    async fn next_enabled_target_page(
        &self,
        after: Option<PublicTargetCursor>,
        limit: usize,
    ) -> Result<Vec<(OrgId, Target)>> {
        AdminRepo::next_enabled_target_page(self, after, limit).await
    }
}

/// `targets` row plus its `org_id`. Reuses [`TargetRow`] via `#[sqlx(flatten)]`
/// so the column list stays single-sourced with the org-scoped store.
#[derive(sqlx::FromRow)]
struct OrgTargetRow {
    org_id: Uuid,
    #[sqlx(flatten)]
    target: TargetRow,
}

type RegionPullEtagRow = (
    i64,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<String>,
    Option<String>,
);

/// Single source for the cross-tenant target column list. Both the
/// scheduler-snapshot and incident-writer-keyset queries return the same
/// `targets` shape that [`decode_target_row`] consumes.
const TARGET_COLUMNS: &str = "t.org_id, t.id, t.name, t.check_spec, t.interval_secs, t.enabled, t.tags, t.alerts, t.region_policy, t.alert_confirmations, t.notify_recovery, t.renotify_interval_secs, t.group_name, t.owner_user_id, t.write_source, t.created_at, t.updated_at, t.plan_hold_at";

/// The per-org region ceiling, joined in as `cap`. `$2` carries the org ids and
/// `$3` their limits; an org missing from the arrays has no ceiling. Written
/// against a `targets` aliased `t`.
pub(crate) const REGION_CAP_JOIN: &str = "LEFT JOIN unnest($2::uuid[], $3::int[]) AS cap(org_id, max_regions) \
     ON cap.org_id = t.org_id";

/// Whether this region is one of the monitor's first `max_regions`: rank its
/// enabled regions default-on first and then by id, and keep the row while that
/// rank is within the cap. Written against the aliases `t`, `tr`, `rg` and
/// [`REGION_CAP_JOIN`]. Every hand-out shares it — the pull, the etag that
/// validates it, and silence detection — so a rank that moved (a plan retuned,
/// another region enabled or disabled, a default flipped) reaches all three.
///
/// The middle test is what keeps the cheap paths cheap: a monitor can never be
/// assigned more regions than exist, so a ceiling at or above the enabled-region
/// count cannot bind. It is uncorrelated, so Postgres runs it once per query and
/// short-circuits the per-row rank for every plan that sells unlimited regions —
/// which, outside `free`, is all of them.
pub(crate) const REGION_CAP_PREDICATE: &str = "(cap.max_regions IS NULL \
     OR cap.max_regions >= (SELECT count(*) FROM regions WHERE enabled) \
     OR cap.max_regions >= ( \
     SELECT count(*) FROM target_regions tr2 \
     JOIN regions rg2 ON rg2.id = tr2.region \
     WHERE tr2.target_id = t.id AND rg2.enabled \
       AND (rg2.default_selected > rg.default_selected \
            OR (rg2.default_selected = rg.default_selected \
                AND tr2.region <= tr.region))))";

/// Whether the plan still covers this monitor. An account over its cap keeps
/// every row it has; the excess carries a `plan_hold_at` and stops being served
/// until the plan grows back. Written against a `targets` aliased `t`; use
/// [`not_held_sql`] where the alias differs or the query reaches `targets`
/// through a target id.
///
/// Shared by every hand-out for the same reason [`REGION_CAP_PREDICATE`] is: a
/// hold has to reach the pull, the etag that validates it, the incident writer
/// and silence detection together, or an agent keeps probing a monitor the
/// account no longer holds and the no-data alarm fires for the one it does.
pub(crate) const NOT_HELD_PREDICATE: &str = "t.plan_hold_at IS NULL";

/// [`NOT_HELD_PREDICATE`] for a query whose `targets` is aliased something
/// other than `t`, or which names a target only by id.
pub(crate) fn not_held_sql(target_col: &str) -> String {
    format!("(SELECT th.plan_hold_at FROM targets th WHERE th.id = {target_col}) IS NULL")
}

/// Per-row lenient decode: skip (logged + counted) any row whose `check_spec`,
/// `alerts`, or credential decrypt won't parse, rather than failing the whole
/// batch — one bad row (a shape predating a schema change, a credential that
/// won't decrypt) must never blind the scheduler to every other target.
///
/// No all-failed guard here: a page of all-bad rows is not necessarily systemic
/// (bad rows cluster by the `(org_id, id)` keyset), so the paginator skips and
/// advances rather than aborting the walk. The snapshot reads layer the guard
/// on top via [`decode_targets_or_err`].
fn decode_targets_skipping(
    rows: Vec<OrgTargetRow>,
    cipher: Option<&Cipher>,
) -> Vec<(OrgId, Target)> {
    rows.into_iter()
        .filter_map(|r| {
            let org = OrgId(r.org_id);
            let target_id = r.target.id;
            match decode_target_row(r.target, cipher) {
                Ok(t) => Some((org, t)),
                Err(err) => {
                    tracing::error!(
                        %target_id,
                        org_id = %org.0,
                        error = ?err,
                        "skipping target that failed to decode (check_spec/alerts/credentials)"
                    );
                    metrics::counter!("uptimepage_undecodable_targets_total").increment(1);
                    None
                }
            }
        })
        .collect()
}

/// Snapshot decode for the scheduler's full lists. Skips one bad row, but if
/// *every* row fails it's a systemic fault (wrong KEK, schema drift): an empty
/// result here is dangerous — callers read it as "no targets" and drop the
/// whole fleet — so all-failed returns `Err`, keeping prior state and surfacing
/// loudly. A genuinely empty input returns `Ok(empty)` as normal.
fn decode_targets_or_err(
    rows: Vec<OrgTargetRow>,
    cipher: Option<&Cipher>,
) -> Result<Vec<(OrgId, Target)>> {
    let total = rows.len();
    let decoded = decode_targets_skipping(rows, cipher);
    if decoded.is_empty() && total > 0 {
        return Err(anyhow::anyhow!(
            "all {total} cross-tenant targets failed to decode — likely a cipher/KEK or schema fault"
        )
        .into());
    }
    Ok(decoded)
}

pub struct AdminRepo {
    pool: PgPool,
    cipher: Option<Arc<Cipher>>,
    reason: &'static str,
}

impl AdminRepo {
    /// `reason` is recorded for audit. Use a short, lower-case identifier
    /// pinned to the call site (e.g. `"scheduler_refresh"`,
    /// `"purge_worker"`). The `&'static str` bound prevents dynamic strings
    /// that would defeat a CI grep.
    pub fn new(pool: PgPool, cipher: Option<Arc<Cipher>>, reason: &'static str) -> Self {
        tracing::debug!(reason, "AdminRepo constructed");
        Self {
            pool,
            cipher,
            reason,
        }
    }

    pub fn reason(&self) -> &'static str {
        self.reason
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// All enabled targets across every *live* organisation. The
    /// `organizations.deleted_at IS NULL` filter is load-bearing: a
    /// soft-deleted org has signalled "stop monitoring me" and the 30-day
    /// recovery grace window must not double as a 30-day stream of
    /// post-deletion check writes, alert deliveries, and ClickHouse rows
    /// the cascade purge then has to clear.
    pub async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        let sql = format!(
            "SELECT {TARGET_COLUMNS} \
             FROM targets t JOIN organizations o ON o.id = t.org_id \
             WHERE t.enabled = true AND o.deleted_at IS NULL AND {NOT_HELD_PREDICATE}"
        );
        let rows: Vec<OrgTargetRow> = sqlx::query_as::<_, OrgTargetRow>(&sql)
            .fetch_all(&self.pool)
            .await
            .context("admin: list all enabled targets")?;
        decode_targets_or_err(rows, self.cipher.as_deref())
    }

    /// Boot reconciliation for the config-driven region model. Idempotent.
    /// Upserts the control plane's own region and the new-target default region
    /// so their FK targets exist, then assigns every still-unassigned enabled
    /// target to the default region — so no target is ever orphaned between the
    /// region tables existing and the create path writing assignments.
    pub async fn reconcile_regions(
        &self,
        scheduler_region: &str,
        default_region: &str,
    ) -> Result<()> {
        for id in [scheduler_region, default_region] {
            sqlx::query(
                "INSERT INTO regions (id, name, city) VALUES ($1, $1, '') \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(id)
            .execute(&self.pool)
            .await
            .context("admin: reconcile_regions upsert region")?;
        }
        sqlx::query(
            "INSERT INTO target_regions (target_id, region) \
             SELECT t.id, $1 FROM targets t \
             WHERE NOT EXISTS (SELECT 1 FROM target_regions tr WHERE tr.target_id = t.id) \
             ON CONFLICT DO NOTHING",
        )
        .bind(default_region)
        .execute(&self.pool)
        .await
        .context("admin: reconcile_regions backfill assignments")?;
        Ok(())
    }

    /// Every enabled heartbeat-kind target in a live org that has been pinged at
    /// least once. Passive checks run only on the control plane (state is this
    /// process's memory), so this list is region-independent and excluded from
    /// the agent-facing region queries.
    ///
    /// The `first_ping_at` join is the pending gate: a job that has never spoken
    /// has no health to report, so it is not evaluated against a window that
    /// started when somebody clicked save.
    pub async fn list_enabled_heartbeat_targets(&self) -> Result<Vec<(OrgId, Target)>> {
        let sql = format!(
            "SELECT {TARGET_COLUMNS} \
             FROM targets t \
             JOIN organizations o ON o.id = t.org_id \
             JOIN heartbeat_monitors hm ON hm.target_id = t.id \
             WHERE t.enabled = true AND o.deleted_at IS NULL AND {NOT_HELD_PREDICATE} \
               AND t.kind = 'heartbeat' \
               AND hm.first_ping_at IS NOT NULL"
        );
        let rows: Vec<OrgTargetRow> = sqlx::query_as::<_, OrgTargetRow>(&sql)
            .fetch_all(&self.pool)
            .await
            .context("admin: list enabled heartbeat targets")?;
        decode_targets_or_err(rows, self.cipher.as_deref())
    }

    /// Per-refresh reconciliation of heartbeat state. Heals rows lost to a
    /// partial create or org restore (mints a fresh token), then returns every
    /// live monitor's ping state, including disabled targets so a later enable
    /// finds its state. Projection rule:
    /// [`crate::storage::heartbeats::ping_state_of`].
    pub async fn sync_heartbeat_rows(
        &self,
    ) -> Result<Vec<(Uuid, crate::domain::heartbeat::PingState)>> {
        let missing: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT t.id FROM targets t \
             JOIN organizations o ON o.id = t.org_id AND o.deleted_at IS NULL \
             WHERE t.kind = 'heartbeat' \
               AND NOT EXISTS (SELECT 1 FROM heartbeat_monitors hm WHERE hm.target_id = t.id)",
        )
        .fetch_all(&self.pool)
        .await
        .context("admin: heartbeat rows to heal")?;
        for (target_id,) in missing {
            // Lock the target so a delete can't commit between this check and the
            // insert — that race trips the FK or the same-org trigger and would
            // abort every org's reconcile. A target already gone has nothing to heal.
            let mut tx = self
                .pool
                .begin()
                .await
                .context("admin: heal heartbeat tx")?;
            let owner: Option<(Uuid,)> =
                sqlx::query_as("SELECT org_id FROM targets WHERE id = $1 FOR KEY SHARE")
                    .bind(target_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .context("admin: lock heartbeat target")?;
            let Some((org_id,)) = owner else { continue };
            let minted = crate::storage::capability_token::mint(self.cipher.as_deref())?;
            sqlx::query(
                "INSERT INTO heartbeat_monitors (target_id, org_id, token_hash, token_enc) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT (target_id) DO NOTHING",
            )
            .bind(target_id)
            .bind(org_id)
            .bind(&minted.hash)
            .bind(&minted.sealed)
            .execute(&mut *tx)
            .await
            .context("admin: heal heartbeat row")?;
            tx.commit().await.context("admin: commit heartbeat heal")?;
            tracing::warn!(%target_id, "healed missing heartbeat_monitors row");
        }
        type StateRow = (
            Uuid,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<i16>,
        );
        let rows: Vec<StateRow> = sqlx::query_as(
            "SELECT hm.target_id, hm.armed_at, hm.last_ping_at, hm.last_start_at, \
                        hm.last_fail_at, hm.last_exit_code \
                 FROM heartbeat_monitors hm \
                 JOIN organizations o ON o.id = hm.org_id AND o.deleted_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("admin: list heartbeat state")?;
        Ok(rows
            .into_iter()
            .map(|(id, armed, ping, start, fail, code)| {
                (
                    id,
                    crate::storage::heartbeats::ping_state_of(
                        armed,
                        ping,
                        start,
                        fail,
                        code.map(|c| c as u8),
                    ),
                )
            })
            .collect())
    }

    /// Orgs with an enabled target in this region. The caller resolves their
    /// plans through the quota service, which is what governs the pull.
    pub async fn region_org_ids(&self, region: &str) -> Result<Vec<OrgId>> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT DISTINCT t.org_id FROM target_regions tr \
             JOIN targets t ON t.id = tr.target_id \
             JOIN organizations o ON o.id = t.org_id \
             WHERE tr.region = $1 AND t.enabled = true AND o.deleted_at IS NULL \
               AND t.plan_hold_at IS NULL",
        )
        .bind(region)
        .fetch_all(&self.pool)
        .await
        .context("admin: region org ids")?;
        Ok(rows.into_iter().map(|(id,)| OrgId(id)).collect())
    }

    /// Cheap config-pull validator for one region: a digest over the assigned
    /// targets' ids and a hash of each `check_spec`, plus count + max
    /// `updated_at`. The per-row `check_spec` hash means the etag changes on any
    /// content rewrite — including a credential re-encrypt (KEK rotation) that
    /// leaves `updated_at` untouched — so an agent never serves stale config off
    /// a `304`. Still no decrypt: it hashes the stored (encrypted) ciphertext.
    ///
    /// `caps` and `plan_digest` are supplied by the caller so the etag and the
    /// pull it validates are derived from one resolution of the plan, not two
    /// that can disagree: a stale clamp behind a fresh etag latches once the
    /// agent stores it and never re-pulls. The same cap predicate the pull uses
    /// filters the digest, so a region the cap admits or drops — including one
    /// freed by a *different* region being disabled — moves the etag.
    pub async fn region_pull_etag(
        &self,
        region: &str,
        caps: &RegionCaps,
        plan_digest: &str,
    ) -> Result<String> {
        // The fourth column digests each region org's variables, so a variable
        // edit (which never touches targets.updated_at) still bumps the etag.
        let sql = format!(
            "SELECT count(*)::bigint, max(t.updated_at), \
                        md5(coalesce(string_agg(t.id::text || ':' || md5(t.check_spec::text), \
                                                ',' ORDER BY t.id), '')), \
                        (SELECT md5(coalesce(string_agg( \
                                 v.id::text || ':' || extract(epoch from v.updated_at)::text, \
                                 ',' ORDER BY v.id), '')) \
                         FROM org_variables v \
                         WHERE v.org_id IN ( \
                            SELECT DISTINCT t2.org_id FROM target_regions tr2 \
                            JOIN targets t2 ON t2.id = tr2.target_id \
                            WHERE tr2.region = $1 AND t2.enabled = true \
                              AND t2.plan_hold_at IS NULL)) \
                 FROM target_regions tr \
                 JOIN targets t ON t.id = tr.target_id \
                 JOIN organizations o ON o.id = t.org_id \
                 JOIN regions rg ON rg.id = tr.region \
                 {REGION_CAP_JOIN} \
                 WHERE tr.region = $1 AND t.enabled = true AND o.deleted_at IS NULL \
                   AND {NOT_HELD_PREDICATE} \
                   AND rg.enabled AND t.kind IS DISTINCT FROM 'heartbeat' \
                   AND {REGION_CAP_PREDICATE}"
        );
        let (cap_orgs, cap_limits) = caps.arrays();
        let row: RegionPullEtagRow = sqlx::query_as(&sql)
            .bind(region)
            .bind(cap_orgs)
            .bind(cap_limits)
            .fetch_one(&self.pool)
            .await
            .context("admin: region pull etag")?;
        let (count, max_updated, digest, var_digest) = row;
        let ts = max_updated.map(|d| d.timestamp_millis()).unwrap_or(0);
        Ok(format!(
            "\"{count}-{ts}-{}-{}-{plan_digest}\"",
            digest.unwrap_or_default(),
            var_digest.unwrap_or_default()
        ))
    }

    /// Enabled targets assigned to one region (via `target_regions`), in live
    /// orgs. Backs the agent config-pull API. Same decrypted shape as
    /// [`Self::list_all_enabled_targets`]. Heartbeat monitors are excluded:
    /// their region rows are inert and an agent has no ping state to evaluate
    /// them against.
    ///
    /// `caps` limits how many of a monitor's regions are served: the first
    /// `max_regions` of its enabled ones, defaults first then by id, so a plan
    /// moving under a monitor drops the same regions on every node and the
    /// assignment row survives for when the plan comes back.
    pub async fn list_enabled_targets_for_region(
        &self,
        region: &str,
        include_flow: bool,
        caps: &RegionCaps,
    ) -> Result<Vec<(OrgId, Target)>> {
        // Flow monitors run only on flow-capable nodes; a node that can't run
        // one must never be handed it (it would just error every cycle).
        let sql = format!(
            "SELECT {TARGET_COLUMNS} \
             FROM targets t \
             JOIN organizations o ON o.id = t.org_id \
             JOIN target_regions tr ON tr.target_id = t.id \
             JOIN regions rg ON rg.id = tr.region \
             {REGION_CAP_JOIN} \
             WHERE t.enabled = true AND o.deleted_at IS NULL AND tr.region = $1 \
               AND {NOT_HELD_PREDICATE} \
               AND rg.enabled AND t.kind IS DISTINCT FROM 'heartbeat' \
               AND ($4 OR t.kind IS DISTINCT FROM 'flow') \
               AND {REGION_CAP_PREDICATE}"
        );
        let (cap_orgs, cap_limits) = caps.arrays();
        let rows: Vec<OrgTargetRow> = sqlx::query_as::<_, OrgTargetRow>(&sql)
            .bind(region)
            .bind(cap_orgs)
            .bind(cap_limits)
            .bind(include_flow)
            .fetch_all(&self.pool)
            .await
            .context("admin: list enabled targets for region")?;
        let decoded = decode_targets_or_err(rows, self.cipher.as_deref())?;
        self.resolve_target_variables(decoded).await
    }

    /// Substitute each HTTP target's `{{var}}` references from its org's variables
    /// so the agent receives a ready-to-probe spec and never needs the database.
    /// A target whose variables can't resolve (missing/policy/un-openable secret)
    /// is dropped, so one broken target never blinds the rest of the region.
    async fn resolve_target_variables(
        &self,
        targets: Vec<(OrgId, Target)>,
    ) -> Result<Vec<(OrgId, Target)>> {
        use std::collections::{HashMap, HashSet};

        use crate::domain::interpolate::{
            flow_uses_vars, resolve_flow_spec, resolve_http_spec, uses_vars,
        };
        use crate::domain::{CheckSpec, VarMap};
        use crate::storage::variables::{PgVariableStore, VariableStore};

        fn spec_uses_vars(spec: &CheckSpec) -> bool {
            match spec {
                CheckSpec::Http(h) => uses_vars(h),
                CheckSpec::Flow(f) => flow_uses_vars(f),
                _ => false,
            }
        }

        let orgs: HashSet<OrgId> = targets
            .iter()
            .filter(|(_, t)| spec_uses_vars(&t.check))
            .map(|(o, _)| *o)
            .collect();
        if orgs.is_empty() {
            return Ok(targets);
        }

        // One map error skips that org's var-using targets, not the whole pull.
        let store = PgVariableStore::new(self.pool.clone(), self.cipher.clone());
        let mut maps: HashMap<OrgId, VarMap> = HashMap::with_capacity(orgs.len());
        for org in orgs {
            match store.resolve_map(org).await {
                Ok(map) => {
                    maps.insert(org, map);
                }
                Err(err) => {
                    tracing::warn!(org = %org.0, %err, "skipping org variables: resolve_map failed");
                }
            }
        }

        let mut out = Vec::with_capacity(targets.len());
        'target: for (org, mut target) in targets {
            if spec_uses_vars(&target.check) {
                let Some(map) = maps.get(&org) else {
                    tracing::warn!(target_id = %target.id, "dropping target: org variables unavailable");
                    continue;
                };
                let resolved = match &target.check {
                    CheckSpec::Http(http) => resolve_http_spec(http, map).map(CheckSpec::Http),
                    CheckSpec::Flow(flow) => resolve_flow_spec(flow, map).map(CheckSpec::Flow),
                    other => Ok(other.clone()),
                };
                match resolved {
                    Ok(spec) => target.check = spec,
                    Err(err) => {
                        tracing::warn!(target_id = %target.id, %err, "dropping target: variable resolution failed");
                        continue 'target;
                    }
                }
            }
            out.push((org, target));
        }
        Ok(out)
    }

    /// Map of enabled `target_id -> owning org` for one region. Ingest uses it
    /// both to reject results for targets outside the agent's region and to
    /// stamp the authoritative `org_id` (never trusting the agent-supplied one).
    ///
    /// Deliberately blind to the region cap and to a plan hold, matching the
    /// region cap's own reasoning: this is a scope check against cross-region
    /// spoofing, not a quota. Between a hold landing and the agent's next pull
    /// the agent still holds the target and probes it; discarding those results
    /// here would buy nothing (the writer and the silence sweep already skip a
    /// held monitor) and would turn a normal pull delay into an error stream.
    pub async fn assigned_targets_for_region(
        &self,
        region: &str,
    ) -> Result<std::collections::HashMap<Uuid, OrgId>> {
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT tr.target_id, t.org_id \
             FROM target_regions tr \
             JOIN targets t ON t.id = tr.target_id \
             JOIN organizations o ON o.id = t.org_id \
             JOIN regions rg ON rg.id = tr.region \
             WHERE tr.region = $1 AND t.enabled = true AND o.deleted_at IS NULL \
               AND rg.enabled AND t.kind IS DISTINCT FROM 'heartbeat'",
        )
        .bind(region)
        .fetch_all(&self.pool)
        .await
        .context("admin: assigned targets for region")?;
        Ok(rows.into_iter().map(|(t, o)| (t, OrgId(o))).collect())
    }

    /// Keyset-paginated walk over every enabled target in a live org — the set
    /// the incident writer watches. Incidents open for any monitor, not only
    /// public status-page components; whether the resulting incident is
    /// publicly visible is decided when it is inserted. Keyset-ordered by
    /// `(org_id, id)`; `limit` is clamped.
    pub async fn next_enabled_target_page(
        &self,
        after: Option<PublicTargetCursor>,
        limit: usize,
    ) -> Result<Vec<(OrgId, Target)>> {
        const MAX_PAGE: usize = 2_048;
        let limit = limit.clamp(1, MAX_PAGE) as i64;
        let base = format!(
            "SELECT {TARGET_COLUMNS} \
             FROM targets t JOIN organizations o ON o.id = t.org_id \
             WHERE t.enabled = true AND o.deleted_at IS NULL AND {NOT_HELD_PREDICATE}"
        );
        // Advance by the raw last row, not the last *decoded* one: if an entire
        // page decodes to nothing (a clustered run of bad rows), skip past it
        // and keep reading instead of returning empty — empty must mean "no more
        // rows", which is the caller's walk-complete signal.
        let mut cursor = after;
        loop {
            let rows: Vec<OrgTargetRow> = match cursor {
                None => {
                    let sql = format!("{base} ORDER BY t.org_id, t.id LIMIT $1");
                    sqlx::query_as::<_, OrgTargetRow>(&sql)
                        .bind(limit)
                        .fetch_all(&self.pool)
                        .await
                }
                Some(c) => {
                    let sql = format!(
                        "{base} AND (t.org_id, t.id) > ($1, $2) ORDER BY t.org_id, t.id LIMIT $3"
                    );
                    sqlx::query_as::<_, OrgTargetRow>(&sql)
                        .bind(c.org_id.0)
                        .bind(c.target_id)
                        .bind(limit)
                        .fetch_all(&self.pool)
                        .await
                }
            }
            .context("admin: next enabled-target page")?;
            let Some(raw_last) = rows.last() else {
                return Ok(Vec::new()); // no more rows — walk complete
            };
            let next = PublicTargetCursor::after(OrgId(raw_last.org_id), raw_last.target.id);
            let decoded = decode_targets_skipping(rows, self.cipher.as_deref());
            if !decoded.is_empty() {
                return Ok(decoded);
            }
            cursor = Some(next);
        }
    }
}
