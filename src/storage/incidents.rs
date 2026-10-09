//! Storage layer for operator narration of incidents.
//!
//! Separate from `public_status::incident_writer::IncidentStore` (which only
//! exposes open/insert/close to the background materialiser) so the operator
//! surface can read full incident rows and append public update entries
//! without widening the writer trait.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    CheckStatus, Incident, IncidentNarrationUpdate, IncidentSeverity, IncidentStatusPhase,
    NewIncidentUpdate, OrgId, PublicIncidentUpdate,
};
use std::collections::HashMap;

use crate::error::Result;
use crate::storage::TimeRange;

/// One incident with its monitor's name + latest update. Powers the
/// operator dashboard banner (open only) and the MCP incident list (any window).
#[derive(Debug, Clone)]
pub struct IncidentBrief {
    pub id: Uuid,
    /// `None` once the monitor has been deleted; `target_name` still names it.
    pub target_id: Option<Uuid>,
    pub target_name: String,
    pub severity: IncidentSeverity,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub public_title: Option<String>,
    pub latest_update: Option<PublicIncidentUpdate>,
}

impl IncidentBrief {
    /// A brief always has a monitor, so a missing id means it was deleted.
    pub fn monitor_deleted(&self) -> bool {
        self.target_id.is_none()
    }
}

/// One downtime incident's span on a monitor, slim enough to paint a timeline.
/// `origin`, `severity` and `status_at_start` are what an impact is read from.
#[derive(Debug, Clone)]
pub struct IncidentSpan {
    pub target_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub origin: String,
    pub severity: IncidentSeverity,
    pub status_at_start: String,
}

impl IncidentSpan {
    /// The part of `range` this incident covers; `None` when it covers none.
    pub fn clip(&self, range: TimeRange) -> Option<TimeRange> {
        let from = self.started_at.max(range.from);
        let to = self.ended_at.unwrap_or(range.to).min(range.to);
        (from < to).then_some(TimeRange { from, to })
    }
}

/// Which slice of an org's incidents [`IncidentNarrationStore::list_briefs`]
/// returns. `range: None` is unbounded — the open-incident callers don't need a
/// window and shouldn't have to invent one.
#[derive(Debug, Clone)]
pub struct IncidentBriefFilter {
    pub range: Option<TimeRange>,
    pub target_id: Option<Uuid>,
    pub open_only: bool,
    pub oldest_first: bool,
    pub limit: usize,
    pub offset: usize,
}

impl Default for IncidentBriefFilter {
    fn default() -> Self {
        Self {
            range: None,
            target_id: None,
            open_only: true,
            oldest_first: false,
            limit: 50,
            offset: 0,
        }
    }
}

/// Operator-facing incident narration repository. Every method takes the
/// caller's `org` (resolved from `CurrentOrg`) and refuses to touch any other
/// tenant's rows. Compile-time scoping means "forgot to pass the org" is a
/// type error, not a cross-tenant leak.
#[async_trait]
pub trait IncidentNarrationStore: Send + Sync {
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<Incident>>;
    /// Amend an incident: the customer-facing narration plus the operator
    /// fields on the same row (`title`, `severity`, `urgency`). One writer, so
    /// a save cannot half-apply. The returned projection is public, so the
    /// operator fields do not come back.
    async fn patch_narration(
        &self,
        org: OrgId,
        id: Uuid,
        update: IncidentNarrationUpdate,
    ) -> Result<Option<Incident>>;
    async fn append_update(
        &self,
        org: OrgId,
        incident_id: Uuid,
        new: NewIncidentUpdate,
        author: Option<String>,
    ) -> Result<Option<PublicIncidentUpdate>>;
    /// Incidents across every target in `org` matching `filter`, with the
    /// monitor name and latest update. Includes incidents of deleted monitors;
    /// an incident declared without a monitor is absent.
    async fn list_briefs(
        &self,
        org: OrgId,
        filter: IncidentBriefFilter,
    ) -> Result<Vec<IncidentBrief>>;
    /// Count of open incidents (`ended_at IS NULL`) for the nav health pill.
    async fn count_active(&self, org: OrgId) -> Result<u32>;
    /// Open incidents for one monitor. The tab badge counts these rather than
    /// inferring from the last check: a monitor that fails once and recovers
    /// never opens one, so inferring would badge an empty tab.
    async fn count_ongoing_for_target(&self, org: OrgId, target_id: Uuid) -> Result<u32>;
    /// Confirmed incidents for one target overlapping `range`, newest first. An
    /// incident still open at `range.to` is included; `ongoing_only` keeps only those.
    async fn list_for_target(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: TimeRange,
        limit: usize,
        offset: usize,
        ongoing_only: bool,
    ) -> Result<Vec<Incident>>;
    /// Operator-set titles for `ids`; an incident with none is absent. Internal,
    /// so they stay off the public [`Incident`] projection and out of shared views.
    async fn titles(&self, org: OrgId, ids: &[Uuid]) -> Result<HashMap<Uuid, String>>;
    /// Confirmed downtime per target over `range`; targets with no overlap absent.
    async fn confirmed_downtime_by_target(
        &self,
        org: OrgId,
        range: TimeRange,
    ) -> Result<HashMap<Uuid, i64>>;
    /// Incidents of any origin open at some point in `range`.
    async fn count_overlapping(&self, org: OrgId, range: TimeRange) -> Result<u64>;
    /// The incidents [`Self::confirmed_downtime_by_target`] sums, overlapping
    /// `range`, oldest first.
    async fn downtime_spans(&self, org: OrgId, range: TimeRange) -> Result<Vec<IncidentSpan>>;
}

