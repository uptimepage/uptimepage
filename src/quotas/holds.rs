//! What an account keeps when its plan no longer covers everything it has.
//!
//! A plan can shrink under an account that is already using more than the new
//! tier sells. Deleting the excess would destroy work the customer paid for
//! once and may pay for again, so the excess is *held* instead: the row stays
//! exactly as it is and stops being served. Restoring the plan releases it
//! with nothing to repair, which is the same promise the read-time interval
//! and region clamps make.
//!
//! Two rules shape everything here:
//!
//! - **A hold is not a delete and not a pause.** `enabled` is the customer's
//!   own switch and is never touched, so a monitor they had paused comes back
//!   paused.
//! - **A held row still counts against the cap.** It occupies the slot it is
//!   waiting for; if it did not, an account could hold twenty monitors and
//!   then create twenty more on top of them.

use std::sync::Arc;

use anyhow::Context;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{AccountId, OrgId, Plan};
use crate::error::{AppError, Result};
use crate::storage::accounts::live_orgs;
use crate::storage::locks::{account_lock_key, advisory_xact_lock};
use crate::storage::orgs::record_audit_tx;

/// What one reconcile changed. Both counts are zero on the common path, where
/// the account fits its plan and the statement matches nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub held: usize,
    pub released: usize,
}

impl Reconciled {
    pub fn changed(self) -> bool {
        self.held > 0 || self.released > 0
    }
}

/// One row the reconcile moved, carried out of the statement so the audit
/// trail can name what stopped being watched.
#[derive(sqlx::FromRow)]
struct Moved {
    id: Uuid,
    org_id: Uuid,
    name: String,
    held: bool,
}

/// Cap on named rows in one audit entry: past this the count answers "what did
/// they lose" as well as a full list would. Matches the pause audit's bound.
const AUDIT_SAMPLE: usize = 100;

/// Where a reconcile gets the plan it judges against: resolved once the
/// account lock is held, or handed in by a test shaping a cap without an
/// override row.
pub enum PlanSource<'a> {
    Fixed(Arc<Plan>),
    Resolve(&'a crate::quotas::QuotaService, OrgId),
}

/// Brings one account's holds in line with its plan.
///
/// Idempotent, and deliberately not incremental: each statement recomputes the
/// whole held set from the current plan rather than diffing against what is
/// already held. Converging in one shot is what makes a repeat run a no-op and
/// makes release fall out of the same code as hold, with no second path to
/// keep in step.
///
/// The customer's own pick reaches this through the stored `plan_keep` flag
/// rather than an argument, which is what makes it survive: recomputing from
/// scratch means a pick held only in the request that set it would be undone
/// by the next sweep, putting back on hold the very row they chose to keep.
/// Set it with [`set_keep`].
///
/// A pick is authoritative in both directions: what it names is kept, and what
/// it leaves out is held even when a seat is free. Refilling a free seat from
/// the rows the customer just gave up would make un-picking a monitor do
/// literally nothing, since the row that lost the seat is the one that ranks
/// first to reclaim it. Keeping fewer than the plan sells is theirs to choose.
///
/// Failing a pick, rows rank live-before-paused and then oldest-first: a
/// monitor that is switched off is the cheapest one to hold, and the oldest
/// are what the account was built around.
///
/// The pick only binds while a cap is actually exceeded. Once the plan covers
/// everything the whole held set clears, including rows the pick left out —
/// a hold is the plan's mechanism, and a customer who wants a monitor quiet
/// inside their plan has `enabled` for that. The pick itself is dropped at the
/// same moment, so it cannot arm a later shortage it was never asked about.
///
/// The plan is resolved under the lock, not handed in: reconciles serialise
/// on the account and the last one decides the final state, so one carrying a
/// plan resolved before it queued could hold monitors against a plan the
/// account has already left. It is read on the transaction's own connection,
/// since holding one pooled connection while waiting for a second is how a
/// few concurrent reconciles would exhaust the pool.
pub async fn reconcile_account(
    pool: &PgPool,
    account: AccountId,
    plan: PlanSource<'_>,
    actor: Option<crate::domain::UserId>,
) -> Result<Reconciled> {
    let mut tx = pool.begin().await.context("reconcile_account: begin")?;
    // The same lock every pooled create takes. Without it a create racing a
    // reconcile can slip a monitor in against a count the reconcile has
    // already read, leaving the account one over its cap until the next sweep.
    advisory_xact_lock(&mut *tx, &account_lock_key(account))
        .await
        .context("reconcile_account: lock")?;
    let plan = match plan {
        PlanSource::Fixed(plan) => plan,
        PlanSource::Resolve(quotas, org) => quotas.limit_for_org_on(&mut tx, org).await?,
    };
    let plan = &*plan;

    let targets = reconcile_targets(&mut tx, account, plan).await?;
    let pages = reconcile_status_pages(&mut tx, account, plan).await?;
    forget_spent_pick(&mut tx, account, plan).await?;

    let mut out = Reconciled::default();
    for (moved, held_action, release_action) in [
        (targets, "target.plan_hold", "target.plan_release"),
        (pages, "status_page.plan_hold", "status_page.plan_release"),
    ] {
        for row in &moved {
            if row.held {
                out.held += 1;
            } else {
                out.released += 1;
            }
        }
        audit_moves(&mut tx, moved, held_action, release_action, actor).await?;
    }

    tx.commit().await.context("reconcile_account: commit")?;
    Ok(out)
}

