//! SQL fragments for a query that aliases a status page `sp`.

/// A literal, so `page_served!` can build on it with `concat!`.
macro_rules! page_not_held {
    () => {
        "sp.plan_hold_at IS NULL"
    };
}

/// A literal, so [`PAGE_CUSTOM_DOMAIN_PUBLISHED`] can build on it with `concat!`.
macro_rules! page_served {
    () => {
        concat!(
            "(sp.enabled AND ",
            page_not_held!(),
            " AND sp_org.deleted_at IS NULL)"
        )
    };
}

/// The plan a page's account is on, for a query that aliases the page `sp`.
pub(crate) const PAGE_PLAN_JOIN: &str = "JOIN organizations sp_org ON sp_org.id = sp.org_id \
     JOIN accounts sp_acct ON sp_acct.id = sp_org.account_id \
     JOIN plans sp_plan ON sp_plan.id = sp_acct.plan_id";

/// A page's public name as [`crate::domain::page_display_name`] resolves it,
/// for a query that aliases the page `sp` and joins [`PAGE_PLAN_JOIN`].
pub(crate) const PAGE_PUBLIC_NAME: &str =
    "COALESCE(NULLIF(btrim(sp.public_display_name), ''), sp_org.name)";

/// Verified plus a plan that sells one. Permits routing and certificate
/// issuance; it does not move a published URL.
pub(crate) const PAGE_CUSTOM_DOMAIN_SERVED: &str =
    "(sp.custom_domain_verified_at IS NOT NULL AND sp_plan.custom_domain_enabled)";

/// Activated, for the addresses a page publishes: subscriber mail, the feed
/// self link, `og:url`, the canonical tag. A host that has completed no TLS
/// handshake would hand every one of those a connection failure, and mail
/// cannot be recalled.
///
/// Carries [`PAGE_SERVED`] itself because callers differ: `subscribe.rs` reads
/// a page by id and filters nothing.
pub(crate) const PAGE_CUSTOM_DOMAIN_PUBLISHED: &str = concat!(
    "(sp.custom_domain_activated_at IS NOT NULL AND sp_plan.custom_domain_enabled AND ",
    page_served!(),
    ")"
);

/// Whether the plan still covers this page. Over cap, the excess pages carry a
/// `plan_hold_at`: they 404 publicly and their subscribers hear nothing, while
/// the console keeps them whole so the operator can see what is waiting and
/// choose differently. For a query that aliases the page `sp`.
pub(crate) const PAGE_NOT_HELD: &str = page_not_held!();

/// Served at its URL: on, not held, its org not deleted. Anything else 404s,
/// so its subscribers hear nothing either. For a query that aliases the page
/// `sp` and joins [`PAGE_PLAN_JOIN`].
pub(crate) const PAGE_SERVED: &str = page_served!();

/// The columns of a [`crate::public_status::subscriber_notice::PageRow`], for
/// a query that aliases the page `sp` and joins [`PAGE_PLAN_JOIN`].
pub(crate) fn page_row_columns() -> String {
    format!(
        "sp.id AS page_id, sp.org_id, {PAGE_PUBLIC_NAME} AS page_name, sp.public_locale, \
         sp.slug::text AS slug, sp.custom_domain::text AS custom_domain, \
         {PAGE_CUSTOM_DOMAIN_PUBLISHED} AS custom_domain_published"
    )
}