// ── Postgres impl ────────────────────────────────────────────────────────

pub struct PgIncidentNarrationStore {
    pool: PgPool,
}

impl PgIncidentNarrationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct IncidentRow {
    id: Uuid,
    target_id: Option<Uuid>,
    target_name: Option<String>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    severity: String,
    status_at_start: String,
    check_count: i32,
    error_sample: Option<String>,
    counts_as_downtime: bool,
    public_title: Option<String>,
    public_description: Option<String>,
    duration_secs: Option<i32>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    regions_down: Option<Vec<String>>,
    regions_up: Option<Vec<String>>,
}

#[derive(sqlx::FromRow)]
struct UpdateRow {
    posted_at: DateTime<Utc>,
    phase: String,
    message: String,
    generated: bool,
}

async fn load_with_updates(pool: &PgPool, id: Uuid, org_id: Uuid) -> Result<Option<Incident>> {
    let Some(row): Option<IncidentRow> = sqlx::query_as(
        r#"SELECT id, target_id, target_name, started_at, ended_at, severity, status_at_start,
                  check_count, error_sample, counts_as_downtime,
                  public_title, public_description,
                  duration_secs, created_at, updated_at, regions_down, regions_up
           FROM incidents WHERE id = $1 AND org_id = $2"#,
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| anyhow::anyhow!("get incident: {e}"))?
    else {
        return Ok(None);
    };
    let updates: Vec<UpdateRow> = sqlx::query_as(
        r#"SELECT posted_at, phase, message, generated
           FROM incident_updates
           WHERE incident_id = $1 AND org_id = $2
           ORDER BY posted_at ASC"#,
    )
    .bind(id)
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(|e| anyhow::anyhow!("get incident updates: {e}"))?;
    Ok(Some(row_to_incident(row, updates)))
}

fn row_to_incident(row: IncidentRow, updates: Vec<UpdateRow>) -> Incident {
    Incident {
        id: row.id,
        target_id: row.target_id,
        target_name: row.target_name,
        started_at: row.started_at,
        ended_at: row.ended_at,
        status: parse_status(&row.status_at_start),
        duration_secs: row.duration_secs.map(|d| d.max(0) as u64),
        check_count: row.check_count.max(0) as u64,
        error_sample: row.error_sample,
        counts_as_downtime: row.counts_as_downtime,
        severity: IncidentSeverity::from_db_str(&row.severity),
        public_title: row.public_title,
        public_description: row.public_description,
        created_at: Some(row.created_at),
        updated_at: Some(row.updated_at),
        updates: updates
            .into_iter()
            .map(|u| PublicIncidentUpdate {
                posted_at: u.posted_at,
                phase: IncidentStatusPhase::from_db_str(&u.phase),
                message: u.message,
                generated: u.generated,
            })
            .collect(),
        regions_down: row.regions_down.unwrap_or_default(),
        regions_up: row.regions_up.unwrap_or_default(),
    }
}

fn parse_status(s: &str) -> CheckStatus {
    match s {
        "down" => CheckStatus::Down,
        "degraded" => CheckStatus::Degraded,
        "error" => CheckStatus::Error,
        _ => CheckStatus::Down,
    }
}

#[async_trait]
impl IncidentNarrationStore for PgIncidentNarrationStore {
    async fn get(&self, org: OrgId, id: Uuid) -> Result<Option<Incident>> {
        load_with_updates(&self.pool, id, org.0).await
    }

