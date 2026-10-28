//! Silence (no-data) detection state.
//!
//! A monitor is *silent* when every region that would actually be handed it has
//! lost its probe — no enabled `agents` row for that region with a fresh
//! `last_seen_at`. "Would be handed it" is the same question the agent pull
//! answers: an assignment the plan's `max_regions` puts past the cap is probed
//! by nobody, so a live agent there must not count as coverage. Detection is derived from agent liveness, not check results:
//! a failing or timing-out check still reports an `Error` and opens a normal
//! incident, so true silence means the probe stopped running (overwhelmingly,
//! the agent/region died). `agents.last_seen_at` is real liveness — it is bumped
//! on the agent's authenticated config pull, independent of whether any check
//! produced a result.
//!
//! This store runs that detection query and records the open/resolved boundary
//! in `monitor_silence_state` so a silence is announced once and a recovery
//! once. Healthy monitors never get a row.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use std::collections::HashMap;

use crate::domain::quota::RegionCaps;
use crate::domain::{OrgId, TargetAlerts};
use crate::error::Result;
use crate::storage::admin::{
    NOT_HELD_PREDICATE, PROBED_KIND_PREDICATE, REGION_CAP_JOIN, REGION_CAP_PREDICATE,
};

/// An open (unresolved) silence row. `notified` = the customer was already told.
#[derive(Debug, Clone, Copy)]
pub struct OpenSilence {
    pub org: OrgId,
    pub target_id: Uuid,
    pub notified: bool,
    pub silent_since: DateTime<Utc>,
}

/// What resolving a silence did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// Already resolved, by another instance or an earlier tick.
    Already,
    /// Resolved; its own resumed notice is owed when it was told it stopped.
    Alone,
    /// Resolved inside a group silence, which says it once for every member.
    InGroup,
}

/// The group a silent monitor's notices go through: its own, while that group
/// alerts as one.
#[derive(Debug, Clone, PartialEq)]
pub struct SilenceGroup {
    pub id: Uuid,
    pub name: String,
    pub alerts: TargetAlerts,
    pub tags: Vec<String>,
}

/// A group silence episode whose notice is owed: the opening one, or with
/// every member back (`finished`) the resumed one.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSilence {
    pub id: Uuid,
    pub org: OrgId,
    pub group_name: String,
    /// Where its opening notice went, resolved once when it opened.
    pub channel_ids: Vec<Uuid>,
    /// Every member it has held, once each, in the order they went silent.
    pub members: Vec<String>,
    pub notified: bool,
    pub finished: bool,
    /// A member came back, as opposed to every one of them being deleted.
    pub resumed: bool,
}

