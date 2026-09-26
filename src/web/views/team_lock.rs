//! What an org whose plan lacks on-call sees on the on-call and escalation
//! pages and the monitor form, in place of controls the API would refuse.

use crate::api::handlers::on_call::on_call_available;
use crate::app::AppState;
use crate::domain::{OrgId, UserId};
use crate::storage::accounts;
use crate::web::error::WebResult;

/// The org's plan does not include on-call.
pub struct TeamLock {
    /// The viewer pays for the account, so the switch is theirs to make.
    pub payer: bool,
    /// Nothing built that anyone still needs to reach: the pitch stands in
    /// for the page.
    pub teaser: bool,
}

/// Whether the plan leaves the org unable to add on-call coverage.
pub(crate) async fn plan_locked(state: &AppState, org: OrgId) -> WebResult<bool> {
    let plan = state.quotas.limit_for_org(org).await?;
    Ok(!on_call_available(state, &plan))
}

/// `None` when the org may build schedules and policies. `built` answers
/// whether anything the org set up still needs the working page; it runs
/// only when the plan is locked.
pub(crate) async fn team_lock(
    state: &AppState,
    org: OrgId,
    user: UserId,
    built: impl Future<Output = WebResult<bool>>,
) -> WebResult<Option<TeamLock>> {
    if !plan_locked(state, org).await? {
        return Ok(None);
    }
    let payer = match state.db.as_ref() {
        Some(pool) => accounts::pays_for_org(pool, user, org).await?,
        None => false,
    };
    Ok(Some(TeamLock {
        payer,
        teaser: !built.await?,
    }))
}

/// Locked with nothing built: the pitch stands in for the page.
pub(crate) fn shows_teaser(lock: &Option<TeamLock>) -> bool {
    lock.as_ref().is_some_and(|l| l.teaser)
}