/// Monitors, under two caps at once. A flow monitor spends both a flow slot
/// and a monitor slot, so flows are ranked first and the monitors a flow cap
/// already held are out of the running for the monitor cap. Doing it the other
/// way would hold an ordinary monitor to make room for a flow the flow cap is
/// about to hold anyway.
async fn reconcile_targets(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
    plan: &Plan,
) -> Result<Vec<Moved>> {
    let sql = format!(
        r#"WITH pool AS (
               SELECT id, org_id, name, created_at, enabled,
                      kind = 'flow' AS is_flow,
                      plan_keep AS kept
               FROM targets
               WHERE org_id IN ({orgs})
           ),
           pick AS (
               SELECT coalesce(bool_or(kept), false) AS picked,
                      count(*) > $3 AS over_targets,
                      count(*) FILTER (WHERE is_flow) > $2 AS over_flows
               FROM pool
           ),
           flow_ranked AS (
               SELECT *,
                      CASE WHEN is_flow THEN row_number() OVER (
                          PARTITION BY is_flow
                          ORDER BY kept DESC, enabled DESC, created_at ASC, id ASC
                      ) END AS flow_rank
               FROM pool
           ),
           after_flow AS (
               SELECT *, coalesce(flow_rank > $2, false) AS flow_held FROM flow_ranked
           ),
           ranked AS (
               SELECT id, org_id, name, flow_held, kept, is_flow,
                      row_number() OVER (
                          PARTITION BY flow_held
                          ORDER BY kept DESC, enabled DESC, created_at ASC, id ASC
                      ) AS rank
               FROM after_flow
           ),
           want AS (
               SELECT r.id, r.org_id, r.name,
                      (r.flow_held OR r.rank > $3
                       OR (p.picked AND NOT r.kept
                           AND (p.over_targets OR (r.is_flow AND p.over_flows)))) AS hold
               FROM ranked r CROSS JOIN pick p
           )
           UPDATE targets t
           SET plan_hold_at = CASE WHEN w.hold THEN now() ELSE NULL END,
               updated_at   = now()
           FROM want w
           WHERE t.id = w.id AND w.hold <> (t.plan_hold_at IS NOT NULL)
           RETURNING t.id, t.org_id, w.name, w.hold AS held"#,
        orgs = live_orgs("$1"),
    );
    sqlx::query_as(&sql)
        .bind(account.0)
        .bind(i64::from(plan.max_flow_checks))
        .bind(i64::from(plan.max_targets))
        .fetch_all(&mut **tx)
        .await
        .context("reconcile_account: targets")
        .map_err(AppError::from)
}