fn first_seen(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

#[async_trait]
pub trait SilenceStore: Send + Sync {
    /// Enabled, assigned targets whose every *served* region lacks a fresh,
    /// enabled agent (`last_seen_at` within `stale_after_secs`) and that no
    /// active alert-silencing maintenance window covers. The unmonitored set.
    /// `caps` is the same per-org region ceiling the pull applies, so a region
    /// the plan has dropped cannot pass for coverage.
    async fn unmonitored(
        &self,
        stale_after_secs: u64,
        caps: &RegionCaps,
    ) -> Result<Vec<(OrgId, Uuid)>>;
    /// Orgs owning at least one enabled, region-assigned target — the set whose
    /// plans the sweep resolves to build `caps`.
    async fn orgs_with_assigned_targets(&self) -> Result<Vec<OrgId>>;
    /// Open silences (`resolved_at IS NULL`), with whether each was notified.
    async fn list_open(&self) -> Result<Vec<OpenSilence>>;
    /// Target ids currently silent for one org — for the grey "no data" overlay
    /// on the dashboard and status page.
    async fn open_target_ids(&self, org: OrgId) -> Result<Vec<Uuid>>;
    /// Region ids with a fresh, enabled agent (probe alive). Powers the
    /// per-region "no data" mark on the monitor detail breakdown table.
    async fn live_regions(&self, stale_after_secs: u64) -> Result<Vec<String>>;
    /// Total enabled targets in live orgs — denominator for mass-outage damping.
    async fn enabled_target_count(&self) -> Result<i64>;
    /// Record a target as silent. Idempotent: an already-open silence keeps its
    /// `silent_since`; re-entering a previously-resolved one starts a fresh
    /// episode (new `silent_since`, cleared `notified_at`/`resolved_at`).
    async fn enter(&self, org: OrgId, target_id: Uuid, at: DateTime<Utc>) -> Result<()>;
    /// Stamp the customer-notified time on an open silence. Idempotent.
    async fn mark_notified(&self, org: OrgId, target_id: Uuid, at: DateTime<Utc>) -> Result<()>;
    /// Mark an open silence resolved (monitoring resumed), and its open group
    /// silence membership with it, in one statement. Says whether this call
    /// flipped it, so only the winner of a 2-instance race notifies.
    async fn resolve(&self, org: OrgId, target_id: Uuid, at: DateTime<Utc>) -> Result<Resolved>;
    /// For each of these monitors in a group that alerts as one, that group.
    async fn alerting_groups(
        &self,
        monitors: &[(OrgId, Uuid)],
    ) -> Result<HashMap<(OrgId, Uuid), SilenceGroup>>;
    /// Puts these silent members into the group's open silence episode,
    /// opening one that notifies `channel_ids` when there is none. A monitor
    /// already in an open episode stays where it is.
    async fn join_group_silence(
        &self,
        org: OrgId,
        group: &SilenceGroup,
        channel_ids: &[Uuid],
        members: &[(Uuid, DateTime<Utc>)],
    ) -> Result<Uuid>;
    /// Open episodes owing a notice. A member whose monitor was deleted counts
    /// as back.
    async fn group_silences_due(&self) -> Result<Vec<GroupSilence>>;
    async fn mark_group_silence_notified(
        &self,
        org: OrgId,
        id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<()>;
    /// Ends an episode with every member back; `told` when its resumed notice
    /// went out.
    async fn end_group_silence(
        &self,
        org: OrgId,
        id: Uuid,
        at: DateTime<Utc>,
        told: bool,
    ) -> Result<()>;
}

// ── PostgreSQL implementation ────────────────────────────────────────────────

pub struct PgSilenceStore {
    pool: PgPool,
}

/// A member of episode `e` still silent: not back, and its monitor not
/// deleted, which counts as back.
const STILL_SILENT: &str = "EXISTS (SELECT 1 FROM group_silence_members m \
     WHERE m.episode_id = e.id AND m.org_id = e.org_id \
       AND m.resumed_at IS NULL AND m.target_id IS NOT NULL)";

impl PgSilenceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SilenceStore for PgSilenceStore {
    async fn unmonitored(
        &self,
        stale_after_secs: u64,
        caps: &RegionCaps,
    ) -> Result<Vec<(OrgId, Uuid)>> {
        let sql = format!(
            r#"SELECT t.org_id, t.id
               FROM targets t
               JOIN organizations o ON o.id = t.org_id
               {REGION_CAP_JOIN}
               WHERE t.enabled AND o.deleted_at IS NULL AND {NOT_HELD_PREDICATE}
                 AND {PROBED_KIND_PREDICATE}
                 AND EXISTS (SELECT 1 FROM target_regions tr WHERE tr.target_id = t.id)
                 AND NOT EXISTS (
                     SELECT 1 FROM target_regions tr
                     JOIN regions rg ON rg.id = tr.region AND rg.enabled
                     JOIN agents a   ON a.region = tr.region AND a.enabled
                                    AND a.last_seen_at > now() - ($1::bigint * interval '1 second')
                     WHERE tr.target_id = t.id AND {REGION_CAP_PREDICATE}
                 )
                 AND NOT {window}"#,
            window = crate::storage::suppressing_window_sql("t.id", "t.org_id"),
        );
        let (cap_orgs, cap_limits) = caps.arrays();
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(&sql)
            .bind(stale_after_secs as i64)
            .bind(cap_orgs)
            .bind(cap_limits)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("silence unmonitored: {e}"))?;
        Ok(rows.into_iter().map(|(o, t)| (OrgId(o), t)).collect())
    }

    async fn orgs_with_assigned_targets(&self) -> Result<Vec<OrgId>> {
        let sql = format!(
            "SELECT DISTINCT t.org_id \
             FROM targets t \
             JOIN organizations o ON o.id = t.org_id \
             WHERE t.enabled AND o.deleted_at IS NULL AND {NOT_HELD_PREDICATE} \
               AND {PROBED_KIND_PREDICATE} \
               AND EXISTS (SELECT 1 FROM target_regions tr WHERE tr.target_id = t.id)"
        );
        let rows: Vec<(Uuid,)> = sqlx::query_as(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("silence orgs: {e}"))?;
        Ok(rows.into_iter().map(|(o,)| OrgId(o)).collect())
    }

    async fn list_open(&self) -> Result<Vec<OpenSilence>> {
        let rows: Vec<(Uuid, Uuid, bool, DateTime<Utc>)> = sqlx::query_as(
            "SELECT org_id, target_id, notified_at IS NOT NULL, silent_since \
             FROM monitor_silence_state WHERE resolved_at IS NULL",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence list_open: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|(o, t, notified, silent_since)| OpenSilence {
                org: OrgId(o),
                target_id: t,
                notified,
                silent_since,
            })
            .collect())
    }

    async fn open_target_ids(&self, org: OrgId) -> Result<Vec<Uuid>> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT target_id FROM monitor_silence_state \
             WHERE org_id = $1 AND resolved_at IS NULL",
        )
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence open_target_ids: {e}"))?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    async fn live_regions(&self, stale_after_secs: u64) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"SELECT DISTINCT a.region
               FROM agents a
               JOIN regions rg ON rg.id = a.region AND rg.enabled
               WHERE a.enabled
                 AND a.last_seen_at > now() - ($1::bigint * interval '1 second')"#,
        )
        .bind(stale_after_secs as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence live_regions: {e}"))?;
        Ok(rows.into_iter().map(|(r,)| r).collect())
    }

    async fn enabled_target_count(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM targets t \
             JOIN organizations o ON o.id = t.org_id \
             WHERE t.enabled AND o.deleted_at IS NULL AND t.plan_hold_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence enabled_target_count: {e}"))?;
        Ok(n)
    }

    async fn enter(&self, org: OrgId, target_id: Uuid, at: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO monitor_silence_state (target_id, org_id, silent_since)
               VALUES ($1, $2, $3)
               ON CONFLICT (target_id) DO UPDATE SET
                   silent_since = CASE WHEN monitor_silence_state.resolved_at IS NOT NULL
                                       THEN EXCLUDED.silent_since
                                       ELSE monitor_silence_state.silent_since END,
                   notified_at  = CASE WHEN monitor_silence_state.resolved_at IS NOT NULL
                                       THEN NULL
                                       ELSE monitor_silence_state.notified_at END,
                   resolved_at  = NULL"#,
        )
        .bind(target_id)
        .bind(org.0)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence enter: {e}"))?;
        Ok(())
    }

    async fn mark_notified(&self, org: OrgId, target_id: Uuid, at: DateTime<Utc>) -> Result<()> {
        sqlx::query(
            "UPDATE monitor_silence_state SET notified_at = $3 \
             WHERE target_id = $1 AND org_id = $2 AND resolved_at IS NULL AND notified_at IS NULL",
        )
        .bind(target_id)
        .bind(org.0)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence mark_notified: {e}"))?;
        Ok(())
    }

    async fn resolve(&self, org: OrgId, target_id: Uuid, at: DateTime<Utc>) -> Result<Resolved> {
        let (flipped, grouped): (bool, bool) = sqlx::query_as(
            "WITH resolved AS ( \
                 UPDATE monitor_silence_state SET resolved_at = $3 \
                 WHERE target_id = $1 AND org_id = $2 AND resolved_at IS NULL \
                 RETURNING target_id \
             ), resumed AS ( \
                 UPDATE group_silence_members SET resumed_at = $3 \
                 WHERE org_id = $2 AND resumed_at IS NULL \
                   AND target_id IN (SELECT target_id FROM resolved) \
                 RETURNING id \
             ) \
             SELECT EXISTS (SELECT 1 FROM resolved), EXISTS (SELECT 1 FROM resumed)",
        )
        .bind(target_id)
        .bind(org.0)
        .bind(at)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence resolve: {e}"))?;
        Ok(match (flipped, grouped) {
            (false, _) => Resolved::Already,
            (true, false) => Resolved::Alone,
            (true, true) => Resolved::InGroup,
        })
    }

    async fn alerting_groups(
        &self,
        monitors: &[(OrgId, Uuid)],
    ) -> Result<HashMap<(OrgId, Uuid), SilenceGroup>> {
        let (orgs, targets): (Vec<Uuid>, Vec<Uuid>) =
            monitors.iter().map(|(o, t)| (o.0, *t)).unzip();
        #[derive(sqlx::FromRow)]
        struct Row {
            org_id: Uuid,
            target_id: Uuid,
            id: Uuid,
            name: String,
            alerts: serde_json::Value,
            tags: Vec<String>,
        }
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT t.org_id, t.id AS target_id, g.id, g.name, g.tags, ",
            crate::storage::channel_bindings::group_alerts_column!("g"),
            " FROM unnest($1::uuid[], $2::uuid[]) AS m(org_id, target_id) \
             JOIN targets t ON t.id = m.target_id AND t.org_id = m.org_id \
             JOIN monitor_groups g ON g.id = t.group_id AND g.org_id = t.org_id \
             WHERE g.alert_as_one"
        ))
        .bind(&orgs)
        .bind(&targets)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("silence alerting groups: {e}"))?;
        rows.into_iter()
            .map(|r| {
                let alerts = serde_json::from_value(r.alerts)
                    .map_err(|e| anyhow::anyhow!("silence group alerts: {e}"))?;
                Ok((
                    (OrgId(r.org_id), r.target_id),
                    SilenceGroup {
                        id: r.id,
                        name: r.name,
                        alerts,
                        tags: r.tags,
                    },
                ))
            })
            .collect()
    }

    async fn join_group_silence(
        &self,
        org: OrgId,
        group: &SilenceGroup,
        channel_ids: &[Uuid],
        members: &[(Uuid, DateTime<Utc>)],
    ) -> Result<Uuid> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("join group silence: {e}"))?;
        let mut episode = None;
        // The open episode is joined under its row lock, which an end takes
        // too: a member never lands in an episode that ends past it, and one
        // that ended meanwhile is replaced by a new one.
        for _ in 0..3 {
            let opened: Option<Uuid> = sqlx::query_scalar(
                "INSERT INTO group_silence_episodes (org_id, group_id, group_name, channel_ids) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (group_id) WHERE ended_at IS NULL DO NOTHING RETURNING id",
            )
            .bind(org.0)
            .bind(group.id)
            .bind(&group.name)
            .bind(channel_ids)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| anyhow::anyhow!("open group silence: {e}"))?;
            episode = match opened {
                Some(id) => Some(id),
                None => sqlx::query_scalar(
                    "SELECT id FROM group_silence_episodes \
                     WHERE org_id = $1 AND group_id = $2 AND ended_at IS NULL FOR UPDATE",
                )
                .bind(org.0)
                .bind(group.id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("find group silence: {e}"))?,
            };
            if episode.is_some() {
                break;
            }
        }
        let episode = episode
            .ok_or_else(|| anyhow::anyhow!("join group silence: the episode kept ending"))?;
        let (ids, since): (Vec<Uuid>, Vec<DateTime<Utc>>) = members.iter().copied().unzip();
        sqlx::query(
            "INSERT INTO group_silence_members (episode_id, org_id, target_id, target_name, silent_since) \
             SELECT $1, $2, t.id, t.name, m.since \
             FROM unnest($3::uuid[], $4::timestamptz[]) AS m(target_id, since) \
             JOIN targets t ON t.id = m.target_id AND t.org_id = $2 \
             ON CONFLICT (target_id) WHERE resumed_at IS NULL DO NOTHING",
        )
        .bind(episode)
        .bind(org.0)
        .bind(&ids)
        .bind(&since)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("join group silence members: {e}"))?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("join group silence: {e}"))?;
        Ok(episode)
    }

    async fn group_silences_due(&self) -> Result<Vec<GroupSilence>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            id: Uuid,
            org_id: Uuid,
            group_name: String,
            channel_ids: Vec<Uuid>,
            notified: bool,
            finished: bool,
            resumed: bool,
            members: Vec<String>,
        }
        let rows: Vec<Row> = sqlx::query_as(&format!(
            "SELECT e.id, e.org_id, e.group_name, e.channel_ids, \
                    e.notified_at IS NOT NULL AS notified, s.finished, \
                    EXISTS (SELECT 1 FROM group_silence_members m \
                            WHERE m.episode_id = e.id AND m.org_id = e.org_id \
                              AND m.resumed_at IS NOT NULL) AS resumed, \
                    ARRAY(SELECT m.target_name FROM group_silence_members m \
                          WHERE m.episode_id = e.id AND m.org_id = e.org_id \
                          ORDER BY m.silent_since, m.id) AS members \
             FROM group_silence_episodes e \
             CROSS JOIN LATERAL (SELECT NOT {STILL_SILENT} AS finished) s \
             /* SAFE: cross-org sweep, each row carries its own org */ \
             WHERE e.ended_at IS NULL AND (e.notified_at IS NULL OR s.finished)",
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("group silences due: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| GroupSilence {
                id: r.id,
                org: OrgId(r.org_id),
                group_name: r.group_name,
                channel_ids: r.channel_ids,
                members: first_seen(r.members),
                notified: r.notified,
                finished: r.finished,
                resumed: r.resumed,
            })
            .collect())
    }

    async fn mark_group_silence_notified(
        &self,
        org: OrgId,
        id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE group_silence_episodes SET notified_at = $3 \
             WHERE id = $1 AND org_id = $2 AND notified_at IS NULL",
        )
        .bind(id)
        .bind(org.0)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("mark group silence notified: {e}"))?;
        Ok(())
    }

    async fn end_group_silence(
        &self,
        org: OrgId,
        id: Uuid,
        at: DateTime<Utc>,
        told: bool,
    ) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("end group silence: {e}"))?;
        let open: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM group_silence_episodes \
             WHERE id = $1 AND org_id = $2 AND ended_at IS NULL FOR UPDATE",
        )
        .bind(id)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("lock group silence: {e}"))?;
        if open.is_none() {
            return Ok(());
        }
        // Read after the lock, in a statement of its own, so a member that
        // joined first is seen and keeps the episode open.
        let joined: bool = sqlx::query_scalar(&format!(
            "SELECT {STILL_SILENT} FROM group_silence_episodes e WHERE e.id = $1 AND e.org_id = $2"
        ))
        .bind(id)
        .bind(org.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("group silence members: {e}"))?;
        if joined {
            return Ok(());
        }
        sqlx::query(
            "UPDATE group_silence_episodes \
             SET ended_at = $3, resolved_notified_at = CASE WHEN $4 THEN $3 END \
             WHERE id = $1 AND org_id = $2",
        )
        .bind(id)
        .bind(org.0)
        .bind(at)
        .bind(told)
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("end group silence: {e}"))?;
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("end group silence: {e}"))?;
        Ok(())
    }
}

