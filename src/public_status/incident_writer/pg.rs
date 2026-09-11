//! The Postgres-backed [`IncidentStore`].

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::domain::{CheckStatus, OrgId};
use crate::error::Result;

use super::{IncidentStore, NewOpenIncident, OpenIncident};

pub struct PgIncidentStore {
    pool: PgPool,
}

impl PgIncidentStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(FromRow)]
struct OpenIncidentRow {
    id: Uuid,
    target_id: Uuid,
    started_at: DateTime<Utc>,
    region: Option<String>,
    regions_down: Option<Vec<String>>,
}

#[async_trait]
impl IncidentStore for PgIncidentStore {
    async fn open_for_target(&self, org: OrgId, target_id: Uuid) -> Result<Option<OpenIncident>> {
        let row: Option<OpenIncidentRow> = sqlx::query_as::<_, OpenIncidentRow>(
            r#"SELECT id, target_id, started_at, region, regions_down FROM incidents
               WHERE target_id = $1 AND org_id = $2 AND ended_at IS NULL
                 AND origin = 'monitor'
               ORDER BY started_at DESC LIMIT 1"#,
        )
        .bind(target_id)
        .bind(org.0)
        .fetch_optional(&self.pool)
        .await
        .context("incident open_for_target")?;
        Ok(row.map(|r| OpenIncident {
            id: r.id,
            target_id: r.target_id,
            started_at: r.started_at,
            region: r.region,
            regions_down: r.regions_down.unwrap_or_default(),
        }))
    }

    async fn open_for_pairs(
        &self,
        pairs: &[(OrgId, Uuid)],
    ) -> Result<std::collections::HashMap<(OrgId, Uuid), Vec<OpenIncident>>> {
        if pairs.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        // UNNEST is the supported way to push a `(uuid, uuid)` zip into a
        // single SQL set without ballooning bind count or fanning out to
        // `IN ($1,$2),($3,$4),…`. Joining on the partial index
        // `incidents(org_id, target_id) WHERE ended_at IS NULL` makes this
        // O(page_size) index probes inside one round-trip.
        let (orgs, targets): (Vec<Uuid>, Vec<Uuid>) = pairs.iter().map(|(o, t)| (o.0, *t)).unzip();
        #[derive(FromRow)]
        struct Row {
            org_id: Uuid,
            id: Uuid,
            target_id: Uuid,
            started_at: DateTime<Utc>,
            region: Option<String>,
            regions_down: Option<Vec<String>>,
        }
        let rows: Vec<Row> = sqlx::query_as::<_, Row>(
            r#"SELECT i.org_id, i.id, i.target_id, i.started_at, i.region, i.regions_down
               FROM incidents i
               JOIN unnest($1::uuid[], $2::uuid[]) AS pairs(org_id, target_id)
                 ON i.org_id = pairs.org_id AND i.target_id = pairs.target_id
               WHERE i.ended_at IS NULL AND i.origin = 'monitor'"#,
        )
        .bind(&orgs)
        .bind(&targets)
        .fetch_all(&self.pool)
        .await
        .context("incident open_for_pairs")?;
        let mut out: std::collections::HashMap<(OrgId, Uuid), Vec<OpenIncident>> =
            std::collections::HashMap::new();
        for r in rows {
            out.entry((OrgId(r.org_id), r.target_id))
                .or_default()
                .push(OpenIncident {
                    id: r.id,
                    target_id: r.target_id,
                    started_at: r.started_at,
                    region: r.region,
                    regions_down: r.regions_down.unwrap_or_default(),
                });
        }
        Ok(out)
    }