    async fn patch_narration(
        &self,
        org: OrgId,
        id: Uuid,
        update: IncidentNarrationUpdate,
    ) -> Result<Option<Incident>> {
        let severity = update.severity.map(|s| s.as_db_str());
        let urgency = update.urgency.map(|u| u.as_db_str());
        let row: Option<IncidentRow> = sqlx::query_as(
            r#"UPDATE incidents
               SET public_title       = CASE WHEN $2::bool THEN $3 ELSE public_title END,
                   public_description = CASE WHEN $4::bool THEN $5 ELSE public_description END,
                   severity           = COALESCE($6, severity),
                   title              = CASE WHEN $8::bool THEN $9 ELSE title END,
                   urgency            = COALESCE($10, urgency),
                   -- Guards the handler's 422: check rows evidence a monitor incident.
                   counts_as_downtime = CASE
                       WHEN $11::bool AND origin = 'manual' THEN $12
                       ELSE counts_as_downtime END,
                   updated_at         = now()
               WHERE id = $1 AND org_id = $7
               RETURNING id, target_id, target_name, started_at, ended_at, severity, status_at_start,
                         check_count, error_sample, counts_as_downtime,
                         public_title, public_description,
                         duration_secs, created_at, updated_at, regions_down, regions_up"#,
        )
        .bind(id)
        .bind(update.public_title.is_some())
        .bind(update.public_title.clone().flatten())
        .bind(update.public_description.is_some())
        .bind(update.public_description.clone().flatten())
        .bind(severity)
        .bind(org.0)
        .bind(update.title.is_some())
        .bind(update.title.clone().flatten())
        .bind(urgency)
        .bind(update.counts_as_downtime.is_some())
        .bind(update.counts_as_downtime.unwrap_or_default())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("patch_narration: {e}"))?;
        let Some(row) = row else { return Ok(None) };
        let updates: Vec<UpdateRow> = sqlx::query_as(
            r#"SELECT posted_at, phase, message, generated
               FROM incident_updates
               WHERE incident_id = $1 AND org_id = $2
               ORDER BY posted_at ASC"#,
        )
        .bind(id)
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("get incident updates: {e}"))?;
        Ok(Some(row_to_incident(row, updates)))
    }

    async fn append_update(
        &self,
        org: OrgId,
        incident_id: Uuid,
        new: NewIncidentUpdate,
        author: Option<String>,
    ) -> Result<Option<PublicIncidentUpdate>> {
        let phase_db = new.phase.as_db_str();
        // Wrap the INSERT + parent UPDATE in a single transaction so the
        // displayed `updated_at` cannot fall out of sync with the appended
        // entry. CTE alternative would be one round-trip but obscures intent.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| anyhow::anyhow!("begin: {e}"))?;
        // org_id is denormalised onto incident_updates and enforced by the
        // trg_incident_updates_org_match trigger. Filtering by the caller's
        // own org_id here means an attempt to append to another tenant's
        // incident yields a clean no-op instead of touching the parent row.
        let row: Option<(DateTime<Utc>, String, String)> = sqlx::query_as(
            r#"INSERT INTO incident_updates (org_id, incident_id, phase, message, author)
               SELECT i.org_id, $1, $2, $3, $4
               FROM incidents i
               WHERE i.id = $1 AND i.org_id = $5
               RETURNING posted_at, phase, message"#,
        )
        .bind(incident_id)
        .bind(phase_db)
        .bind(&new.message)
        .bind(author)
        .bind(org.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("append_update insert: {e}"))?;
        if row.is_some() {
            sqlx::query(r#"UPDATE incidents SET updated_at = now() WHERE id = $1 AND org_id = $2"#)
                .bind(incident_id)
                .bind(org.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| anyhow::anyhow!("append_update bump parent: {e}"))?;
        }
        tx.commit()
            .await
            .map_err(|e| anyhow::anyhow!("commit: {e}"))?;
        Ok(row.map(|(posted_at, phase, message)| PublicIncidentUpdate {
            posted_at,
            phase: IncidentStatusPhase::from_db_str(&phase),
            message,
            generated: false,
        }))
    }

    async fn list_briefs(
        &self,
        org: OrgId,
        filter: IncidentBriefFilter,
    ) -> Result<Vec<IncidentBrief>> {
        // LATERAL picks the latest update per incident in one round-trip. The
        // ceiling bounds a runaway query; callers that paginate (MCP) pass a
        // limit one page past what they'll surface so "is there more?" is
        // answerable without a second count.
        let cap = filter.limit.clamp(1, 1000) as i64;
        // An offset past i64 is a page that cannot exist; Postgres rejects a
        // negative one outright, so saturate into an empty page instead.
        let skip = i64::try_from(filter.offset).unwrap_or(i64::MAX);
        // Fixed strings chosen by a closed match — never caller text.
        let order = if filter.oldest_first {
            "i.started_at ASC, i.id ASC"
        } else {
            "i.started_at DESC, i.id DESC"
        };
        // Literal rather than a parameter so the open-incident partial index is
        // usable; a parameterised `NOT $n OR …` can never match it.
        let open_clause = if filter.open_only {
            "AND i.ended_at IS NULL"
        } else {
            ""
        };
        let sql = format!(
            r#"SELECT i.id, i.target_id, i.target_name,
                      i.severity, i.started_at, i.ended_at, i.public_title,
                      u.posted_at AS update_posted_at,
                      u.phase     AS update_phase,
                      u.message   AS update_message,
                      u.generated AS update_generated
               FROM incidents i
               LEFT JOIN LATERAL (
                   SELECT posted_at, phase, message, generated
                   FROM incident_updates iu
                   WHERE iu.incident_id = i.id AND iu.org_id = i.org_id
                   ORDER BY iu.posted_at DESC
                   LIMIT 1
               ) u ON true
               WHERE i.org_id = $1
                 AND i.target_name IS NOT NULL
                 AND ($2::uuid IS NULL OR i.target_id = $2)
                 {open_clause}
                 AND ($3::timestamptz IS NULL OR i.started_at < $4)
                 AND ($3::timestamptz IS NULL
                      OR i.ended_at IS NULL OR i.ended_at >= $3)
               ORDER BY {order}
               LIMIT $5 OFFSET $6"#
        );
        let rows: Vec<IncidentBriefRow> = sqlx::query_as(&sql)
            .bind(org.0)
            .bind(filter.target_id)
            .bind(filter.range.map(|r| r.from))
            .bind(filter.range.map(|r| r.to))
            .bind(cap)
            .bind(skip)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("list_briefs incidents: {e}"))?;
        Ok(rows.into_iter().map(row_to_brief).collect())
    }

    async fn count_active(&self, org: OrgId) -> Result<u32> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM incidents WHERE org_id = $1 AND ended_at IS NULL",
        )
        .bind(org.0)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("count_active incidents: {e}"))?;
        Ok(n.max(0) as u32)
    }

    async fn count_ongoing_for_target(&self, org: OrgId, target_id: Uuid) -> Result<u32> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM incidents \
             WHERE org_id = $1 AND target_id = $2 AND ended_at IS NULL",
        )
        .bind(org.0)
        .bind(target_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("count_ongoing_for_target incidents: {e}"))?;
        Ok(n.max(0) as u32)
    }

    async fn list_for_target(
        &self,
        org: OrgId,
        target_id: Uuid,
        range: TimeRange,
        limit: usize,
        offset: usize,
        ongoing_only: bool,
    ) -> Result<Vec<Incident>> {
        // Backstop only; callers pass their own page size (the API peeks one past
        // its limit, so the ceiling must clear the largest caller limit + 1).
        let cap = limit.clamp(1, 5000) as i64;
        let skip = offset as i64;
        let rows: Vec<IncidentRow> = sqlx::query_as(
            r#"SELECT id, target_id, target_name, started_at, ended_at, severity, status_at_start,
                      check_count, error_sample, counts_as_downtime,
                      public_title, public_description,
                      duration_secs, created_at, updated_at, regions_down, regions_up
               FROM incidents
               WHERE org_id = $1 AND target_id = $2
                 AND started_at < $4
                 AND (ended_at IS NULL OR ended_at >= $3)
                 AND ($5 = false OR ended_at IS NULL)
               ORDER BY started_at DESC, id DESC
               LIMIT $6 OFFSET $7"#,
        )
        .bind(org.0)
        .bind(target_id)
        .bind(range.from)
        .bind(range.to)
        .bind(ongoing_only)
        .bind(cap)
        .bind(skip)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("list_for_target incidents: {e}"))?;
        // History rows don't render the update timeline; skip the per-row join.
        Ok(rows
            .into_iter()
            .map(|r| row_to_incident(r, Vec::new()))
            .collect())
    }

    async fn titles(&self, org: OrgId, ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, title FROM incidents WHERE org_id = $1 AND id = ANY($2) AND title <> ''",
        )
        .bind(org.0)
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("incident titles: {e}"))?;
        Ok(rows.into_iter().collect())
    }

    async fn confirmed_downtime_by_target(
        &self,
        org: OrgId,
        range: TimeRange,
    ) -> Result<HashMap<Uuid, i64>> {
        // FLOOR per incident to match the detail page's per-incident truncation.
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            r#"SELECT target_id,
                      SUM(GREATEST(0, FLOOR(EXTRACT(EPOCH FROM
                          (LEAST(COALESCE(ended_at, $2), $2) - GREATEST(started_at, $1))))))::bigint
               FROM incidents
               WHERE org_id = $3 AND target_id IS NOT NULL AND started_at < $2
                 AND (ended_at IS NULL OR ended_at >= $1)
                 AND counts_as_downtime
               GROUP BY target_id"#,
        )
        .bind(range.from)
        .bind(range.to)
        .bind(org.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("confirmed_downtime_by_target: {e}"))?;
        Ok(rows.into_iter().collect())
    }

    async fn count_overlapping(&self, org: OrgId, range: TimeRange) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM incidents \
             WHERE org_id = $1 AND started_at < $3 AND (ended_at IS NULL OR ended_at >= $2)",
        )
        .bind(org.0)
        .bind(range.from)
        .bind(range.to)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("count_overlapping incidents: {e}"))?;
        Ok(n.max(0) as u64)
    }

    async fn downtime_spans(&self, org: OrgId, range: TimeRange) -> Result<Vec<IncidentSpan>> {
        let rows: Vec<IncidentSpanRow> = sqlx::query_as(
            r#"SELECT target_id, started_at, ended_at, origin, severity, status_at_start
               FROM incidents
               WHERE org_id = $1 AND target_id IS NOT NULL AND started_at < $3
                 AND (ended_at IS NULL OR ended_at >= $2)
                 AND counts_as_downtime
               ORDER BY started_at"#,
        )
        .bind(org.0)
        .bind(range.from)
        .bind(range.to)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("downtime_spans: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|r| IncidentSpan {
                target_id: r.target_id,
                started_at: r.started_at,
                ended_at: r.ended_at,
                origin: r.origin,
                severity: IncidentSeverity::from_db_str(&r.severity),
                status_at_start: r.status_at_start,
            })
            .collect())
    }
}