// ── In-memory implementation (for sweep-logic tests) ─────────────────────────

/// Test double. `unmonitored` is a settable fixture (the real query needs PG);
/// `enter`/`resolve`/`list_open` exercise the sweep's diff logic.
#[derive(Default)]
pub struct InMemorySilenceStore {
    inner: parking_lot::Mutex<InMemorySilenceState>,
}

#[derive(Default)]
struct InMemorySilenceState {
    unmonitored: Vec<(OrgId, Uuid)>,
    total: i64,
    open: std::collections::HashMap<Uuid, (OrgId, bool, DateTime<Utc>)>,
    resolved: Vec<(OrgId, Uuid)>,
    groups: HashMap<(OrgId, Uuid), SilenceGroup>,
    episodes: Vec<MemEpisode>,
}

struct MemEpisode {
    silence: GroupSilence,
    group_id: Uuid,
    ended: bool,
    /// `(target, resumed)` per membership row.
    members: Vec<(Uuid, bool)>,
}

impl InMemorySilenceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_unmonitored(&self, set: Vec<(OrgId, Uuid)>) {
        self.inner.lock().unmonitored = set;
    }

    pub fn set_total(&self, total: i64) {
        self.inner.lock().total = total;
    }

    pub fn set_alerting_group(&self, org: OrgId, target_id: Uuid, group: SilenceGroup) {
        self.inner.lock().groups.insert((org, target_id), group);
    }

    pub fn open_target_ids(&self) -> Vec<Uuid> {
        let mut v: Vec<Uuid> = self.inner.lock().open.keys().copied().collect();
        v.sort();
        v
    }

    pub fn resolved_target_ids(&self) -> Vec<Uuid> {
        let mut v: Vec<Uuid> = self.inner.lock().resolved.iter().map(|(_, t)| *t).collect();
        v.sort();
        v
    }
}

