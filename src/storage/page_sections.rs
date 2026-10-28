//! A monitor group shown on a status page as a live section. Its members are
//! the page's rows whose monitor is in the group, so a section has no rows of
//! its own: joining the group adds the monitor's row to every page with a
//! section for it, leaving deletes those rows, and every change runs inside
//! the transaction of the write that caused it.
//!
//! LOCK ORDER: the account advisory lock (held by every caller here), groups,
//! monitors, page rows. Under the account lock no two of these writers hold
//! one group while waiting on another.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::{OrgId, UserId};
use crate::error::{AppError, Result, codes};
use crate::storage::accounts::{self, live_orgs};
use crate::storage::locks::{account_lock_key, advisory_xact_lock};
use crate::storage::monitor_groups::lock_group;
use crate::storage::orgs::record_audit_tx;

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

pub const MAX_HEADING_LEN: usize = 50;

/// A group's section on one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PageSection {
    pub id: Uuid,
    pub group_id: Uuid,
    /// The group's name, the heading unless `heading` is set. Untrusted data.
    pub group_name: String,
    #[schema(nullable = true, max_length = 50)]
    pub heading: Option<String>,
    /// The members on the page, in page order.
    pub members: Vec<Uuid>,
    /// Members the plan's public component limit keeps off the page.
    pub left_off: u32,
    pub created_at: DateTime<Utc>,
}

/// One monitor's group change in a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GroupMove {
    pub target: Uuid,
    pub from: Option<Uuid>,
    pub to: Option<Uuid>,
}

/// The pages a membership change touched, for the cache clear after commit.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PageRowChange {
    pub pages: BTreeSet<Uuid>,
    /// Rows the public component limit left off.
    pub left_off: usize,
}

fn db(context: &'static str) -> impl FnOnce(sqlx::Error) -> AppError {
    move |e| AppError::Other(anyhow!("{context}: {e}"))
}

/// The rows the monitors in `moves` gain and lose on pages with a section for
/// the group they join or leave.
pub(crate) async fn follow_moves(
    tx: &mut Tx<'_>,
    org: OrgId,
    moves: &[GroupMove],
    max_public_components: i64,
    actor: Option<UserId>,
) -> Result<PageRowChange> {
    let moved: Vec<&GroupMove> = moves.iter().filter(|m| m.from != m.to).collect();
    if moved.is_empty() {
        return Ok(PageRowChange::default());
    }
    let targets: Vec<Uuid> = moved.iter().map(|m| m.target).collect();
    let from: Vec<Option<Uuid>> = moved.iter().map(|m| m.from).collect();
    let to: Vec<Option<Uuid>> = moved.iter().map(|m| m.to).collect();
    let pairs = |column: &'static str| {
        format!(
            "SELECT s.status_page_id, m.target FROM status_page_sections s \
             JOIN unnest($2::uuid[], $3::uuid[], $4::uuid[]) AS m(target, from_g, to_g) \
               ON s.group_id = m.{column} \
             WHERE s.org_id = $1 ORDER BY s.status_page_id, m.target"
        )
    };
    let leaving: Vec<(Uuid, Uuid)> = sqlx::query_as(&pairs("from_g"))
        .bind(org.0)
        .bind(&targets)
        .bind(&from)
        .bind(&to)
        .fetch_all(&mut **tx)
        .await
        .map_err(db("sections left"))?;
    let joining: Vec<(Uuid, Uuid)> = sqlx::query_as(&pairs("to_g"))
        .bind(org.0)
        .bind(&targets)
        .bind(&from)
        .bind(&to)
        .fetch_all(&mut **tx)
        .await
        .map_err(db("sections joined"))?;
    let mut change = remove_rows(tx, org, &leaving, actor).await?;
    let added = add_rows(tx, org, &joining, max_public_components, actor).await?;
    change.pages.extend(added.pages);
    change.left_off = added.left_off;
    Ok(change)
}

