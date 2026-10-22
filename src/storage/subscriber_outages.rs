//! Page outages for the subscriber planner: the open ones with what each
//! member's incident now says, the incidents no outage holds yet, and writing
//! back what the rules changed. Cross-org by design: one planner serves every
//! page.

use anyhow::Context;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::Result;
use crate::public_status::subscriber_notice::PageRow;
use crate::storage::page_sql::{PAGE_PLAN_JOIN, PAGE_SERVED, page_row_columns};
use crate::storage::subscribers::FANOUT_LOOKBACK_HOURS;

/// One member of an open outage, with its outage and what its incident says.
#[derive(Debug, sqlx::FromRow)]
pub struct MemberRow {
    #[sqlx(flatten)]
    pub page: PageRow,
    pub outage_id: Uuid,
    pub opened_at: DateTime<Utc>,
    pub outage_started_at: DateTime<Utc>,
    pub recovered_at: Option<DateTime<Utc>>,
    pub started_told: bool,
    pub incident_id: Uuid,
    pub target_id: Uuid,
    pub component_name: String,
    /// The member's own end, kept as the page saw it.
    pub ended_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub incident_ended_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub public_title: Option<String>,
    pub status_at_start: String,
    /// Public now, not just published once.
    pub public: bool,
    /// Still a component of a served page, not held.
    pub shows: bool,
}

pub async fn open_members(conn: &mut sqlx::PgConnection) -> Result<Vec<MemberRow>> {
    let sql = format!(
        "SELECT {page},
                o.id AS outage_id, o.opened_at, o.started_at AS outage_started_at,
                o.recovered_at, o.started_told,
                m.incident_id, m.target_id, m.component_name, m.ended_at, m.closed_at,
                i.started_at, i.ended_at AS incident_ended_at, i.resolved_at, i.public_title,
                i.status_at_start,
                i.visibility = 'public' AS public,
                ({PAGE_SERVED} AND EXISTS (
                    SELECT 1 FROM status_page_components c
                    JOIN targets t ON t.id = c.target_id AND t.org_id = c.org_id
                    WHERE c.status_page_id = sp.id AND c.org_id = sp.org_id
                      AND c.target_id = i.target_id AND t.plan_hold_at IS NULL)) AS shows
         FROM status_page_outages o
         JOIN status_page_outage_members m ON m.outage_id = o.id AND m.org_id = o.org_id
         JOIN incidents i ON i.id = m.incident_id AND i.org_id = m.org_id
         JOIN status_pages sp ON sp.id = o.status_page_id AND sp.org_id = o.org_id
         {PAGE_PLAN_JOIN}
         WHERE o.resolved_at IS NULL
         ORDER BY o.status_page_id, o.opened_at, m.joined_at",
        page = page_row_columns(),
    );
    sqlx::query_as::<_, MemberRow>(&sql)
        .fetch_all(conn)
        .await
        .context("subscriber_outages::open_members")
        .map_err(Into::into)
}

/// A monitor incident on a served page that no outage holds yet.
#[derive(Debug, sqlx::FromRow)]
pub struct CandidateRow {
    #[sqlx(flatten)]
    pub page: PageRow,
    pub incident_id: Uuid,
    pub target_id: Uuid,
    pub component_name: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// When the page first said something about it.
    pub said_at: DateTime<Utc>,
    pub public_title: Option<String>,
    pub status_at_start: String,
    pub public: bool,
    /// Left this page while down after its start was told, and is back: its
    /// return is not news. One that ended and was reopened is.
    pub told_before: bool,
}

/// Open incidents that sit in no unresolved outage on the page, and incidents
/// that ended inside the lookback and sit in no outage on the page at all, so
/// one that opened and closed between two passes is still told.
pub async fn candidates(conn: &mut sqlx::PgConnection) -> Result<Vec<CandidateRow>> {
    let select = format!(
        "SELECT {page},
                i.id AS incident_id, i.target_id,
                COALESCE(NULLIF(c.public_name, ''), t.name) AS component_name,
                i.started_at, i.ended_at, i.resolved_at,
                COALESCE((SELECT min(u.posted_at) FROM incident_updates u
                          WHERE u.incident_id = i.id AND u.org_id = i.org_id),
                         i.created_at) AS said_at,
                i.public_title, i.status_at_start, i.visibility = 'public' AS public,
                EXISTS (SELECT 1 FROM status_page_outage_members m
                        JOIN status_page_outages o ON o.id = m.outage_id AND o.org_id = m.org_id
                        WHERE m.status_page_id = sp.id AND m.incident_id = i.id
                          AND m.org_id = i.org_id AND o.started_told
                          AND m.closed_at IS NOT NULL AND m.ended_at IS NULL) AS told_before
         FROM incidents i
         JOIN status_page_components c ON c.org_id = i.org_id AND c.target_id = i.target_id
         JOIN targets t ON t.id = i.target_id AND t.org_id = i.org_id AND t.plan_hold_at IS NULL
         JOIN status_pages sp ON sp.id = c.status_page_id AND sp.org_id = c.org_id
         {PAGE_PLAN_JOIN}
         WHERE i.origin = 'monitor' AND {PAGE_SERVED} AND {told}",
        page = page_row_columns(),
        told = crate::storage::incidents::ever_public_sql("i", "i.org_id"),
    );
    let sql = format!(
        "{select}
           AND i.ended_at IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM status_page_outage_members m
               JOIN status_page_outages o ON o.id = m.outage_id
               WHERE m.status_page_id = sp.id AND m.incident_id = i.id
                 AND o.resolved_at IS NULL)
         UNION ALL
         {select}
           AND i.ended_at >= now() - make_interval(hours => $1)
           AND NOT EXISTS (
               SELECT 1 FROM status_page_outage_members m
               WHERE m.status_page_id = sp.id AND m.incident_id = i.id)"
    );
    sqlx::query_as::<_, CandidateRow>(&sql)
        .bind(FANOUT_LOOKBACK_HOURS as i32)
        .fetch_all(conn)
        .await
        .context("subscriber_outages::candidates")
        .map_err(Into::into)
}