#[async_trait]
impl SilenceStore for InMemorySilenceStore {
    async fn unmonitored(
        &self,
        _stale_after_secs: u64,
        _caps: &RegionCaps,
    ) -> Result<Vec<(OrgId, Uuid)>> {
        Ok(self.inner.lock().unmonitored.clone())
    }

    async fn orgs_with_assigned_targets(&self) -> Result<Vec<OrgId>> {
        Ok(self
            .inner
            .lock()
            .unmonitored
            .iter()
            .map(|(org, _)| *org)
            .collect())
    }

    async fn list_open(&self) -> Result<Vec<OpenSilence>> {
        Ok(self
            .inner
            .lock()
            .open
            .iter()
            .map(|(t, (o, notified, since))| OpenSilence {
                org: *o,
                target_id: *t,
                notified: *notified,
                silent_since: *since,
            })
            .collect())
    }

    async fn open_target_ids(&self, org: OrgId) -> Result<Vec<Uuid>> {
        Ok(self
            .inner
            .lock()
            .open
            .iter()
            .filter(|(_, (o, _, _))| *o == org)
            .map(|(t, _)| *t)
            .collect())
    }

    async fn live_regions(&self, _stale_after_secs: u64) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn enabled_target_count(&self) -> Result<i64> {
        Ok(self.inner.lock().total)
    }