async fn reconcile_status_pages(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
    plan: &Plan,
) -> Result<Vec<Moved>> {
    let sql = format!(
        r#"WITH pool AS (
               SELECT id, org_id, name, created_at, enabled, plan_keep AS kept
               FROM status_pages
               WHERE org_id IN ({orgs})
           ),
           pick AS (
               SELECT coalesce(bool_or(kept), false) AS picked,
                      count(*) > $2 AS over_cap
               FROM pool
           ),
           ranked AS (
               SELECT id, org_id, name, kept,
                      row_number() OVER (
                          ORDER BY kept DESC, enabled DESC, created_at ASC, id ASC
                      ) AS rank
               FROM pool
           ),
           want AS (
               SELECT r.id, r.org_id, r.name,
                      (r.rank > $2 OR (p.picked AND p.over_cap AND NOT r.kept)) AS hold
               FROM ranked r CROSS JOIN pick p
           )
           UPDATE status_pages sp
           SET plan_hold_at = CASE WHEN w.hold THEN now() ELSE NULL END,
               updated_at   = now()
           FROM want w
           WHERE sp.id = w.id AND w.hold <> (sp.plan_hold_at IS NOT NULL)
           RETURNING sp.id, sp.org_id, w.name, w.hold AS held"#,
        orgs = live_orgs("$1"),
    );
    sqlx::query_as(&sql)
        .bind(account.0)
        .bind(i64::from(plan.max_status_pages))
        .fetch_all(&mut **tx)
        .await
        .context("reconcile_account: status pages")
        .map_err(AppError::from)
}

/// Drops a pick once its plan covers the whole pool again.
///
/// A pick answers one question — which rows lose their slots — and that
/// question only exists while a cap is exceeded. Left behind, it silently arms
/// the next shortage: an account that picked forty monitors a year ago would
/// have every monitor added since count as "not chosen" the moment any cap
/// slipped, holding rows the customer never declined. Scoped per table, so a
/// monitor shortage does not clear a status page pick.
async fn forget_spent_pick(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account: AccountId,
    plan: &Plan,
) -> Result<()> {
    let orgs = live_orgs("$1");
    let targets = format!(
        "UPDATE targets SET plan_keep = false \
         WHERE org_id IN ({orgs}) AND plan_keep \
           AND NOT EXISTS ( \
               SELECT 1 FROM ( \
                   SELECT count(*) AS n, \
                          count(*) FILTER (WHERE kind = 'flow') AS flows \
                   FROM targets WHERE org_id IN ({orgs}) \
               ) c WHERE c.n > $3 OR c.flows > $2)"
    );
    sqlx::query(&targets)
        .bind(account.0)
        .bind(i64::from(plan.max_flow_checks))
        .bind(i64::from(plan.max_targets))
        .execute(&mut **tx)
        .await
        .context("reconcile_account: forget target pick")?;

    let pages = format!(
        "UPDATE status_pages SET plan_keep = false \
         WHERE org_id IN ({orgs}) AND plan_keep \
           AND (SELECT count(*) FROM status_pages WHERE org_id IN ({orgs})) <= $2"
    );
    sqlx::query(&pages)
        .bind(account.0)
        .bind(i64::from(plan.max_status_pages))
        .execute(&mut **tx)
        .await
        .context("reconcile_account: forget page pick")?;
    Ok(())
}

/// One audit row per org per direction. An account's orgs are audited
/// separately because `org_audit_log` is org-scoped, and whoever reads one
/// org's trail should see what left that org, not the whole account's total.
async fn audit_moves(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    moved: Vec<Moved>,
    held_action: &str,
    release_action: &str,
    actor: Option<crate::domain::UserId>,
) -> Result<()> {
    let mut by_org: std::collections::BTreeMap<(Uuid, bool), Vec<Moved>> = Default::default();
    for row in moved {
        by_org.entry((row.org_id, row.held)).or_default().push(row);
    }
    for ((org, held), rows) in by_org {
        let sample: Vec<_> = rows
            .iter()
            .take(AUDIT_SAMPLE)
            .map(|r| json!({ "id": r.id, "name": r.name }))
            .collect();
        let action = if held { held_action } else { release_action };
        record_audit_tx(
            tx,
            OrgId(org),
            actor,
            action,
            json!({
                "count": rows.len(),
                "items": sample,
                "truncated": rows.len() > AUDIT_SAMPLE,
            }),
        )
        .await
        .context("reconcile_account: audit")?;
    }
    Ok(())
}