#[derive(sqlx::FromRow)]
struct IncidentSpanRow {
    target_id: Uuid,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    origin: String,
    severity: String,
    status_at_start: String,
}

#[derive(sqlx::FromRow)]
struct IncidentBriefRow {
    id: Uuid,
    target_id: Option<Uuid>,
    target_name: String,
    severity: String,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    public_title: Option<String>,
    update_posted_at: Option<DateTime<Utc>>,
    update_phase: Option<String>,
    update_message: Option<String>,
    update_generated: Option<bool>,
}

fn row_to_brief(r: IncidentBriefRow) -> IncidentBrief {
    let latest_update = match (r.update_posted_at, r.update_phase, r.update_message) {
        (Some(posted_at), Some(phase), Some(message)) => Some(PublicIncidentUpdate {
            posted_at,
            phase: IncidentStatusPhase::from_db_str(&phase),
            message,
            generated: r.update_generated.unwrap_or_default(),
        }),
        _ => None,
    };
    IncidentBrief {
        id: r.id,
        target_id: r.target_id,
        target_name: r.target_name,
        severity: IncidentSeverity::from_db_str(&r.severity),
        started_at: r.started_at,
        ended_at: r.ended_at,
        public_title: r.public_title,
        latest_update,
    }
}

// ── In-memory impl (tests) ──────────────────────────────────────────────