    async fn enter(&self, org: OrgId, target_id: Uuid, at: DateTime<Utc>) -> Result<()> {
        self.inner
            .lock()
            .open
            .entry(target_id)
            .or_insert((org, false, at));
        Ok(())
    }

    async fn mark_notified(&self, _org: OrgId, target_id: Uuid, _at: DateTime<Utc>) -> Result<()> {
        if let Some(v) = self.inner.lock().open.get_mut(&target_id) {
            v.1 = true;
        }
        Ok(())
    }

    async fn resolve(&self, org: OrgId, target_id: Uuid, _at: DateTime<Utc>) -> Result<Resolved> {
        let mut g = self.inner.lock();
        if g.open.remove(&target_id).is_none() {
            return Ok(Resolved::Already);
        }
        g.resolved.push((org, target_id));
        let mut grouped = false;
        for e in g.episodes.iter_mut().filter(|e| !e.ended) {
            for m in e.members.iter_mut().filter(|(t, r)| *t == target_id && !r) {
                m.1 = true;
                grouped = true;
            }
        }
        Ok(if grouped {
            Resolved::InGroup
        } else {
            Resolved::Alone
        })
    }

    async fn alerting_groups(
        &self,
        monitors: &[(OrgId, Uuid)],
    ) -> Result<HashMap<(OrgId, Uuid), SilenceGroup>> {
        let g = self.inner.lock();
        Ok(monitors
            .iter()
            .filter_map(|m| g.groups.get(m).map(|group| (*m, group.clone())))
            .collect())
    }