/// Adds the `(page, monitor)` rows not there yet, in order, until the
/// account's distinct published monitors would pass `cap`; the rest are left
/// off. A monitor already published anywhere in the account costs nothing.
pub(crate) async fn add_rows(
    tx: &mut Tx<'_>,
    org: OrgId,
    pairs: &[(Uuid, Uuid)],
    cap: i64,
    actor: Option<UserId>,
) -> Result<PageRowChange> {
    if pairs.is_empty() {
        return Ok(PageRowChange::default());
    }
    // Held so a page deleted meanwhile drops out here instead of failing the
    // monitor's write on its foreign key.
    let wanted: Vec<Uuid> = pairs.iter().map(|p| p.0).collect();
    let live: HashSet<Uuid> = sqlx::query_scalar(
        "SELECT id FROM status_pages WHERE org_id = $1 AND id = ANY($2) ORDER BY id FOR KEY SHARE",
    )
    .bind(org.0)
    .bind(&wanted)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("lock pages"))?
    .into_iter()
    .collect();
    let pairs: Vec<(Uuid, Uuid)> = pairs
        .iter()
        .copied()
        .filter(|p| live.contains(&p.0))
        .collect();
    let pairs = pairs.as_slice();
    let account = accounts::account_for_org(&mut **tx, org).await?;
    let pages: Vec<Uuid> = pairs.iter().map(|p| p.0).collect();
    let targets: Vec<Uuid> = pairs.iter().map(|p| p.1).collect();
    let pool_orgs = live_orgs("$2");
    let used: i64 = sqlx::query_scalar(&format!(
        "SELECT count(DISTINCT target_id) FROM status_page_components \
         WHERE org_id IN ({})",
        live_orgs("$1")
    ))
    .bind(account.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(db("count published"))?;
    let published: Vec<Uuid> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT target_id FROM status_page_components \
         WHERE org_id IN ({pool_orgs}) AND target_id = ANY($1)"
    ))
    .bind(&targets)
    .bind(account.0)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("published members"))?;
    let present: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT spc.status_page_id, spc.target_id FROM status_page_components spc \
         JOIN unnest($2::uuid[], $3::uuid[]) AS r(page, target) \
           ON spc.status_page_id = r.page AND spc.target_id = r.target \
         WHERE spc.org_id = $1",
    )
    .bind(org.0)
    .bind(&pages)
    .bind(&targets)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("rows present"))?;

    let present: HashSet<(Uuid, Uuid)> = present.into_iter().collect();
    let mut counted: HashSet<Uuid> = published.into_iter().collect();
    let mut used = used;
    let mut change = PageRowChange::default();
    let mut by_page: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for &(page, target) in pairs {
        if present.contains(&(page, target)) {
            continue;
        }
        if !counted.contains(&target) {
            if used + 1 > cap {
                change.left_off += 1;
                continue;
            }
            used += 1;
            counted.insert(target);
        }
        by_page.entry(page).or_default().push(target);
    }
    if by_page.is_empty() {
        return Ok(change);
    }
    let (mut ins_pages, mut ins_targets, mut ins_nth) = (Vec::new(), Vec::new(), Vec::new());
    for (page, members) in &by_page {
        for (n, target) in (1_i32..).zip(members) {
            ins_pages.push(*page);
            ins_targets.push(*target);
            ins_nth.push(n);
        }
    }
    // A joining member goes last: a section's rows render together at its
    // first member's place, whatever their own order numbers.
    sqlx::query(
        "INSERT INTO status_page_components (org_id, status_page_id, target_id, sort_order) \
         SELECT $1, r.page, r.target, \
                COALESCE((SELECT max(c.sort_order) FROM status_page_components c \
                          WHERE c.status_page_id = r.page AND c.org_id = $1), -1) + r.nth \
         FROM unnest($2::uuid[], $3::uuid[], $4::int4[]) AS r(page, target, nth) \
         ON CONFLICT (status_page_id, target_id) DO NOTHING",
    )
    .bind(org.0)
    .bind(&ins_pages)
    .bind(&ins_targets)
    .bind(&ins_nth)
    .execute(&mut **tx)
    .await
    .map_err(db("add section rows"))?;
    for (page, members) in by_page {
        record_audit_tx(
            tx,
            org,
            actor,
            "status_page.component_added",
            serde_json::json!({ "page_id": page, "target_ids": members, "via": "group" }),
        )
        .await?;
        change.pages.insert(page);
    }
    Ok(change)
}