#[derive(Default)]
pub struct InMemoryIncidentNarrationStore {
    inner: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    incidents: Vec<Incident>,
}

impl InMemoryIncidentNarrationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seed(&self, incident: Incident) {
        self.inner.lock().incidents.push(incident);
    }
}

#[async_trait]
impl IncidentNarrationStore for InMemoryIncidentNarrationStore {
    async fn count_ongoing_for_target(&self, _org: OrgId, target_id: Uuid) -> Result<u32> {
        Ok(self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| i.target_id == Some(target_id) && i.ended_at.is_none())
            .count() as u32)
    }

    async fn get(&self, _org: OrgId, id: Uuid) -> Result<Option<Incident>> {
        Ok(self
            .inner
            .lock()
            .incidents
            .iter()
            .find(|i| i.id == id)
            .cloned())
    }

    async fn patch_narration(
        &self,
        _org: OrgId,
        id: Uuid,
        update: IncidentNarrationUpdate,
    ) -> Result<Option<Incident>> {
        let mut g = self.inner.lock();
        let Some(inc) = g.incidents.iter_mut().find(|i| i.id == id) else {
            return Ok(None);
        };
        if let Some(t) = update.public_title {
            inc.public_title = t;
        }
        if let Some(d) = update.public_description {
            inc.public_description = d;
        }
        if let Some(s) = update.severity {
            inc.severity = s;
        }
        if let Some(c) = update.counts_as_downtime {
            inc.counts_as_downtime = c;
        }
        inc.updated_at = Some(Utc::now());
        Ok(Some(inc.clone()))
    }

    async fn append_update(
        &self,
        _org: OrgId,
        incident_id: Uuid,
        new: NewIncidentUpdate,
        _author: Option<String>,
    ) -> Result<Option<PublicIncidentUpdate>> {
        let mut g = self.inner.lock();
        let Some(inc) = g.incidents.iter_mut().find(|i| i.id == incident_id) else {
            return Ok(None);
        };
        let entry = PublicIncidentUpdate {
            posted_at: Utc::now(),
            phase: new.phase,
            message: new.message,
            generated: false,
        };
        inc.updates.push(entry.clone());
        inc.updated_at = Some(entry.posted_at);
        Ok(Some(entry))
    }

    async fn list_briefs(
        &self,
        _org: OrgId,
        filter: IncidentBriefFilter,
    ) -> Result<Vec<IncidentBrief>> {
        // In-memory store doesn't track targets; tests that need a real
        // `target_name` use the Postgres-backed store via the harness.
        let mut out: Vec<IncidentBrief> = self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| !filter.open_only || i.ended_at.is_none())
            .filter(|i| filter.target_id.is_none_or(|t| i.target_id == Some(t)))
            .filter(|i| {
                filter
                    .range
                    .is_none_or(|r| i.started_at < r.to && i.ended_at.is_none_or(|e| e >= r.from))
            })
            // A brief names a monitor, so a declared incident without one has
            // no place in this list; one whose monitor was deleted still does.
            .filter(|i| i.target_id.is_some() || i.target_name.is_some())
            .map(|i| IncidentBrief {
                id: i.id,
                target_id: i.target_id,
                target_name: i.target_name.clone().unwrap_or_default(),
                severity: i.severity,
                started_at: i.started_at,
                ended_at: i.ended_at,
                public_title: i.public_title.clone(),
                latest_update: i.updates.last().cloned(),
            })
            .collect();
        out.sort_by_key(|a| (a.started_at, a.id));
        if !filter.oldest_first {
            out.reverse();
        }
        Ok(out
            .into_iter()
            .skip(filter.offset)
            .take(filter.limit)
            .collect())
    }

    async fn count_active(&self, _org: OrgId) -> Result<u32> {
        let n = self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| i.ended_at.is_none())
            .count();
        Ok(n as u32)
    }

    async fn list_for_target(
        &self,
        _org: OrgId,
        target_id: Uuid,
        range: TimeRange,
        limit: usize,
        offset: usize,
        ongoing_only: bool,
    ) -> Result<Vec<Incident>> {
        let mut out: Vec<Incident> = self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| i.target_id == Some(target_id))
            .filter(|i| i.started_at < range.to && i.ended_at.is_none_or(|e| e >= range.from))
            .filter(|i| !ongoing_only || i.ended_at.is_none())
            .cloned()
            .collect();
        out.sort_by_key(|i| (std::cmp::Reverse(i.started_at), std::cmp::Reverse(i.id)));
        Ok(out.into_iter().skip(offset).take(limit).collect())
    }

    async fn titles(&self, _org: OrgId, _ids: &[Uuid]) -> Result<HashMap<Uuid, String>> {
        Ok(HashMap::new())
    }

    async fn confirmed_downtime_by_target(
        &self,
        _org: OrgId,
        range: TimeRange,
    ) -> Result<HashMap<Uuid, i64>> {
        let mut out: HashMap<Uuid, i64> = HashMap::new();
        for i in self.inner.lock().incidents.iter() {
            // Downtime is per monitor; a declared incident naming none has no
            // monitor to charge it to.
            let Some(target_id) = i.target_id else {
                continue;
            };
            if !i.counts_as_downtime {
                continue;
            }
            if i.started_at < range.to && i.ended_at.is_none_or(|e| e >= range.from) {
                let end = i.ended_at.unwrap_or(range.to).min(range.to);
                let start = i.started_at.max(range.from);
                *out.entry(target_id).or_default() += (end - start).num_seconds().max(0);
            }
        }
        Ok(out)
    }
    async fn count_overlapping(&self, _org: OrgId, range: TimeRange) -> Result<u64> {
        Ok(self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| i.started_at < range.to && i.ended_at.is_none_or(|e| e >= range.from))
            .count() as u64)
    }

    async fn downtime_spans(&self, _org: OrgId, range: TimeRange) -> Result<Vec<IncidentSpan>> {
        let mut out: Vec<IncidentSpan> = self
            .inner
            .lock()
            .incidents
            .iter()
            .filter(|i| i.counts_as_downtime)
            .filter(|i| i.started_at < range.to && i.ended_at.is_none_or(|e| e >= range.from))
            .filter_map(|i| {
                Some(IncidentSpan {
                    target_id: i.target_id?,
                    started_at: i.started_at,
                    ended_at: i.ended_at,
                    // The in-memory incident keeps no origin, so each reads as monitor-opened.
                    origin: "monitor".into(),
                    severity: i.severity,
                    status_at_start: i.status.as_str().into(),
                })
            })
            .collect();
        out.sort_by_key(|s| s.started_at);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn sample() -> Incident {
        Incident {
            id: Uuid::now_v7(),
            target_id: Some(Uuid::now_v7()),
            target_name: None,
            started_at: Utc::now() - ChronoDuration::minutes(15),
            ended_at: None,
            status: CheckStatus::Down,
            duration_secs: None,
            check_count: 3,
            error_sample: Some("timeout".into()),
            counts_as_downtime: true,
            severity: IncidentSeverity::Major,
            public_title: None,
            public_description: None,
            created_at: Some(Utc::now() - ChronoDuration::minutes(15)),
            updated_at: Some(Utc::now() - ChronoDuration::minutes(15)),
            updates: vec![],
            regions_down: Vec::new(),
            regions_up: Vec::new(),
        }
    }

    fn org() -> OrgId {
        OrgId(Uuid::nil())
    }

    /// The public projection carries no monitor for an incident declared
    /// without one, and every narration path decodes the same row.
    #[tokio::test]
    async fn a_target_less_incident_reads_and_patches() {
        let store = InMemoryIncidentNarrationStore::new();
        let mut inc = sample();
        inc.target_id = None;
        let id = inc.id;
        store.seed(inc);

        let read = store.get(org(), id).await.unwrap().unwrap();
        assert_eq!(read.target_id, None);

        let patched = store
            .patch_narration(
                org(),
                id,
                IncidentNarrationUpdate {
                    public_title: Some(Some("Partner API down".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(patched.target_id, None);
        assert_eq!(patched.public_title.as_deref(), Some("Partner API down"));
    }

    /// A brief names a monitor, so one that has none belongs to no list rather
    /// than to an arbitrary monitor.
    #[tokio::test]
    async fn briefs_leave_out_an_incident_with_no_monitor() {
        let store = InMemoryIncidentNarrationStore::new();
        let mut bound = sample();
        bound.target_id = Some(Uuid::now_v7());
        let mut loose = sample();
        loose.id = Uuid::now_v7();
        loose.target_id = None;
        let (bound_id, loose_id) = (bound.id, loose.id);
        store.seed(bound);
        store.seed(loose);

        let briefs = store
            .list_briefs(org(), IncidentBriefFilter::default())
            .await
            .unwrap();
        assert!(briefs.iter().any(|b| b.id == bound_id));
        assert!(!briefs.iter().any(|b| b.id == loose_id));
    }

    #[tokio::test]
    async fn patch_narration_overwrites_fields() {
        let store = InMemoryIncidentNarrationStore::new();
        let inc = sample();
        let id = inc.id;
        store.seed(inc);
        let patched = store
            .patch_narration(
                org(),
                id,
                IncidentNarrationUpdate {
                    public_title: Some(Some("Latency spike".into())),
                    public_description: Some(Some("EU".into())),
                    severity: Some(IncidentSeverity::Critical),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(patched.public_title.as_deref(), Some("Latency spike"));
        assert_eq!(patched.public_description.as_deref(), Some("EU"));
        assert_eq!(patched.severity, IncidentSeverity::Critical);
    }

    #[tokio::test]
    async fn patch_narration_with_none_clears_fields() {
        let store = InMemoryIncidentNarrationStore::new();
        let mut inc = sample();
        inc.public_title = Some("old".into());
        let id = inc.id;
        store.seed(inc);
        let patched = store
            .patch_narration(
                org(),
                id,
                IncidentNarrationUpdate {
                    public_title: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(patched.public_title, None);
    }

    #[tokio::test]
    async fn append_update_records_entry() {
        let store = InMemoryIncidentNarrationStore::new();
        let inc = sample();
        let id = inc.id;
        store.seed(inc);
        let entry = store
            .append_update(
                org(),
                id,
                NewIncidentUpdate {
                    phase: IncidentStatusPhase::Identified,
                    message: "rolling back".into(),
                },
                Some("op@example.com".into()),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.phase, IncidentStatusPhase::Identified);
        let again = store.get(org(), id).await.unwrap().unwrap();
        assert_eq!(again.updates.len(), 1);
    }

    #[tokio::test]
    async fn append_update_for_missing_incident_returns_none() {
        let store = InMemoryIncidentNarrationStore::new();
        let res = store
            .append_update(
                org(),
                Uuid::now_v7(),
                NewIncidentUpdate {
                    phase: IncidentStatusPhase::Investigating,
                    message: "x".into(),
                },
                None,
            )
            .await
            .unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn list_for_target_filters_by_window_ongoing_and_offset() {
        let store = InMemoryIncidentNarrationStore::new();
        let target = Uuid::now_v7();
        let other = Uuid::now_v7();
        let now = Utc::now();
        let mk = |tid: Uuid, started_min_ago: i64, ended_min_ago: Option<i64>| {
            let mut inc = sample();
            inc.id = Uuid::now_v7();
            inc.target_id = Some(tid);
            inc.started_at = now - ChronoDuration::minutes(started_min_ago);
            inc.ended_at = ended_min_ago.map(|m| now - ChronoDuration::minutes(m));
            inc
        };
        store.seed(mk(target, 30, Some(20))); // closed, inside window
        store.seed(mk(target, 10, None)); // ongoing
        store.seed(mk(target, 60 * 48, Some(60 * 47))); // fully before window
        store.seed(mk(other, 5, None)); // different target

        let window = TimeRange {
            from: now - ChronoDuration::hours(24),
            to: now,
        };
        let all = store
            .list_for_target(org(), target, window, 50, 0, false)
            .await
            .unwrap();
        assert_eq!(all.len(), 2); // the out-of-window and other-target rows excluded
        assert!(all[0].started_at >= all[1].started_at); // newest first

        let ongoing = store
            .list_for_target(org(), target, window, 50, 0, true)
            .await
            .unwrap();
        assert_eq!(ongoing.len(), 1);
        assert!(ongoing[0].ended_at.is_none());

        let page2 = store
            .list_for_target(org(), target, window, 1, 1, false)
            .await
            .unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].started_at, all[1].started_at); // offset skipped the newest
    }

    #[tokio::test]
    async fn list_briefs_keeps_a_deleted_monitors_incident_and_skips_a_bare_declared_one() {
        let store = InMemoryIncidentNarrationStore::new();
        let mut deleted = sample();
        deleted.id = Uuid::now_v7();
        deleted.target_id = None;
        deleted.target_name = Some("old-worker".into());
        store.seed(deleted.clone());
        let mut declared = sample();
        declared.id = Uuid::now_v7();
        declared.target_id = None;
        declared.target_name = None;
        store.seed(declared);

        let briefs = store
            .list_briefs(org(), IncidentBriefFilter::default())
            .await
            .unwrap();
        assert_eq!(briefs.len(), 1);
        assert_eq!(briefs[0].id, deleted.id);
        assert!(briefs[0].monitor_deleted());
        assert_eq!(briefs[0].target_name, "old-worker");
    }

    #[tokio::test]
    async fn list_briefs_filters_by_state_target_and_window() {
        let store = InMemoryIncidentNarrationStore::new();
        let target = Uuid::now_v7();
        let other = Uuid::now_v7();
        let now = Utc::now();
        let mk = |tid: Uuid, started_min_ago: i64, ended_min_ago: Option<i64>| {
            let mut inc = sample();
            inc.id = Uuid::now_v7();
            inc.target_id = Some(tid);
            inc.started_at = now - ChronoDuration::minutes(started_min_ago);
            inc.ended_at = ended_min_ago.map(|m| now - ChronoDuration::minutes(m));
            inc
        };
        store.seed(mk(target, 30, Some(20))); // resolved, inside the window
        store.seed(mk(target, 10, None)); // still open
        store.seed(mk(target, 60 * 24 * 40, Some(60 * 24 * 39))); // long resolved
        store.seed(mk(other, 5, None)); // different monitor

        let window = TimeRange {
            from: now - ChronoDuration::days(30),
            to: now,
        };
        let open = store
            .list_briefs(org(), IncidentBriefFilter::default())
            .await
            .unwrap();
        assert_eq!(open.len(), 2);
        assert!(open.iter().all(|i| i.ended_at.is_none()));

        let all = store
            .list_briefs(
                org(),
                IncidentBriefFilter {
                    range: Some(window),
                    target_id: Some(target),
                    open_only: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // The other monitor's row and the pre-window one are both excluded.
        assert_eq!(all.len(), 2);
        assert!(all[0].started_at >= all[1].started_at); // newest first

        let oldest_first = store
            .list_briefs(
                org(),
                IncidentBriefFilter {
                    oldest_first: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(oldest_first[0].started_at <= oldest_first[1].started_at);

        let page2 = store
            .list_briefs(
                org(),
                IncidentBriefFilter {
                    range: Some(window),
                    target_id: Some(target),
                    open_only: false,
                    limit: 1,
                    offset: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].started_at, all[1].started_at);
    }

    /// An incident that opened before the window and is still running stays
    /// visible: "what's broken" must not depend on when it broke.
    #[tokio::test]
    async fn list_briefs_keeps_an_open_incident_older_than_the_window() {
        let store = InMemoryIncidentNarrationStore::new();
        let now = Utc::now();
        let mut inc = sample();
        inc.started_at = now - ChronoDuration::days(90);
        inc.ended_at = None;
        store.seed(inc);

        let found = store
            .list_briefs(
                org(),
                IncidentBriefFilter {
                    range: Some(TimeRange {
                        from: now - ChronoDuration::days(30),
                        to: now,
                    }),
                    open_only: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
    }

    #[tokio::test]
    async fn confirmed_downtime_by_target_sums_clamped_overlap() {
        let store = InMemoryIncidentNarrationStore::new();
        let (t1, t2, t3) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let now = Utc::now();
        let mk = |tid: Uuid, start_min: i64, end_min: Option<i64>| {
            let mut i = sample();
            i.id = Uuid::now_v7();
            i.target_id = Some(tid);
            i.started_at = now - ChronoDuration::minutes(start_min);
            i.ended_at = end_min.map(|m| now - ChronoDuration::minutes(m));
            i
        };
        store.seed(mk(t1, 50, Some(40))); // 600s
        store.seed(mk(t1, 20, None)); // ongoing -> 1200s
        store.seed(mk(t2, 90, Some(50))); // clamped to window -> 600s
        store.seed(mk(t3, 120, Some(110))); // before window -> excluded

        let window = TimeRange {
            from: now - ChronoDuration::hours(1),
            to: now,
        };
        let map = store
            .confirmed_downtime_by_target(org(), window)
            .await
            .unwrap();
        assert_eq!(map.get(&t1).copied(), Some(1800));
        assert_eq!(map.get(&t2).copied(), Some(600));
        assert_eq!(map.get(&t3), None);
    }
}
