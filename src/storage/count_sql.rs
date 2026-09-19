//! The account-pooled count queries: `$1` is the account, and each counts
//! across its live orgs via [`super::accounts::live_orgs`]. Declared
//! once and shared by the atomic friendly-check path, the store-side race-safe
//! guards, *and* the usage snapshot, so the number a customer is blocked at
//! always equals the number the usage page shows (single source).

use super::accounts::live_orgs;

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