    async fn join_group_silence(
        &self,
        org: OrgId,
        group: &SilenceGroup,
        channel_ids: &[Uuid],
        members: &[(Uuid, DateTime<Utc>)],
    ) -> Result<Uuid> {
        let mut g = self.inner.lock();
        let in_one: Vec<Uuid> = g
            .episodes
            .iter()
            .filter(|e| !e.ended)
            .flat_map(|e| e.members.iter().filter(|(_, r)| !r).map(|(t, _)| *t))
            .collect();
        let at = match g
            .episodes
            .iter()
            .position(|e| !e.ended && e.group_id == group.id)
        {
            Some(at) => at,
            None => {
                g.episodes.push(MemEpisode {
                    silence: GroupSilence {
                        id: Uuid::now_v7(),
                        org,
                        group_name: group.name.clone(),
                        channel_ids: channel_ids.to_vec(),
                        members: Vec::new(),
                        notified: false,
                        finished: false,
                        resumed: false,
                    },
                    group_id: group.id,
                    ended: false,
                    members: Vec::new(),
                });
                g.episodes.len() - 1
            }
        };
        let episode = &mut g.episodes[at];
        for (target, _) in members.iter().filter(|(t, _)| !in_one.contains(t)) {
            episode.members.push((*target, false));
            episode.silence.members.push(target.to_string());
        }
        Ok(episode.silence.id)
    }