/// Deletes the `(page, monitor)` rows, keeping what the pages already showed
/// for them in their history as a hand removal does, and revokes each row's
/// detail link in the same transaction.
pub(crate) async fn remove_rows(
    tx: &mut Tx<'_>,
    org: OrgId,
    pairs: &[(Uuid, Uuid)],
    actor: Option<UserId>,
) -> Result<PageRowChange> {
    let removed = delete_rows(tx, org, pairs, actor).await?;
    let mut by_page: BTreeMap<Uuid, Vec<Uuid>> = BTreeMap::new();
    for (page, target) in removed {
        by_page.entry(page).or_default().push(target);
    }
    let mut change = PageRowChange::default();
    for (page, members) in by_page {
        record_audit_tx(
            tx,
            org,
            actor,
            "status_page.component_removed",
            serde_json::json!({ "page_id": page, "target_ids": members, "via": "group" }),
        )
        .await?;
        change.pages.insert(page);
    }
    Ok(change)
}

/// The shared delete behind [`remove_rows`] and a hand removal: rows gone,
/// history kept, detail links revoked. Returns the rows it removed.
pub(crate) async fn delete_rows(
    tx: &mut Tx<'_>,
    org: OrgId,
    pairs: &[(Uuid, Uuid)],
    actor: Option<UserId>,
) -> Result<Vec<(Uuid, Uuid)>> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let pages: Vec<Uuid> = pairs.iter().map(|p| p.0).collect();
    let targets: Vec<Uuid> = pairs.iter().map(|p| p.1).collect();
    let rows: Vec<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(&format!(
        r#"WITH removed AS (
               DELETE FROM status_page_components spc
               USING unnest($1::uuid[], $2::uuid[]) AS r(page, target)
               WHERE spc.status_page_id = r.page AND spc.target_id = r.target
                 AND spc.org_id = $3
               RETURNING spc.status_page_id, spc.target_id, spc.public_name, spc.share_id
           ), leaving AS (
               SELECT r.status_page_id, r.target_id,
                      COALESCE(NULLIF(r.public_name, ''), t.name) AS name
               FROM removed r
               JOIN targets t ON t.id = r.target_id AND t.org_id = $3
           ), windows AS (
               {keep_windows}
           ), incidents AS (
               INSERT INTO incident_status_pages
                   (org_id, incident_id, status_page_id, component_name)
               SELECT $3, i.id, l.status_page_id, l.name
               FROM leaving l
               JOIN incidents i ON i.target_id = l.target_id AND i.org_id = $3
               WHERE {ever_public}
                 AND i.started_at >= now() - make_interval(days => {kept_days})
               ON CONFLICT (incident_id, status_page_id) DO UPDATE
               SET component_name = COALESCE(incident_status_pages.component_name,
                                             EXCLUDED.component_name)
           ), revoked AS (
               UPDATE monitor_shares ms SET revoked_at = now()
               FROM removed r
               WHERE ms.id = r.share_id AND ms.org_id = $3 AND ms.revoked_at IS NULL
               RETURNING ms.id
           )
           SELECT r.status_page_id, r.target_id, rv.id
           FROM removed r LEFT JOIN revoked rv ON rv.id = r.share_id"#,
        keep_windows = crate::storage::maintenance::keep_started_windows_sql("leaving", "$3", None),
        ever_public = crate::storage::incidents::ever_public_sql("i", "$3"),
        kept_days = crate::storage::maintenance::PAGE_HISTORY_DAYS,
    ))
    .bind(&pages)
    .bind(&targets)
    .bind(org.0)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("remove page rows"))?;
    for (_, target, share) in &rows {
        if let Some(share) = share {
            record_audit_tx(
                tx,
                org,
                actor,
                "monitor_share.revoked",
                serde_json::json!({ "share_id": share, "target_id": target }),
            )
            .await?;
        }
    }
    Ok(rows.into_iter().map(|(p, t, _)| (p, t)).collect())
}

/// The page rows of every section the group has, for a group delete.
pub(crate) async fn group_rows(
    tx: &mut Tx<'_>,
    org: OrgId,
    group: Uuid,
) -> Result<Vec<(Uuid, Uuid)>> {
    sqlx::query_as(
        "SELECT spc.status_page_id, spc.target_id FROM status_page_sections s \
         JOIN targets t ON t.group_id = s.group_id AND t.org_id = s.org_id \
         JOIN status_page_components spc \
           ON spc.status_page_id = s.status_page_id AND spc.target_id = t.id \
         WHERE s.org_id = $1 AND s.group_id = $2 ORDER BY 1, 2",
    )
    .bind(org.0)
    .bind(group)
    .fetch_all(&mut **tx)
    .await
    .map_err(db("group page rows"))
}