/// Accounts whose holds may be out of step with their plan, each paired with
/// one of its live orgs so the caller can resolve the plan through the same
/// cached path every request uses (overrides and add-ons included, since both
/// are per-account and any of its orgs resolves the identical plan).
///
/// Two ways in: the account already holds something, which the partial indexes
/// answer for nothing on an install that holds nothing, or it is over one of
/// the three caps a hold can act on. Anything else has no work to do, and
/// reconciling it would take a lock and write an audit row for a no-op.
pub async fn accounts_needing_reconcile(pool: &PgPool) -> Result<Vec<(AccountId, OrgId)>> {
    // Built here rather than inline so the three caps cannot drift apart.
    //
    // CASE, not `guard AND cast`. Postgres does not promise to evaluate the
    // arms of an AND left to right, so a guard beside the cast can be reordered
    // behind it and the malformed value reaches the cast anyway. Only CASE
    // orders the test before the branch. A test can pass on this either way,
    // since whether it breaks depends on the plan the planner happens to pick.
    let lowered = ["max_targets", "max_flow_checks", "max_status_pages"]
        .map(|f| {
            format!(
                "CASE WHEN jsonb_typeof(po.override_json->'{f}') = 'number' \
                      THEN (po.override_json->>'{f}')::numeric < p.{f} \
                      ELSE false END"
            )
        })
        .join(" OR ");
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(&format!(
        r#"/* SAFE: the sweep is account-wide by definition; it reconciles every
              tenant's holds against its own plan and returns no tenant data */
           WITH live AS (
               SELECT id, account_id FROM organizations WHERE deleted_at IS NULL
           ),
           acct AS (
               SELECT account_id, min(id::text)::uuid AS any_org
               FROM live GROUP BY account_id
           )
           SELECT a.id, acct.any_org
           FROM acct
           JOIN accounts a ON a.id = acct.account_id
           JOIN plans p ON p.id = a.plan_id
           -- Counted in separate laterals on purpose: joining targets and
           -- status pages in one pass multiplies each by the other's row
           -- count, which reads as "over cap" for any account with two pages.
           CROSS JOIN LATERAL (
               SELECT count(*) AS n,
                      count(*) FILTER (WHERE t.kind = 'flow') AS flows,
                      bool_or(t.plan_hold_at IS NOT NULL) AS holds
               FROM targets t
               JOIN live lt ON lt.id = t.org_id AND lt.account_id = a.id
           ) tc
           CROSS JOIN LATERAL (
               SELECT count(*) AS n, bool_or(sp.plan_hold_at IS NOT NULL) AS holds
               FROM status_pages sp
               JOIN live lp ON lp.id = sp.org_id AND lp.account_id = a.id
           ) pc
           WHERE coalesce(tc.holds, false) OR coalesce(pc.holds, false)
              OR tc.n > p.max_targets
              OR tc.flows > p.max_flow_checks
              OR pc.n > p.max_status_pages
              -- An override *replaces* a named cap, so it can put the effective
              -- ceiling below the plans row this query reads, and the reconcile
              -- that follows resolves the effective plan. Comparing only
              -- against the raw row would skip exactly those accounts.
              --
              -- Only a lowering override matters. Admitting every overridden
              -- account instead would sweep the whole fleet, since an override
              -- is also how capacity is granted. Add-ons need no test at all:
              -- they only ever add, so they can raise an account out of scope
              -- but never hide one that belongs in it.
              --
              -- Rows written before the override endpoint existed are
              -- hand-typed JSON nothing validated, and a bad cast aborts the
              -- whole statement, not one row: a single mistyped override would
              -- stop holds *and releases* for every account until someone
              -- found it. Hence the CASE built
              -- above, and numeric rather than int so a value too large is out
              -- of scope rather than an error.
              OR EXISTS (
                  SELECT 1 FROM plan_overrides po
                  WHERE po.account_id = a.id
                    AND (po.expires_at IS NULL OR po.expires_at > now())
                    AND ({lowered})
              )"#
    ))
    .fetch_all(pool)
    .await
    .context("accounts_needing_reconcile")?;
    Ok(rows
        .into_iter()
        .map(|(a, o)| (AccountId(a), OrgId(o)))
        .collect())
}