    async fn group_silences_due(&self) -> Result<Vec<GroupSilence>> {
        let g = self.inner.lock();
        Ok(g.episodes
            .iter()
            .filter(|e| !e.ended)
            .map(|e| GroupSilence {
                finished: e.members.iter().all(|(_, r)| *r),
                resumed: e.members.iter().any(|(_, r)| *r),
                members: first_seen(e.silence.members.clone()),
                ..e.silence.clone()
            })
            .filter(|s| !s.notified || s.finished)
            .collect())
    }

    async fn mark_group_silence_notified(
        &self,
        _org: OrgId,
        id: Uuid,
        _at: DateTime<Utc>,
    ) -> Result<()> {
        if let Some(e) = self
            .inner
            .lock()
            .episodes
            .iter_mut()
            .find(|e| e.silence.id == id)
        {
            e.silence.notified = true;
        }
        Ok(())
    }

    async fn end_group_silence(
        &self,
        _org: OrgId,
        id: Uuid,
        _at: DateTime<Utc>,
        _told: bool,
    ) -> Result<()> {
        if let Some(e) = self
            .inner
            .lock()
            .episodes
            .iter_mut()
            .find(|e| e.silence.id == id && e.members.iter().all(|(_, r)| *r))
        {
            e.ended = true;
        }
        Ok(())
    }
}