    async fn insert_open(&self, org: OrgId, new: NewOpenIncident) -> Result<Option<Uuid>> {
        let status_at_start = status_to_db(new.status_at_start)
            .ok_or_else(|| anyhow::anyhow!("cannot open incident from status=up"))?;
        // ON CONFLICT on the partial unique index is the race-safe single-open
        // guarantee: a concurrent writer yields no row → None → no page. Scoped
        // to monitor origin so an operator's open declaration cannot shadow a
        // real detection.
        // Visibility is derived here, not by the caller: an incident is public
        // only while its monitor is a component of an enabled status page that
        // the plan still covers. A held page shows nobody anything, so an
        // incident opened behind one starts internal and is not fanned out.
        let row: Option<(Uuid,)> = sqlx::query_as(
            r#"INSERT INTO incidents (org_id, target_id, started_at, status_at_start, check_count, error_sample, region, regions_down, regions_up, origin, visibility)
               SELECT $6, $1, $2, $3, $4, $5, $7, $8, $9, 'monitor',
                      CASE WHEN EXISTS (
                          SELECT 1 FROM status_page_components spc
                          JOIN status_pages sp ON sp.id = spc.status_page_id
                          WHERE spc.target_id = $1 AND spc.org_id = $6 AND sp.enabled = true
                            AND sp.plan_hold_at IS NULL
                      ) THEN 'public' ELSE 'internal' END
               ON CONFLICT (org_id, target_id) WHERE ended_at IS NULL AND origin = 'monitor'
               DO NOTHING
               RETURNING id"#,
        )
        .bind(new.target_id)
        .bind(new.started_at)
        .bind(status_at_start)
        .bind(new.check_count as i32)
        .bind(new.error_sample)
        .bind(org.0)
        .bind(new.region)
        .bind(&new.regions_down)
        .bind(&new.regions_up)
        .fetch_optional(&self.pool)
        .await
        .context("incident insert_open")?;
        Ok(row.map(|r| r.0))
    }

    async fn close(&self, org: OrgId, incident_id: Uuid, ended_at: DateTime<Utc>) -> Result<bool> {
        // Recovery: close the row, resolve with no human resolver. For a public
        // incident, append a `resolved` update for the timeline. The UPDATE
        // matches only while ended_at was NULL, so the final SELECT returns a
        // row only for the call that actually closed it — a re-run or the race
        // loser returns None and never re-pages.
        let row: Option<(Uuid,)> = sqlx::query_as(
            r#"WITH closed AS (
                   UPDATE incidents
                      SET ended_at = $2,
                          duration_secs = GREATEST(0, EXTRACT(EPOCH FROM ($2 - started_at))::int),
                          state = 'resolved',
                          resolved_by = NULL,
                          next_escalation_at = NULL,
                          updated_at = now()
                    WHERE id = $1 AND org_id = $3 AND ended_at IS NULL
                   RETURNING id, org_id, visibility
               ),
               ins AS (
                   INSERT INTO incident_updates (org_id, incident_id, phase, message, author)
                   SELECT org_id, id, 'resolved', $4, 'system'
                   FROM closed WHERE visibility = 'public'
               )
               SELECT id FROM closed"#,
        )
        .bind(incident_id)
        .bind(ended_at)
        .bind(org.0)
        .bind(crate::storage::incident_ops::AUTO_RESOLVED_MESSAGE)
        .fetch_optional(&self.pool)
        .await
        .context("incident close")?;
        Ok(row.is_some())
    }

    async fn widen(&self, org: OrgId, incident_id: Uuid, regions: &[String]) -> Result<()> {
        sqlx::query(
            r#"UPDATE incidents
                  SET regions_down = COALESCE(regions_down, '{}')
                        || ARRAY(SELECT r FROM unnest($3::text[]) AS r
                                  WHERE r <> ALL(COALESCE(regions_down, '{}'))),
                      regions_up = ARRAY(SELECT r FROM unnest(COALESCE(regions_up, '{}')) AS r
                                          WHERE r <> ALL($3::text[])),
                      updated_at = now()
                WHERE id = $1 AND org_id = $2 AND ended_at IS NULL"#,
        )
        .bind(incident_id)
        .bind(org.0)
        .bind(regions)
        .execute(&self.pool)
        .await
        .context("incident widen")?;
        Ok(())
    }
}

fn status_to_db(s: CheckStatus) -> Option<&'static str> {
    match s {
        CheckStatus::Down => Some("down"),
        CheckStatus::Degraded => Some("degraded"),
        CheckStatus::Error => Some("error"),
        CheckStatus::Up => None,
    }
}