#[derive(sqlx::FromRow)]
struct SectionRow {
    id: Uuid,
    group_id: Uuid,
    group_name: String,
    heading: Option<String>,
    members: Vec<Uuid>,
    group_size: i64,
    created_at: DateTime<Utc>,
}

impl From<SectionRow> for PageSection {
    fn from(r: SectionRow) -> Self {
        let on_page = r.members.len() as i64;
        Self {
            id: r.id,
            group_id: r.group_id,
            group_name: r.group_name,
            heading: r.heading,
            left_off: u32::try_from((r.group_size - on_page).max(0)).unwrap_or(u32::MAX),
            members: r.members,
            created_at: r.created_at,
        }
    }
}

/// Every section on `page`, oldest first.
pub async fn list(pool: &PgPool, org: OrgId, page: Uuid) -> Result<Vec<PageSection>> {
    let rows: Vec<SectionRow> = sqlx::query_as(
        "SELECT s.id, s.group_id, g.name AS group_name, s.heading, s.created_at, \
                COALESCE(array_agg(spc.target_id ORDER BY spc.sort_order, t.name) \
                         FILTER (WHERE spc.target_id IS NOT NULL), '{}') AS members, \
                count(t.id) AS group_size \
         FROM status_page_sections s \
         JOIN monitor_groups g ON g.id = s.group_id AND g.org_id = s.org_id \
         LEFT JOIN targets t ON t.group_id = s.group_id AND t.org_id = s.org_id \
         LEFT JOIN status_page_components spc \
           ON spc.status_page_id = s.status_page_id AND spc.target_id = t.id \
         WHERE s.org_id = $1 AND s.status_page_id = $2 \
         GROUP BY s.id, g.name ORDER BY s.created_at, s.id",
    )
    .bind(org.0)
    .bind(page)
    .fetch_all(pool)
    .await
    .map_err(db("list sections"))?;
    Ok(rows.into_iter().map(PageSection::from).collect())
}

pub fn section_not_found() -> AppError {
    AppError::not_found(codes::SECTION_NOT_FOUND, "section not found")
}

/// Adds `group` to `page` as a section: every member gets a row, a member
/// already there by hand moves into it with its settings, and members past
/// `cap` are left off.
pub async fn add(
    pool: &PgPool,
    org: OrgId,
    page: Uuid,
    group: Uuid,
    heading: Option<String>,
    cap: i64,
    actor: Option<UserId>,
) -> Result<(Uuid, PageRowChange)> {
    let mut tx = pool.begin().await.map_err(db("add section: begin"))?;
    let account = accounts::account_for_org(&mut *tx, org).await?;
    advisory_xact_lock(&mut *tx, &account_lock_key(account))
        .await
        .map_err(db("add section: account lock"))?;
    let page_ok: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM status_pages WHERE id = $1 AND org_id = $2")
            .bind(page)
            .bind(org.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db("add section: page"))?;
    if page_ok.is_none() {
        return Err(AppError::not_found(
            codes::STATUS_PAGE_NOT_FOUND,
            "status page not found",
        ));
    }
    if !lock_group(&mut tx, org, group).await? {
        return Err(AppError::not_found(
            codes::GROUP_NOT_FOUND,
            "group not found",
        ));
    }
    let id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO status_page_sections \
             (org_id, status_page_id, group_id, heading, created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $5) \
         ON CONFLICT (status_page_id, group_id) DO NOTHING RETURNING id",
    )
    .bind(org.0)
    .bind(page)
    .bind(group)
    .bind(&heading)
    .bind(actor.map(|u| u.0))
    .fetch_optional(&mut *tx)
    .await
    .map_err(db("add section"))?;
    let Some(id) = id else {
        return Err(AppError::conflict(
            codes::SECTION_EXISTS,
            "this group already has a section on the page",
        ));
    };
    let members: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM targets WHERE org_id = $1 AND group_id = $2 ORDER BY name, id",
    )
    .bind(org.0)
    .bind(group)
    .fetch_all(&mut *tx)
    .await
    .map_err(db("add section: members"))?;
    let pairs: Vec<(Uuid, Uuid)> = members.iter().map(|m| (page, *m)).collect();
    let mut change = add_rows(&mut tx, org, &pairs, cap, actor).await?;
    change.pages.insert(page);
    record_audit_tx(
        &mut tx,
        org,
        actor,
        "status_page.section_added",
        serde_json::json!({
            "page_id": page,
            "section_id": id,
            "group_id": group,
            "heading": heading,
            "left_off": change.left_off,
        }),
    )
    .await?;
    tx.commit().await.map_err(db("add section: commit"))?;
    Ok((id, change))
}