/// One sweep over every account whose holds may have drifted from its plan.
///
/// A plan change reconciles the account itself, so this is the backstop for
/// whatever bypasses that path: a plan moved by hand, an override that
/// expired on its own, a reconcile that failed after the change committed. A
/// downgrade lands within a day whatever route it arrived by.
///
/// One account failing does not stop the others: a plan that will not resolve
/// leaves that account exactly as it is and logs, in the same spirit as the
/// read-time clamp's infallible `govern`.
pub async fn sweep(pool: &PgPool, quotas: &crate::quotas::QuotaService) -> Result<u64> {
    let mut moved = 0u64;
    for (account, org) in accounts_needing_reconcile(pool).await? {
        match reconcile_account(pool, account, PlanSource::Resolve(quotas, org), None).await {
            Ok(r) => {
                if r.changed() {
                    tracing::info!(
                        account = %account.0,
                        held = r.held, released = r.released,
                        "plan holds reconciled"
                    );
                    moved += (r.held + r.released) as u64;
                }
            }
            Err(err) => {
                tracing::warn!(account = %account.0, error = %err, "plan holds: reconcile failed")
            }
        }
    }
    Ok(moved)
}

/// One row a plan is holding, for the customer to look at before choosing
/// differently.
#[derive(sqlx::FromRow)]
pub struct Held {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub held_at: chrono::DateTime<chrono::Utc>,
}

/// Everything one account currently has held, monitors then status pages,
/// oldest hold first so the list reads in the order the plan gave them up.
pub async fn list_held(pool: &PgPool, account: AccountId) -> Result<(Vec<Held>, Vec<Held>)> {
    let orgs = live_orgs("$1");
    let targets = sqlx::query_as(&format!(
        "SELECT id, org_id, name, plan_hold_at AS held_at FROM targets \
         WHERE org_id IN ({orgs}) AND plan_hold_at IS NOT NULL \
         ORDER BY plan_hold_at ASC, created_at ASC, id ASC"
    ))
    .bind(account.0)
    .fetch_all(pool)
    .await
    .context("list_held: targets")?;
    let pages = sqlx::query_as(&format!(
        "SELECT id, org_id, name, plan_hold_at AS held_at FROM status_pages \
         WHERE org_id IN ({orgs}) AND plan_hold_at IS NOT NULL \
         ORDER BY plan_hold_at ASC, created_at ASC, id ASC"
    ))
    .bind(account.0)
    .fetch_all(pool)
    .await
    .context("list_held: status pages")?;
    Ok((targets, pages))
}

/// Records which of the account's rows the customer wants kept, replacing any
/// previous answer. Ids that are not the account's own are ignored rather than
/// rejected, so a stale page listing a since-deleted monitor still saves.
///
/// One list per resource, and `None` leaves that resource's answer alone. A
/// single pooled list could not say the difference between "keep no status
/// page" and "this caller was not asked about status pages", so a picker shown
/// only the monitors would silently wipe the page pick the customer made
/// during their last shortage.
///
/// Stored, not applied: the caller reconciles afterwards, and every later run
/// reads the same flag, which is what stops the daily sweep undoing the choice.
pub async fn set_keep(
    pool: &PgPool,
    account: AccountId,
    targets: Option<&[Uuid]>,
    status_pages: Option<&[Uuid]>,
) -> Result<()> {
    let orgs = live_orgs("$1");
    for (table, keep) in [("targets", targets), ("status_pages", status_pages)] {
        let Some(keep) = keep else { continue };
        sqlx::query(&format!(
            "UPDATE {table} SET plan_keep = (id = ANY($2)) \
             WHERE org_id IN ({orgs}) AND plan_keep <> (id = ANY($2))"
        ))
        .bind(account.0)
        .bind(keep)
        .execute(pool)
        .await
        .context("set_keep")?;
    }
    Ok(())
}

/// Whether this account is holding anything at all. Answered from the partial
/// indexes, so the overwhelmingly common "nothing held" case costs an index
/// probe rather than a scan.
pub async fn holds_anything(pool: &PgPool, account: AccountId) -> Result<bool> {
    let orgs = live_orgs("$1");
    let (any,): (bool,) = sqlx::query_as(&format!(
        "SELECT EXISTS (SELECT 1 FROM targets \
                        WHERE org_id IN ({orgs}) AND plan_hold_at IS NOT NULL) \
             OR EXISTS (SELECT 1 FROM status_pages \
                        WHERE org_id IN ({orgs}) AND plan_hold_at IS NOT NULL)"
    ))
    .bind(account.0)
    .fetch_one(pool)
    .await
    .context("holds_anything")?;
    Ok(any)
}