/// An outage as the rules left it.
pub struct OutageWrite {
    pub id: Uuid,
    pub org_id: Uuid,
    pub page_id: Uuid,
    pub opened_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub recovered_at: Option<DateTime<Utc>>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub started_told: bool,
}

pub struct MemberWrite {
    pub outage_id: Uuid,
    pub org_id: Uuid,
    pub page_id: Uuid,
    pub incident_id: Uuid,
    pub target_id: Uuid,
    pub component_name: String,
    pub ended_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// Inserts new outages and members and updates changed ones, outages first.
pub async fn write(
    conn: &mut sqlx::PgConnection,
    outages: &[OutageWrite],
    members: &[MemberWrite],
) -> Result<()> {
    if !outages.is_empty() {
        sqlx::query(
            "INSERT INTO status_page_outages
                 (id, org_id, status_page_id, opened_at, started_at, recovered_at, resolved_at,
                  started_told)
             SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::timestamptz[],
                                  $5::timestamptz[], $6::timestamptz[], $7::timestamptz[],
                                  $8::bool[])
             ON CONFLICT (id) DO UPDATE SET
                 started_at = EXCLUDED.started_at,
                 recovered_at = EXCLUDED.recovered_at,
                 resolved_at = EXCLUDED.resolved_at,
                 started_told = EXCLUDED.started_told",
        )
        .bind(outages.iter().map(|o| o.id).collect::<Vec<_>>())
        .bind(outages.iter().map(|o| o.org_id).collect::<Vec<_>>())
        .bind(outages.iter().map(|o| o.page_id).collect::<Vec<_>>())
        .bind(outages.iter().map(|o| o.opened_at).collect::<Vec<_>>())
        .bind(outages.iter().map(|o| o.started_at).collect::<Vec<_>>())
        .bind(outages.iter().map(|o| o.recovered_at).collect::<Vec<_>>())
        .bind(outages.iter().map(|o| o.resolved_at).collect::<Vec<_>>())
        .bind(outages.iter().map(|o| o.started_told).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await
        .context("subscriber_outages::write outages")?;
    }
    if !members.is_empty() {
        sqlx::query(
            "INSERT INTO status_page_outage_members
                 (outage_id, org_id, status_page_id, incident_id, target_id, component_name,
                  ended_at, closed_at)
             SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::uuid[], $5::uuid[],
                                  $6::text[], $7::timestamptz[], $8::timestamptz[])
             ON CONFLICT (outage_id, incident_id) DO UPDATE SET
                 ended_at = EXCLUDED.ended_at, closed_at = EXCLUDED.closed_at",
        )
        .bind(members.iter().map(|m| m.outage_id).collect::<Vec<_>>())
        .bind(members.iter().map(|m| m.org_id).collect::<Vec<_>>())
        .bind(members.iter().map(|m| m.page_id).collect::<Vec<_>>())
        .bind(members.iter().map(|m| m.incident_id).collect::<Vec<_>>())
        .bind(members.iter().map(|m| m.target_id).collect::<Vec<_>>())
        .bind(
            members
                .iter()
                .map(|m| m.component_name.clone())
                .collect::<Vec<_>>(),
        )
        .bind(members.iter().map(|m| m.ended_at).collect::<Vec<_>>())
        .bind(members.iter().map(|m| m.closed_at).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await
        .context("subscriber_outages::write members")?;
    }
    Ok(())
}

/// Periodic cleanup: outages that ended more than 30 days ago, and their
/// members with them.
pub async fn purge_old(pool: &sqlx::PgPool) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "DELETE FROM status_page_outages
         WHERE resolved_at < now() - INTERVAL '30 days'",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
