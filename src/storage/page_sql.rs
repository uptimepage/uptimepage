//! SQL fragments for a query that aliases a status page `sp`, and the
//! [`PageRow`] they select.

use uuid::Uuid;

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

/// Where a page lives, as `page_row_columns` reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PageRow {
    pub page_id: Uuid,
    pub org_id: Uuid,
    pub page_name: String,
    pub public_locale: String,
    pub slug: String,
    pub custom_domain: Option<String>,
    pub custom_domain_published: bool,
    /// The page's stored badge preference; `None` follows the deploy default.
    pub show_powered_by: Option<bool>,
    pub white_label: bool,
}

/// Each [`PageRow`] field: the expression that reads it, and its name.
const PAGE_ROW: [(&str, &str); 9] = [
    ("sp.id", "page_id"),
    ("sp.org_id", "org_id"),
    (PAGE_PUBLIC_NAME, "page_name"),
    ("sp.public_locale", "public_locale"),
    ("sp.slug::text", "slug"),
    ("sp.custom_domain::text", "custom_domain"),
    (PAGE_CUSTOM_DOMAIN_PUBLISHED, "custom_domain_published"),
    ("sp.public_show_powered_by", "show_powered_by"),
    ("sp_plan.white_label_enabled", "white_label"),
];

/// The columns of a [`PageRow`], for a query that aliases the page `sp` and
/// joins [`PAGE_PLAN_JOIN`].
pub(crate) fn page_row_columns() -> String {
    PAGE_ROW
        .iter()
        .map(|(expr, name)| format!("{expr} AS {name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The names [`page_row_columns`] gives its columns, for a query that reads
/// them back out of a subquery.
pub(crate) fn page_row_names() -> String {
    PAGE_ROW.map(|(_, name)| name).join(", ")
}