pub async fn update(
    pool: &PgPool,
    org: OrgId,
    page: Uuid,
    section: Uuid,
    heading: Option<String>,
    actor: Option<UserId>,
) -> Result<()> {
    let mut tx = pool.begin().await.map_err(db("update section: begin"))?;
    let changed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE status_page_sections SET heading = $4, updated_by = $5, updated_at = now() \
         WHERE id = $1 AND status_page_id = $2 AND org_id = $3 RETURNING id",
    )
    .bind(section)
    .bind(page)
    .bind(org.0)
    .bind(&heading)
    .bind(actor.map(|u| u.0))
    .fetch_optional(&mut *tx)
    .await
    .map_err(db("update section"))?;
    if changed.is_none() {
        return Err(section_not_found());
    }
    record_audit_tx(
        &mut tx,
        org,
        actor,
        "status_page.section_updated",
        serde_json::json!({ "page_id": page, "section_id": section, "heading": heading }),
    )
    .await?;
    tx.commit().await.map_err(db("update section: commit"))
}

/// Removes the section and its members' rows.
pub async fn remove(
    pool: &PgPool,
    org: OrgId,
    page: Uuid,
    section: Uuid,
    actor: Option<UserId>,
) -> Result<PageRowChange> {
    let mut tx = pool.begin().await.map_err(db("remove section: begin"))?;
    let account = accounts::account_for_org(&mut *tx, org).await?;
    advisory_xact_lock(&mut *tx, &account_lock_key(account))
        .await
        .map_err(db("remove section: account lock"))?;
    let group: Option<Uuid> = sqlx::query_scalar(
        "SELECT group_id FROM status_page_sections \
         WHERE id = $1 AND status_page_id = $2 AND org_id = $3",
    )
    .bind(section)
    .bind(page)
    .bind(org.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db("remove section: read"))?;
    let Some(group) = group else {
        return Err(section_not_found());
    };
    lock_group(&mut tx, org, group).await?;
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT spc.status_page_id, spc.target_id FROM status_page_components spc \
         JOIN targets t ON t.id = spc.target_id AND t.org_id = spc.org_id \
         WHERE spc.org_id = $1 AND spc.status_page_id = $2 AND t.group_id = $3",
    )
    .bind(org.0)
    .bind(page)
    .bind(group)
    .fetch_all(&mut *tx)
    .await
    .map_err(db("remove section: rows"))?;
    let mut change = remove_rows(&mut tx, org, &rows, actor).await?;
    change.pages.insert(page);
    sqlx::query("DELETE FROM status_page_sections WHERE id = $1 AND org_id = $2")
        .bind(section)
        .bind(org.0)
        .execute(&mut *tx)
        .await
        .map_err(db("remove section"))?;
    record_audit_tx(
        &mut tx,
        org,
        actor,
        "status_page.section_removed",
        serde_json::json!({
            "page_id": page,
            "section_id": section,
            "group_id": group,
            "removed": rows.len(),
        }),
    )
    .await?;
    tx.commit().await.map_err(db("remove section: commit"))?;
    Ok(change)
}

/// How many pages show each group, by group name, for the monitor form's
/// warning that joining a group publishes the monitor.
pub async fn pages_per_group(
    pool: &PgPool,
    org: OrgId,
) -> Result<std::collections::HashMap<String, i64>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT g.name, count(*) FROM status_page_sections s \
         JOIN monitor_groups g ON g.id = s.group_id AND g.org_id = s.org_id \
         WHERE s.org_id = $1 GROUP BY g.name",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .map_err(db("pages per group"))?;
    Ok(rows.into_iter().collect())
}

/// The pages that show `group` as a section, for the cache clear after a
/// group change their heading or rows read.
pub async fn pages_for_group(pool: &PgPool, org: OrgId, group: Uuid) -> Result<Vec<Uuid>> {
    sqlx::query_scalar(
        "SELECT status_page_id FROM status_page_sections WHERE org_id = $1 AND group_id = $2",
    )
    .bind(org.0)
    .bind(group)
    .fetch_all(pool)
    .await
    .map_err(db("pages for group"))
}