/// Gives back what a freed slot can now cover, right after a delete.
///
/// The daily sweep would find this anyway, but a customer who deletes a monitor
/// precisely to get another one running should not wait a day to see it. Does
/// nothing, and takes no lock, for an account holding nothing.
///
/// A freed slot releases something only while the default order is in charge.
/// Once the customer has picked, every held row is one they declined, so a
/// delete leaves the slot empty and the picker is where they take it back —
/// which is the point of a pick that binds in both directions.
///
/// Failure is logged and swallowed: the delete the caller just made is done and
/// must not be reported as failed because a release could not be computed, and
/// the sweep is the backstop.
pub async fn release_after_delete(pool: &PgPool, quotas: &crate::quotas::QuotaService, org: OrgId) {
    let done = async {
        let account = crate::storage::accounts::account_for_org(pool, org).await?;
        if !holds_anything(pool, account).await? {
            return Ok(Reconciled::default());
        }
        reconcile_account(pool, account, PlanSource::Resolve(quotas, org), None).await
    }
    .await;
    match done {
        Ok(r) if r.changed() => {
            tracing::info!(org = %org.0, released = r.released, "plan holds released after delete")
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(org = %org.0, error = %err, "plan holds: release after delete"),
    }
}

/// The step every entitlement change ends with. Cache first, so the reconcile
/// judges against the plan just written rather than a stale entry. An account
/// with no live org has nothing to reconcile; its cache is still cleared so a
/// later restore does not resurrect the old plan.
pub async fn reconcile_after_change(
    pool: &PgPool,
    quotas: &crate::quotas::QuotaService,
    account: AccountId,
    actor: Option<crate::domain::UserId>,
) -> Result<Reconciled> {
    quotas.invalidate_account(account).await?;
    let Some(org) = crate::storage::accounts::first_live_org(pool, account).await? else {
        return Ok(Reconciled::default());
    };
    reconcile_account(pool, account, PlanSource::Resolve(quotas, org), actor).await
}

/// One row in the account's pool, for the picker: everything the caps apply
/// to, whether or not it is currently held.
#[derive(sqlx::FromRow)]
pub struct PoolRow {
    pub id: Uuid,
    pub name: String,
    pub held: bool,
    /// What the customer last chose, not what is running. The two differ
    /// whenever a pick could not be honoured in full, and a picker that showed
    /// the running set instead would quietly drop those rows on the next save.
    pub kept: bool,
    /// Spends a flow slot as well as a monitor slot, so it answers to a second
    /// cap the picker has to show separately. Always false for status pages.
    pub is_flow: bool,
}

/// How many rows the picker will render. Past this the panel stops offering a
/// choice rather than emitting a checkbox per row: a truncated list cannot
/// express a pick, since every row it omits would read as declined.
pub const MAX_PICKER_ROWS: usize = 500;

/// The account's monitors and status pages, held rows first and the rest in
/// the order the reconcile would give them up. Reads one row past
/// [`MAX_PICKER_ROWS`] so a caller can see that it has been cut.
pub async fn list_pool(pool: &PgPool, account: AccountId) -> Result<(Vec<PoolRow>, Vec<PoolRow>)> {
    let orgs = live_orgs("$1");
    // Held first. The rank order below is what decides who keeps a slot, but
    // the picker is read to answer "what did I lose", and burying those rows
    // under twenty live ones puts the answer off-screen.
    let order = "ORDER BY plan_hold_at IS NOT NULL DESC, \
                 plan_keep DESC, enabled DESC, created_at ASC, id ASC";
    // One past the ceiling, so the caller can tell a full list from a cut one
    // without a second count.
    let over_limit = MAX_PICKER_ROWS as i64 + 1;
    let targets = sqlx::query_as(&format!(
        "SELECT id, name, plan_hold_at IS NOT NULL AS held, plan_keep AS kept, \
                kind = 'flow' AS is_flow \
         FROM targets WHERE org_id IN ({orgs}) {order} LIMIT $2"
    ))
    .bind(account.0)
    .bind(over_limit)
    .fetch_all(pool)
    .await
    .context("list_pool: targets")?;
    let pages = sqlx::query_as(&format!(
        "SELECT id, name, plan_hold_at IS NOT NULL AS held, plan_keep AS kept, \
                false AS is_flow \
         FROM status_pages WHERE org_id IN ({orgs}) {order} LIMIT $2"
    ))
    .bind(account.0)
    .bind(over_limit)
    .fetch_all(pool)
    .await
    .context("list_pool: status pages")?;
    Ok((targets, pages))
}
